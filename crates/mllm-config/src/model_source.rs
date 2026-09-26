//! ADR 0008 (model sources): where a deployment's weights come from, and the
//! host policy that decides whether a remote source may be materialized.
//!
//! A deployment declares one source:
//!
//! - `local`: a path on the host (relative paths resolve against the store);
//! - `huggingface`: a repository pinned to a commit SHA, optionally narrowed by
//!   allow patterns, with an optional `secret://` token reference;
//! - `http`: an HTTPS URL pinned by SHA-256, optionally a tar archive.
//!
//! Both spellings are accepted on input: the internally tagged form
//! (`{type: huggingface, repo, revision}`) that deployments have used since the
//! native launch slice, and the externally tagged form
//! (`{huggingface: {repo, revision}}`). The canonical encoding (snapshots,
//! fingerprints) stays the tagged form, so existing revisions keep their exact
//! fingerprints.
//!
//! SPEC §1.2 as amended by ADR 0008: materializing a *declared* source is
//! explicit intent and permitted; nothing here downloads anything. A remote
//! source resolves to a fixed directory inside the host's model store
//! (`sources/...`), which the host fills through `MaterializeSource` before
//! the first placement. Engines keep reading a local path.

use crate::{ConfigError, ConfigErrorCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

fn invalid(path: impl Into<String>, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

/// The directory under the model store that holds materialized sources.
pub const SOURCES_DIR: &str = "sources";

/// The Hugging Face hub a source is fetched from unless the host names a mirror.
pub const DEFAULT_HUGGINGFACE_ENDPOINT: &str = "https://huggingface.co";

/// How an `http` source's payload is laid out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Archive {
    /// One file, stored under the URL's last path segment.
    #[default]
    None,
    /// A POSIX tar archive, verified as a whole and then extracted.
    Tar,
}

impl Archive {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Where a deployment's weights come from (SPEC §7, ADR 0008).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelSource {
    /// A path on the host. Relative paths resolve against the host's model store;
    /// an absolute path is taken as written.
    Local { path: String },
    /// A Hugging Face repository at one immutable commit.
    #[serde(rename = "huggingface")]
    HuggingFace {
        repo: String,
        /// The full 40-character commit SHA. Branches and tags move, so they
        /// are refused: the same declaration must always mean the same bytes.
        revision: String,
        /// Allow patterns (`*`, `?`) over repository paths; empty means every
        /// file of the commit.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
        /// `secret://<name>`: the host resolves it from its own secret store.
        /// The value is never part of any document, journal or log.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_ref: Option<String>,
    },
    /// A payload over HTTPS, pinned by content digest.
    Http {
        url: String,
        sha256: String,
        #[serde(default, skip_serializing_if = "Archive::is_none")]
        archive: Archive,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Tagged {
    Local {
        path: String,
    },
    #[serde(rename = "huggingface")]
    HuggingFace {
        repo: String,
        revision: String,
        #[serde(default)]
        files: Vec<String>,
        #[serde(default)]
        token_ref: Option<String>,
    },
    Http {
        url: String,
        sha256: String,
        #[serde(default)]
        archive: Archive,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalFields {
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HuggingFaceFields {
    repo: String,
    revision: String,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    token_ref: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpFields {
    url: String,
    sha256: String,
    #[serde(default)]
    archive: Archive,
}

impl From<Tagged> for ModelSource {
    fn from(tagged: Tagged) -> Self {
        match tagged {
            Tagged::Local { path } => Self::Local { path },
            Tagged::HuggingFace {
                repo,
                revision,
                files,
                token_ref,
            } => Self::HuggingFace {
                repo,
                revision,
                files,
                token_ref,
            },
            Tagged::Http {
                url,
                sha256,
                archive,
            } => Self::Http {
                url,
                sha256,
                archive,
            },
        }
    }
}

impl<'de> Deserialize<'de> for ModelSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| D::Error::custom("a model source is a mapping"))?;
        if object.contains_key("type") {
            return serde_json::from_value::<Tagged>(value)
                .map(Into::into)
                .map_err(D::Error::custom);
        }
        // ADR 0008: `{local: {...}} | {huggingface: {...}} | {http: {...}}`.
        let mut entries = object.iter();
        let (Some((kind, fields)), None) = (entries.next(), entries.next()) else {
            return Err(D::Error::custom(
                "a model source names exactly one of local, huggingface or http",
            ));
        };
        let fields = fields.clone();
        match kind.as_str() {
            "local" => serde_json::from_value::<LocalFields>(fields)
                .map(|f| Self::Local { path: f.path })
                .map_err(D::Error::custom),
            "huggingface" => serde_json::from_value::<HuggingFaceFields>(fields)
                .map(|f| Self::HuggingFace {
                    repo: f.repo,
                    revision: f.revision,
                    files: f.files,
                    token_ref: f.token_ref,
                })
                .map_err(D::Error::custom),
            "http" => serde_json::from_value::<HttpFields>(fields)
                .map(|f| Self::Http {
                    url: f.url,
                    sha256: f.sha256,
                    archive: f.archive,
                })
                .map_err(D::Error::custom),
            _ => Err(D::Error::unknown_variant(
                kind,
                &["local", "huggingface", "http"],
            )),
        }
    }
}

/// Which remote source kind a policy decision is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteKind {
    #[serde(rename = "huggingface")]
    HuggingFace,
    Http,
}

impl ModelSource {
    /// Whether the host must materialize this source before it can be used.
    pub fn is_remote(&self) -> bool {
        !matches!(self, Self::Local { .. })
    }

    pub fn remote_kind(&self) -> Option<RemoteKind> {
        match self {
            Self::Local { .. } => None,
            Self::HuggingFace { .. } => Some(RemoteKind::HuggingFace),
            Self::Http { .. } => Some(RemoteKind::Http),
        }
    }

    /// ADR 0008: the directory, relative to the model store, a remote source
    /// materializes into. Stable across hosts and revisions: the same pinned
    /// declaration always names the same directory, so a second deployment of
    /// it reuses the copy. `None` for a local source.
    pub fn store_key(&self) -> Option<String> {
        match self {
            Self::Local { .. } => None,
            Self::HuggingFace {
                repo,
                revision,
                files,
                ..
            } => {
                let mut key = format!(
                    "{SOURCES_DIR}/huggingface/{}@{revision}",
                    repo.replace('/', "--")
                );
                if !files.is_empty() {
                    // Different allow patterns select different files, so they
                    // are different directories.
                    let mut patterns = files.clone();
                    patterns.sort();
                    patterns.dedup();
                    let digest = Sha256::digest(patterns.join("\n").as_bytes());
                    key.push('-');
                    key.push_str(&hex::encode(&digest[..6]));
                }
                Some(key)
            }
            Self::Http {
                sha256, archive, ..
            } => Some(match archive {
                Archive::None => format!("{SOURCES_DIR}/http/{sha256}"),
                Archive::Tar => format!("{SOURCES_DIR}/http/{sha256}-tar"),
            }),
        }
    }

    /// SPEC §13.3, ADR 0008: validate a declared source's shape. Pinned
    /// revisions and digests only; HTTPS only; secret references only.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::Local { path } => {
                if path.is_empty() {
                    return Err(invalid("model.source.path", "must not be empty"));
                }
            }
            Self::HuggingFace {
                repo,
                revision,
                files,
                token_ref,
            } => {
                if !valid_repo(repo) {
                    return Err(invalid(
                        "model.source.repo",
                        "must be `name` or `owner/name` of letters, digits, '.', '_' or '-'",
                    ));
                }
                if !is_commit_sha(revision) {
                    return Err(invalid(
                        "model.source.revision",
                        "must be a full 40-character lowercase commit SHA; branches and tags move",
                    ));
                }
                if files.len() > 256 {
                    return Err(invalid("model.source.files", "at most 256 patterns"));
                }
                for pattern in files {
                    if !valid_pattern(pattern) {
                        return Err(invalid(
                            "model.source.files",
                            "patterns are relative repository paths without `..`",
                        ));
                    }
                }
                if let Some(reference) = token_ref {
                    if secret_name(reference).is_none() {
                        return Err(invalid(
                            "model.source.token_ref",
                            "must be a secret reference `secret://<name>`",
                        ));
                    }
                }
            }
            Self::Http { url, sha256, .. } => {
                // Weights fetched over plain HTTP could be replaced in flight,
                // and a digest is the only thing that makes the fetch
                // reproducible, so both are required rather than recommended.
                if https_host(url).is_none() {
                    return Err(invalid("model.source.url", "must be an https:// URL"));
                }
                if !is_sha256_hex(sha256) {
                    return Err(invalid(
                        "model.source.sha256",
                        "must be 64 lowercase hexadecimal characters",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// A full git commit SHA: 40 lowercase hexadecimal characters.
pub fn is_commit_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 64 lowercase hexadecimal characters.
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 96
        && segment != "."
        && segment != ".."
        && !segment.starts_with('.')
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !segment.contains("--")
}

fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(name), None, None) => valid_segment(name),
        (Some(owner), Some(name), None) => valid_segment(owner) && valid_segment(name),
        _ => false,
    }
}

fn valid_pattern(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern.len() <= 256
        && !pattern.starts_with('/')
        && !pattern.contains('\\')
        && !pattern.chars().any(char::is_control)
        && pattern
            .split('/')
            .all(|segment| segment != ".." && segment != ".")
}

/// The name of a `secret://<name>` reference, or `None` for anything else.
pub fn secret_name(reference: &str) -> Option<&str> {
    let name = reference.strip_prefix("secret://")?;
    (!name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')))
    .then_some(name)
}

/// The lowercase `host[:port]` of an `https://` URL, or `None` when the URL is
/// not HTTPS, carries credentials, or names no host.
pub fn https_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty()
        || authority.contains('@')
        || authority
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    Some(authority.to_ascii_lowercase())
}

/// ADR 0008 (owner decision 2026-09-25): the ceiling on the bytes downloads
/// may take in a host's sources store when its document states none: 500 GiB.
pub const DEFAULT_SOURCES_MAX_BYTES: i64 = 500 << 30;

/// `allowed | denied` for one remote source kind. `disabled` is accepted as
/// another spelling of `denied`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSwitch {
    /// Default (owner decision 2026-09-25): every host, standalone or
    /// enrolled, materializes a declared remote source unless it says not to.
    #[default]
    Allowed,
    #[serde(alias = "disabled")]
    Denied,
}

impl SourceSwitch {
    /// The switch a flag or variable spells: `allowed`, or `disabled` (also
    /// `denied`). Anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "allowed" => Some(Self::Allowed),
            "disabled" | "denied" => Some(Self::Denied),
            _ => None,
        }
    }
}

/// The host's `model_sources` block as written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawModelSources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub huggingface: Option<SourceSwitch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<SourceSwitch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_hosts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub huggingface_endpoint: Option<String>,
    /// The directory downloads are kept under (`<path>/sources/...`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// ADR 0008, SPEC §7: the host's policy for remote model sources. The sources
/// store is a charged filesystem resource owner; `max_bytes` is its ceiling
/// for materialized sources (verified copies plus reservations in flight).
///
/// Owner decision 2026-09-25: Hugging Face and HTTP sources are allowed by
/// default on every host, with a 500 GiB ceiling; a host that states
/// `denied` (or `disabled`) keeps them off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelSourcePolicy {
    pub huggingface: SourceSwitch,
    pub http: SourceSwitch,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub huggingface_endpoint: Option<String>,
    /// The absolute directory downloads live under (`<path>/sources/...`).
    /// `None` keeps them in the model store, as before the sources store had
    /// a directory of its own; the roles state `<state_dir>/models`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<std::path::PathBuf>,
}

impl Default for ModelSourcePolicy {
    fn default() -> Self {
        Self {
            huggingface: SourceSwitch::Allowed,
            http: SourceSwitch::Allowed,
            max_bytes: Some(DEFAULT_SOURCES_MAX_BYTES),
            allowed_hosts: Vec::new(),
            huggingface_endpoint: None,
            path: None,
        }
    }
}

impl ModelSourcePolicy {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The directory a remote source materializes under: [`Self::path`], or
    /// the host's `model_store` when the policy names none.
    pub fn root<'a>(&'a self, model_store: &'a std::path::Path) -> &'a std::path::Path {
        self.path.as_deref().unwrap_or(model_store)
    }

    pub fn from_raw(raw: Option<RawModelSources>) -> Result<Self, ConfigError> {
        let Some(raw) = raw else {
            return Ok(Self::default());
        };
        let max_bytes = raw
            .max_bytes
            .as_deref()
            .map(crate::effective::parse_bytes)
            .transpose()?
            // Owner decision 2026-09-25: a host that states no ceiling gets
            // the default one, so an allowed download is always bounded.
            .or(Some(DEFAULT_SOURCES_MAX_BYTES));
        let path = raw.path.map(std::path::PathBuf::from);
        if path.as_ref().is_some_and(|path| !path.is_absolute()) {
            return Err(invalid("model_sources.path", "must be absolute"));
        }
        let policy = Self {
            huggingface: raw.huggingface.unwrap_or_default(),
            http: raw.http.unwrap_or_default(),
            max_bytes,
            allowed_hosts: raw
                .allowed_hosts
                .unwrap_or_default()
                .into_iter()
                .map(|host| host.to_ascii_lowercase())
                .collect(),
            huggingface_endpoint: raw.huggingface_endpoint,
            path,
        };
        if policy.max_bytes.is_some_and(|bytes| bytes <= 0) {
            return Err(invalid("model_sources.max_bytes", "must be positive"));
        }
        for host in &policy.allowed_hosts {
            if host.is_empty()
                || host.len() > 253
                || !host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
            {
                return Err(invalid(
                    "model_sources.allowed_hosts",
                    "entries are host names, optionally with a port",
                ));
            }
        }
        if let Some(endpoint) = &policy.huggingface_endpoint {
            if https_host(endpoint).is_none() {
                return Err(invalid(
                    "model_sources.huggingface_endpoint",
                    "must be an https:// URL",
                ));
            }
        }
        Ok(policy)
    }

    /// The written form, for rebuilding a host document from a snapshot.
    pub fn to_raw(&self) -> RawModelSources {
        RawModelSources {
            huggingface: Some(self.huggingface),
            http: Some(self.http),
            max_bytes: self.max_bytes.map(|bytes| format!("{bytes}B")),
            allowed_hosts: (!self.allowed_hosts.is_empty()).then(|| self.allowed_hosts.clone()),
            huggingface_endpoint: self.huggingface_endpoint.clone(),
            path: self
                .path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        }
    }

    /// The Hugging Face endpoint this host fetches from.
    pub fn huggingface_endpoint(&self) -> &str {
        self.huggingface_endpoint
            .as_deref()
            .unwrap_or(DEFAULT_HUGGINGFACE_ENDPOINT)
            .trim_end_matches('/')
    }

    /// ADR 0008: whether this host permits materializing `source`. A local
    /// source is always permitted; a remote one needs its kind allowed (the
    /// default since the owner decision of 2026-09-25) and, where the host
    /// lists allowed hosts, a listed origin.
    pub fn permits(&self, source: &ModelSource) -> Result<(), ConfigError> {
        let denied = |detail: &str| {
            ConfigError::new(ConfigErrorCode::ModelSourceDenied, "model.source", detail)
        };
        let origin = match source {
            ModelSource::Local { .. } => return Ok(()),
            ModelSource::HuggingFace { .. } => {
                if self.huggingface != SourceSwitch::Allowed {
                    return Err(denied(
                        "this host does not allow huggingface sources (model_sources.huggingface)",
                    ));
                }
                https_host(self.huggingface_endpoint())
            }
            ModelSource::Http { url, .. } => {
                if self.http != SourceSwitch::Allowed {
                    return Err(denied(
                        "this host does not allow http sources (model_sources.http)",
                    ));
                }
                https_host(url)
            }
        };
        let origin = origin.ok_or_else(|| denied("the source names no HTTPS origin"))?;
        if !self.allowed_hosts.is_empty() && !self.allowed_hosts.contains(&origin) {
            return Err(denied(
                "the source's origin is not in this host's model_sources.allowed_hosts",
            ));
        }
        Ok(())
    }
}

/// The closed failure categories a materialization reports (`reason`).
pub mod reason {
    /// Host policy does not allow the source (ADR 0008 opt-in).
    pub const DENIED: &str = "denied";
    /// The `token_ref` could not be resolved from the host's secrets.
    pub const SECRET_UNAVAILABLE: &str = "secret_unavailable";
    /// The source is larger than the store's remaining `max_bytes`.
    pub const TOO_LARGE: &str = "too_large";
    /// The filesystem does not have room for the reservation.
    pub const INSUFFICIENT_SPACE: &str = "insufficient_space";
    /// The repository, revision or URL does not exist.
    pub const NOT_FOUND: &str = "not_found";
    /// The origin refused the credentials (or their absence).
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// A downloaded file does not have its pinned digest.
    pub const HASH_MISMATCH: &str = "hash_mismatch";
    /// A downloaded file does not have its announced size.
    pub const SIZE_MISMATCH: &str = "size_mismatch";
    /// The origin did not state the payload's size up front.
    pub const SIZE_UNKNOWN: &str = "size_unknown";
    /// The Hugging Face listing is malformed or names another revision.
    pub const INVALID_LISTING: &str = "invalid_listing";
    /// A tar archive holds links, devices or paths outside itself.
    pub const UNSAFE_ARCHIVE: &str = "unsafe_archive";
    /// Something other than mllm occupies the source's directory.
    pub const STORE_CONFLICT: &str = "store_conflict";
    /// The network failed; partial files are kept for a resume.
    pub const NETWORK: &str = "network";
    /// A local filesystem operation failed.
    pub const IO_ERROR: &str = "io_error";
    /// Another process holds the source's download lock.
    pub const BUSY: &str = "busy";
    /// The request named a local source.
    pub const NOT_REMOTE: &str = "not_remote";

    /// A failure that the same declaration will meet again: retrying it
    /// without a new revision only repeats the download.
    pub fn terminal(reason: &str) -> bool {
        matches!(
            reason,
            DENIED
                | TOO_LARGE
                | NOT_FOUND
                | UNAUTHORIZED
                | HASH_MISMATCH
                | SIZE_MISMATCH
                | SIZE_UNKNOWN
                | INVALID_LISTING
                | UNSAFE_ARCHIVE
                | STORE_CONFLICT
                | NOT_REMOTE
        )
    }

    /// Every category, for validating wire and stored values.
    pub const ALL: &[&str] = &[
        DENIED,
        SECRET_UNAVAILABLE,
        TOO_LARGE,
        INSUFFICIENT_SPACE,
        NOT_FOUND,
        UNAUTHORIZED,
        HASH_MISMATCH,
        SIZE_MISMATCH,
        SIZE_UNKNOWN,
        INVALID_LISTING,
        UNSAFE_ARCHIVE,
        STORE_CONFLICT,
        NETWORK,
        IO_ERROR,
        BUSY,
        NOT_REMOTE,
    ];
}

/// `fnmatch`-style matching as Hugging Face allow patterns use it: `*` matches
/// any run of characters (including `/`), `?` exactly one.
pub fn pattern_matches(pattern: &str, path: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pattern.chars().collect(), path.chars().collect());
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(position) = star {
            pi = position + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    // T14: both spellings decode to the same source; the canonical encoding is
    // the tagged one.
    #[test]
    fn both_spellings_decode_to_one_source() {
        let tagged: ModelSource = serde_json::from_value(
            json!({"type": "huggingface", "repo": "Qwen/Qwen3-4B", "revision": SHA}),
        )
        .unwrap();
        let external: ModelSource = serde_json::from_value(
            json!({"huggingface": {"repo": "Qwen/Qwen3-4B", "revision": SHA}}),
        )
        .unwrap();
        assert_eq!(tagged, external);
        assert_eq!(
            serde_json::to_value(&external).unwrap(),
            json!({"type": "huggingface", "repo": "Qwen/Qwen3-4B", "revision": SHA})
        );
        let local: ModelSource = serde_json::from_value(json!({"local": {"path": "x"}})).unwrap();
        assert_eq!(local, ModelSource::Local { path: "x".into() });
        let http: ModelSource = serde_json::from_value(json!({"http": {
            "url": "https://example.test/w.tar", "sha256": "a".repeat(64), "archive": "tar"
        }}))
        .unwrap();
        assert_eq!(
            http.store_key().unwrap(),
            format!("sources/http/{}-tar", "a".repeat(64))
        );
        for bad in [
            json!({"local": {"path": "x"}, "http": {"url": "u", "sha256": "s"}}),
            json!({"s3": {"path": "x"}}),
            json!({"huggingface": {"repo": "r", "revision": SHA, "locked_commit": SHA}}),
            json!({"type": "huggingface", "repo": "r", "revision": SHA, "locked_commit": SHA}),
        ] {
            assert!(
                serde_json::from_value::<ModelSource>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    // T14: pinned revisions only, safe repository names, safe patterns, and
    // secret references only.
    #[test]
    fn remote_sources_must_be_pinned() {
        let hf = |repo: &str, revision: &str| ModelSource::HuggingFace {
            repo: repo.into(),
            revision: revision.into(),
            files: vec![],
            token_ref: None,
        };
        hf("Qwen/Qwen3-4B", SHA).validate().unwrap();
        hf("gpt2", SHA).validate().unwrap();
        for (repo, revision) in [
            ("Qwen/Qwen3-4B", "main"),
            ("Qwen/Qwen3-4B", "v1.0"),
            ("Qwen/Qwen3-4B", &SHA.to_uppercase()),
            ("Qwen/Qwen3-4B", &SHA[..39]),
            ("../etc", SHA),
            ("a/b/c", SHA),
            ("a/..", SHA),
            ("", SHA),
            ("a--b/c", SHA),
        ] {
            assert!(hf(repo, revision).validate().is_err(), "{repo}@{revision}");
        }
        let with = |files: Vec<&str>, token: Option<&str>| ModelSource::HuggingFace {
            repo: "o/n".into(),
            revision: SHA.into(),
            files: files.into_iter().map(Into::into).collect(),
            token_ref: token.map(Into::into),
        };
        with(
            vec!["*.safetensors", "config.json"],
            Some("secret://hf-token"),
        )
        .validate()
        .unwrap();
        for (files, token) in [
            (vec!["../x"], None),
            (vec!["/abs"], None),
            (vec![""], None),
            (vec![], Some("hf_abcdef")),
            (vec![], Some("secret://")),
            (vec![], Some("secret://../x")),
        ] {
            assert!(
                with(files.clone(), token).validate().is_err(),
                "{files:?} {token:?}"
            );
        }
        let http = |url: &str, sha: &str| ModelSource::Http {
            url: url.into(),
            sha256: sha.into(),
            archive: Archive::None,
        };
        http("https://example.test/w.gguf", &"a".repeat(64))
            .validate()
            .unwrap();
        for (url, sha) in [
            ("http://example.test/w", "a".repeat(64)),
            ("https://", "a".repeat(64)),
            ("https://user:pw@example.test/w", "a".repeat(64)),
            ("https://example.test/w", "A".repeat(64)),
            ("https://example.test/w", "a".repeat(63)),
        ] {
            assert!(http(url, &sha).validate().is_err(), "{url}");
        }
    }

    // T14: the store key is stable per pinned declaration and differs with
    // the allow patterns.
    #[test]
    fn store_keys_name_one_directory_per_pinned_declaration() {
        let plain = ModelSource::HuggingFace {
            repo: "Qwen/Qwen3-4B".into(),
            revision: SHA.into(),
            files: vec![],
            token_ref: None,
        };
        assert_eq!(
            plain.store_key().unwrap(),
            format!("sources/huggingface/Qwen--Qwen3-4B@{SHA}")
        );
        let narrowed = |files: Vec<&str>| ModelSource::HuggingFace {
            repo: "Qwen/Qwen3-4B".into(),
            revision: SHA.into(),
            files: files.into_iter().map(Into::into).collect(),
            token_ref: Some("secret://t".into()),
        };
        let a = narrowed(vec!["*.json", "*.safetensors"])
            .store_key()
            .unwrap();
        let b = narrowed(vec!["*.safetensors", "*.json"])
            .store_key()
            .unwrap();
        let c = narrowed(vec!["*.json"]).store_key().unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, plain.store_key().unwrap());
        assert_eq!(ModelSource::Local { path: "x".into() }.store_key(), None);
    }

    // T14 (ADR 0008, owner decision 2026-09-25): remote sources are allowed
    // by default with a 500 GiB ceiling; an explicit `denied` (or its
    // `disabled` spelling) wins over the default, and the origin must be
    // listed when the host lists origins.
    #[test]
    fn host_policy_allows_remote_sources_by_default() {
        let hf = ModelSource::HuggingFace {
            repo: "o/n".into(),
            revision: SHA.into(),
            files: vec![],
            token_ref: None,
        };
        let http = ModelSource::Http {
            url: "https://weights.example.test/w".into(),
            sha256: "a".repeat(64),
            archive: Archive::None,
        };
        let raw = |value: serde_json::Value| {
            ModelSourcePolicy::from_raw(Some(serde_json::from_value(value).unwrap()))
        };
        for default in [
            ModelSourcePolicy::default(),
            ModelSourcePolicy::from_raw(None).unwrap(),
            raw(json!({})).unwrap(),
        ] {
            default.permits(&hf).unwrap();
            default.permits(&http).unwrap();
            assert_eq!(default.max_bytes, Some(DEFAULT_SOURCES_MAX_BYTES));
            assert_eq!(default.max_bytes, Some(500 << 30));
            assert!(default.is_default());
        }
        // An allowed kind without a ceiling takes the default one.
        let allowed = raw(json!({"huggingface": "allowed"})).unwrap();
        assert_eq!(allowed.max_bytes, Some(DEFAULT_SOURCES_MAX_BYTES));
        for spelling in ["denied", "disabled"] {
            let off = raw(json!({"huggingface": spelling, "http": spelling})).unwrap();
            for source in [&hf, &http] {
                let error = off.permits(source).unwrap_err();
                assert_eq!(error.code, ConfigErrorCode::ModelSourceDenied);
            }
            assert!(!off.is_default());
        }
        let only_http = raw(json!({"huggingface": "disabled"})).unwrap();
        assert!(only_http.permits(&hf).is_err());
        only_http.permits(&http).unwrap();
        ModelSourcePolicy::default()
            .permits(&ModelSource::Local { path: "x".into() })
            .unwrap();
        let policy =
            raw(json!({"huggingface": "allowed", "http": "allowed", "max_bytes": "1GiB"})).unwrap();
        policy.permits(&hf).unwrap();
        policy.permits(&http).unwrap();
        let listed = raw(json!({
            "huggingface": "allowed", "http": "allowed", "max_bytes": "1GiB",
            "allowed_hosts": ["huggingface.co"]
        }))
        .unwrap();
        listed.permits(&hf).unwrap();
        assert_eq!(
            listed.permits(&http).unwrap_err().code,
            ConfigErrorCode::ModelSourceDenied
        );
        assert!(raw(json!({"huggingface_endpoint": "http://mirror"})).is_err());
        assert!(raw(json!({"max_bytes": "0B"})).is_err());
        assert_eq!(
            ModelSourcePolicy::from_raw(Some(policy.to_raw())).unwrap(),
            policy
        );
    }

    // T14 (owner decision 2026-09-25): downloads live in their own store,
    // `<path>/sources`; without a path they stay in the model store.
    #[test]
    fn the_sources_store_is_its_own_directory_when_stated() {
        let raw = |value: serde_json::Value| {
            ModelSourcePolicy::from_raw(Some(serde_json::from_value(value).unwrap()))
        };
        let store = std::path::Path::new("/models");
        assert_eq!(ModelSourcePolicy::default().root(store), store);
        let stated = raw(json!({"path": "/state/models"})).unwrap();
        assert_eq!(stated.root(store), std::path::Path::new("/state/models"));
        assert!(!stated.is_default());
        assert_eq!(
            ModelSourcePolicy::from_raw(Some(stated.to_raw())).unwrap(),
            stated
        );
        assert!(raw(json!({"path": "relative/models"})).is_err());
    }

    #[test]
    fn allow_patterns_match_like_fnmatch() {
        assert!(pattern_matches("*.safetensors", "model-00001.safetensors"));
        assert!(pattern_matches("*.json", "sub/dir/config.json"));
        assert!(pattern_matches("config.json", "config.json"));
        assert!(pattern_matches(
            "model-?????-of-*.safetensors",
            "model-00001-of-00002.safetensors"
        ));
        assert!(!pattern_matches("*.json", "model.safetensors"));
        assert!(!pattern_matches("config.json", "sub/config.json"));
    }
}

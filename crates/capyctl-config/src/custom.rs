//! ADR 0031: custom engines, one-off engines CapyCTL has no integration for.
//!
//! A host administrator states how to start the program in a manifest
//! (§2): the whole argument vector after the executable, with placeholders
//! CapyCTL fills from the launch, the API every deployment on it serves, an
//! optional health path, an optional idle signal and an optional version.
//! `capyctl engine add --custom` reads the manifest once, validates it here
//! and copies it, with its digest, into the profile; the profile is the only
//! copy every later reader sees. CapyCTL does not know the program's options
//! (SPEC §8.2), so nothing here interprets an argument: a placeholder is the
//! only substitution, and its value is one argument or a part of one, never
//! split and never passed through a shell (SPEC §8.1).

use crate::effective::DeploymentApi;
use crate::engine_env::{EnvRefusal, MAX_NAMES};
use crate::error::{ConfigError, ConfigErrorCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

/// ADR 0031 §2: the manifest's `kind`.
pub const MANIFEST_KIND: &str = "custom_engine";
/// ADR 0031 §2: the only manifest schema version.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
/// ADR 0031 §2: a manifest is at most 64 KiB.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// ADR 0031 §10: the refusal of a manifest CapyCTL cannot use.
pub const MANIFEST_INVALID: &str = "custom_manifest_invalid";
/// ADR 0031 §3: the build fingerprint of a manifest without `version`.
pub const UNVERSIONED: &str = "unversioned";
/// ADR 0031 §7: what `{host}` renders, always.
pub const LOOPBACK_HOST: &str = "127.0.0.1";
/// ADR 0031 §10: an owned process listens off loopback.
pub const LISTENER_EXPOSED: &str = "custom_listener_exposed";
/// ADR 0031 §9: the host capability a custom engine's deployments need.
pub const CAPABILITY: &str = "custom_engines";

const MAX_ARGS: usize = 256;
const MAX_ARG_BYTES: usize = 4096;
const MAX_VERSION_BYTES: usize = 128;
const MAX_PATH_BYTES: usize = 1024;
const MAX_IDLE_NAMES: usize = 16;

/// ADR 0031 §2: the placeholders a manifest may use, in `args` and in `env`
/// values only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Placeholder {
    /// `{host}`: `127.0.0.1`, always (§7).
    Host,
    /// `{port}`: the instance's leased loopback port.
    Port,
    /// `{model_dir}`: the absolute checkpoint directory.
    ModelDir,
    /// `{served_name}`: the name CapyCTL forwards as `model`.
    ServedName,
    /// `{context_length}`: the deployment's `engine_config.context_length`.
    ContextLength,
    /// `{api_key}`: the launch's per-launch engine key.
    ApiKey,
}

impl Placeholder {
    pub const ALL: [Placeholder; 6] = [
        Placeholder::Host,
        Placeholder::Port,
        Placeholder::ModelDir,
        Placeholder::ServedName,
        Placeholder::ContextLength,
        Placeholder::ApiKey,
    ];

    /// The placeholders every manifest must use (§2).
    pub const REQUIRED: [Placeholder; 3] =
        [Placeholder::Host, Placeholder::Port, Placeholder::ModelDir];

    pub fn name(self) -> &'static str {
        match self {
            Placeholder::Host => "host",
            Placeholder::Port => "port",
            Placeholder::ModelDir => "model_dir",
            Placeholder::ServedName => "served_name",
            Placeholder::ContextLength => "context_length",
            Placeholder::ApiKey => "api_key",
        }
    }

    pub fn from_name(name: &str) -> Option<Placeholder> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// One piece of a template string: literal text or a placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    Value(Placeholder),
}

/// ADR 0031 §2: a template string split into its pieces. `{{` and `}}` are
/// literal braces; any other brace must open or close a known placeholder.
pub fn parse_template(text: &str) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '{' => {
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                if !closed {
                    return Err(format!("`{text}` opens a placeholder it does not close (write `{{{{` for a literal brace)"));
                }
                let placeholder = Placeholder::from_name(&name).ok_or_else(|| {
                    format!(
                        "`{text}` uses the unknown placeholder `{{{name}}}`; the placeholders are {}",
                        Placeholder::ALL
                            .iter()
                            .map(|p| format!("{{{}}}", p.name()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
                if !literal.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut literal)));
                }
                pieces.push(Piece::Value(placeholder));
            }
            '}' => {
                return Err(format!(
                    "`{text}` closes a brace it did not open (write `}}}}` for a literal brace)"
                ))
            }
            c => literal.push(c),
        }
    }
    if !literal.is_empty() {
        pieces.push(Piece::Text(literal));
    }
    Ok(pieces)
}

/// ADR 0031 §6: the engine's idle signal: a path on its own loopback port,
/// read for Prometheus gauges or JSON fields that must all be present and 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleSignal {
    pub url: String,
    /// Prometheus text: each gauge present and 0, summed over its labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gauges: Vec<String>,
    /// A JSON object: each field path (dot-separated) present and 0 or `false`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub json: Vec<String>,
}

impl IdleSignal {
    /// How `engine list` shows the signal.
    pub fn describe(&self) -> String {
        if self.gauges.is_empty() {
            format!("{} json {}", self.url, self.json.join(","))
        } else {
            format!("{} gauges {}", self.url, self.gauges.join(","))
        }
    }
}

/// ADR 0031 §2: the manifest, closed. It is copied into the profile as it
/// validated; its digest ([`CustomManifest::digest`]) names it in the profile
/// and in the recipe fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomManifest {
    pub kind: String,
    pub schema_version: u32,
    pub api: DeploymentApi,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<IdleSignal>,
}

/// Why a manifest was refused: [`MANIFEST_INVALID`], or
/// `engine_env_reserved:<name>` for an `env` name CapyCTL owns (§2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError {
    pub code: String,
    pub detail: String,
}

impl ManifestError {
    fn invalid(detail: impl Into<String>) -> Self {
        Self {
            code: MANIFEST_INVALID.into(),
            detail: detail.into(),
        }
    }

    /// The refusal as a configuration error at `path`, its closed code first.
    pub fn at(&self, path: &str) -> ConfigError {
        ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            path,
            format!("{}: {}", self.code, self.detail),
        )
    }
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for ManifestError {}

impl CustomManifest {
    /// ADR 0031 §2: read a manifest file's text: YAML (duplicate keys
    /// refused), at most 64 KiB, closed, then [`Self::validate`]d.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::invalid(format!(
                "the manifest is {} bytes; a manifest is at most 64 KiB ({MAX_MANIFEST_BYTES} bytes)",
                text.len()
            )));
        }
        let value = crate::strict_yaml::parse_document(text)
            .map_err(|e| ManifestError::invalid(format!("{} at `{}`", e.detail, e.path)))?;
        Self::from_value(value)
    }

    /// A manifest as a profile carries it (JSON), validated.
    pub fn from_value(value: serde_json::Value) -> Result<Self, ManifestError> {
        let manifest: CustomManifest = serde_path_to_error::deserialize(value).map_err(|e| {
            let path = e.path().to_string();
            let mut detail = e.into_inner().to_string();
            if path.starts_with("args") || path.starts_with("env") {
                detail.push_str(
                    " (quote every argument that holds a placeholder: \"{port}\" is a string, {port} is a YAML mapping)",
                );
            }
            ManifestError::invalid(format!("{detail} at `{path}`"))
        })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// ADR 0031 §2, §10: everything `custom_manifest_invalid` and
    /// `engine_env_reserved:<name>` cover. The reserved check runs again at
    /// launch (§2), against the then-current reserved names.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.kind != MANIFEST_KIND {
            return Err(ManifestError::invalid(format!(
                "`kind` is `{}`; a manifest says `kind: {MANIFEST_KIND}`",
                self.kind
            )));
        }
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::invalid(format!(
                "`schema_version` is {}; this release reads version {MANIFEST_SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        if let Some(version) = &self.version {
            if version.trim().is_empty()
                || version.len() > MAX_VERSION_BYTES
                || version.chars().any(|c| c.is_control())
            {
                return Err(ManifestError::invalid(format!(
                    "`version` must be one line of 1 to {MAX_VERSION_BYTES} bytes"
                )));
            }
        }
        if let Some(root) = &self.environment {
            if !absolute_clean(root) {
                return Err(ManifestError::invalid(format!(
                    "`environment` must be an absolute directory without `.` or `..` parts, not `{root}`"
                )));
            }
        }
        if self.args.len() > MAX_ARGS {
            return Err(ManifestError::invalid(format!(
                "`args` holds {} entries; at most {MAX_ARGS}",
                self.args.len()
            )));
        }
        let mut used = BTreeSet::new();
        for (index, arg) in self.args.iter().enumerate() {
            if arg.len() > MAX_ARG_BYTES || arg.contains('\0') {
                return Err(ManifestError::invalid(format!(
                    "`args[{index}]` must be at most {MAX_ARG_BYTES} bytes without NUL"
                )));
            }
            used.extend(
                placeholders(arg)
                    .map_err(|e| ManifestError::invalid(format!("`args[{index}]`: {e}")))?,
            );
        }
        if self.env.len() > MAX_NAMES {
            return Err(ManifestError::invalid(format!(
                "`env` holds more than {MAX_NAMES} names"
            )));
        }
        for (name, value) in &self.env {
            match crate::engine_env::check_entry(name, value) {
                Ok(()) => {}
                Err(EnvRefusal::Reserved(name)) => {
                    return Err(ManifestError {
                        code: format!("engine_env_reserved:{name}"),
                        detail: format!(
                            "the manifest's `env` names `{name}`, which CapyCTL sets or closes for every engine"
                        ),
                    })
                }
                Err(other) => return Err(ManifestError::invalid(other.code())),
            }
            used.extend(
                placeholders(value)
                    .map_err(|e| ManifestError::invalid(format!("`env.{name}`: {e}")))?,
            );
        }
        for required in Placeholder::REQUIRED {
            if !used.contains(&required) {
                return Err(ManifestError::invalid(format!(
                    "the manifest never uses `{{{}}}`; `args` (or an `env` value) must place {{host}}, {{port}} and {{model_dir}}",
                    required.name()
                )));
            }
        }
        if let Some(health) = &self.health {
            loopback_path("health", health)?;
        }
        if let Some(idle) = &self.idle {
            loopback_path("idle.url", &idle.url)?;
            let (field, names) = match (idle.gauges.is_empty(), idle.json.is_empty()) {
                (false, true) => ("idle.gauges", &idle.gauges),
                (true, false) => ("idle.json", &idle.json),
                _ => {
                    return Err(ManifestError::invalid(
                        "`idle` names exactly one of `gauges` (Prometheus text) or `json` (field paths), not empty",
                    ))
                }
            };
            if names.len() > MAX_IDLE_NAMES {
                return Err(ManifestError::invalid(format!(
                    "`{field}` names more than {MAX_IDLE_NAMES} entries"
                )));
            }
            for name in names {
                let shaped = if field == "idle.gauges" {
                    prometheus_name(name)
                } else {
                    !name.is_empty()
                        && name.split('.').all(|part| {
                            !part.is_empty()
                                && part.len() <= 128
                                && !part.contains(['{', '}'])
                                && !part.chars().any(char::is_control)
                        })
                };
                if !shaped {
                    return Err(ManifestError::invalid(format!(
                        "`{field}` entry `{name}` is not a {}",
                        if field == "idle.gauges" {
                            "Prometheus metric name"
                        } else {
                            "dot-separated field path"
                        }
                    )));
                }
            }
        }
        Ok(())
    }

    /// The placeholders the manifest uses, in `args` and `env` values.
    pub fn placeholders(&self) -> BTreeSet<Placeholder> {
        self.args
            .iter()
            .chain(self.env.values())
            .flat_map(|text| placeholders(text).unwrap_or_default())
            .collect()
    }

    /// Whether the launch generates a per-launch engine key (§7).
    pub fn uses_api_key(&self) -> bool {
        self.placeholders().contains(&Placeholder::ApiKey)
    }

    /// Whether `engine add` warns: the key in `args` is visible to every
    /// local user in `/proc/<pid>/cmdline` (§7).
    pub fn api_key_in_args(&self) -> bool {
        self.args
            .iter()
            .any(|arg| placeholders(arg).is_ok_and(|used| used.contains(&Placeholder::ApiKey)))
    }

    /// ADR 0031 §3: the profile's build fingerprint.
    pub fn build_fingerprint(&self) -> String {
        self.version.clone().unwrap_or_else(|| UNVERSIONED.into())
    }

    /// ADR 0031 §2, §3: `sha256:` over the manifest's canonical JSON (its
    /// fields in declaration order, `env` sorted, absent optional fields
    /// left out), so the digest a profile records is recomputed from the
    /// profile alone.
    pub fn digest(&self) -> String {
        let canonical = serde_json::to_vec(self).expect("a manifest always encodes");
        format!("sha256:{}", hex::encode(Sha256::digest(canonical)))
    }
}

/// The placeholders one template string uses.
pub fn placeholders(text: &str) -> Result<BTreeSet<Placeholder>, String> {
    Ok(parse_template(text)?
        .into_iter()
        .filter_map(|piece| match piece {
            Piece::Value(p) => Some(p),
            Piece::Text(_) => None,
        })
        .collect())
}

fn absolute_clean(path: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute()
        && path.as_os_str().len() <= MAX_PATH_BYTES
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

/// ADR 0031 §10: `health` and `idle.url` are paths on the engine's own
/// loopback port: they start with one `/`, carry no placeholder, no scheme or
/// authority, and nothing a request line cannot hold.
fn loopback_path(field: &str, path: &str) -> Result<(), ManifestError> {
    if path.contains(['{', '}']) {
        return Err(ManifestError::invalid(format!(
            "`{field}` holds a placeholder; placeholders are allowed only in `args` and `env` values"
        )));
    }
    let shaped = path.starts_with('/')
        && !path.starts_with("//")
        && path.len() <= MAX_PATH_BYTES
        && path
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'#' && b != b'\\');
    if !shaped {
        return Err(ManifestError::invalid(format!(
            "`{field}` must be a path on the engine's own loopback port, such as `/health`, not `{path}`"
        )));
    }
    Ok(())
}

fn prometheus_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_' || b == b':')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b':')
        && name.len() <= 256
}

/// ADR 0031 §2, §3: the custom block of a profile: the manifest as it
/// validated, its digest, and (ADR 0031 §8) the installation digest the
/// operator approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomProfile {
    pub manifest: CustomManifest,
    pub manifest_digest: String,
    /// ADR 0031 §3, §8: the installation digest `engine add --custom`
    /// measured and showed in the summary the operator confirmed. When set,
    /// registration pins it as the baseline and never adopts a different
    /// measurement. `None` when it could not be measured (or the profile was
    /// written by hand).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_digest: Option<String>,
}

impl CustomProfile {
    pub fn new(manifest: CustomManifest) -> Self {
        let manifest_digest = manifest.digest();
        Self {
            manifest,
            manifest_digest,
            installation_digest: None,
        }
    }

    /// A profile's `custom` block, validated: the manifest again, its digest
    /// recomputed from it, and a pinned installation digest of the form
    /// `sha256:` + 64 lowercase hex.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, ManifestError> {
        let digest = value
            .get("manifest_digest")
            .and_then(|d| d.as_str())
            .ok_or_else(|| ManifestError::invalid("`custom.manifest_digest` is missing"))?;
        if let Some(other) = value.as_object().and_then(|o| {
            o.keys().find(|k| {
                !matches!(
                    k.as_str(),
                    "manifest" | "manifest_digest" | "installation_digest"
                )
            })
        }) {
            return Err(ManifestError::invalid(format!(
                "unknown field `custom.{other}`"
            )));
        }
        let installation_digest = match value.get("installation_digest") {
            None => None,
            Some(pinned) => Some(
                pinned
                    .as_str()
                    .filter(|d| {
                        d.strip_prefix("sha256:")
                            .is_some_and(crate::model_source::is_sha256_hex)
                    })
                    .ok_or_else(|| {
                        ManifestError::invalid(
                            "`custom.installation_digest` must be sha256: and 64 lowercase hex characters",
                        )
                    })?
                    .to_owned(),
            ),
        };
        let manifest = CustomManifest::from_value(
            value
                .get("manifest")
                .cloned()
                .ok_or_else(|| ManifestError::invalid("`custom.manifest` is missing"))?,
        )?;
        let mut profile = Self::new(manifest);
        if profile.manifest_digest != digest {
            return Err(ManifestError::invalid(format!(
                "`custom.manifest_digest` is {digest}, but the manifest digests to {}",
                profile.manifest_digest
            )));
        }
        profile.installation_digest = installation_digest;
        Ok(profile)
    }
}

/// The values a launch fills the placeholders with (ADR 0031 §2).
#[derive(Clone, PartialEq, Eq)]
pub struct PlaceholderValues {
    pub port: u16,
    pub model_dir: String,
    pub served_name: String,
    pub context_length: Option<u32>,
    pub api_key: Option<String>,
}

/// The key stays out of every Debug rendering (SPEC §13.3).
impl std::fmt::Debug for PlaceholderValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaceholderValues")
            .field("port", &self.port)
            .field("model_dir", &self.model_dir)
            .field("served_name", &self.served_name)
            .field("context_length", &self.context_length)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// ADR 0031 §8: a value that comes from the deployment (the served name, the
/// checkpoint directory) is refused when it starts with `-` or holds a control
/// character, so a program can never read it as an option: deployment
/// operators cannot add arguments.
pub fn is_invalid_value(value: &str) -> bool {
    value.starts_with('-') || value.chars().any(char::is_control)
}

/// ADR 0031 §2, §8: a placeholder a launch cannot render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceholderError {
    /// The launch has no value for it (`custom_placeholder_unset:<name>`).
    Unset(Placeholder),
    /// Its value could be read as an option (`custom_value_invalid:<name>`).
    Invalid(Placeholder),
}

impl std::fmt::Display for PlaceholderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unset(p) => write!(f, "custom_placeholder_unset:{}", p.name()),
            Self::Invalid(p) => write!(f, "custom_value_invalid:{}", p.name()),
        }
    }
}

impl PlaceholderValues {
    fn value(&self, placeholder: Placeholder) -> Result<String, PlaceholderError> {
        // ADR 0031 §8: resolution refuses these already; a frozen launch is
        // checked again so nothing older slips through.
        let from_deployment = |value: &str| {
            if is_invalid_value(value) {
                Err(PlaceholderError::Invalid(placeholder))
            } else {
                Ok(value.to_owned())
            }
        };
        Ok(match placeholder {
            Placeholder::Host => LOOPBACK_HOST.into(),
            Placeholder::Port => self.port.to_string(),
            Placeholder::ModelDir => from_deployment(&self.model_dir)?,
            Placeholder::ServedName => from_deployment(&self.served_name)?,
            Placeholder::ContextLength => self
                .context_length
                .ok_or(PlaceholderError::Unset(placeholder))?
                .to_string(),
            Placeholder::ApiKey => self
                .api_key
                .clone()
                .ok_or(PlaceholderError::Unset(placeholder))?,
        })
    }

    /// One template string rendered: each placeholder replaced by its value,
    /// the result one string (one argument), never split (SPEC §8.1).
    pub fn render(&self, template: &str) -> Result<String, PlaceholderError> {
        let mut out = String::new();
        // A template that failed to parse never reached a validated manifest.
        for piece in parse_template(template).unwrap_or_default() {
            match piece {
                Piece::Text(text) => out.push_str(&text),
                Piece::Value(p) => out.push_str(&self.value(p)?),
            }
        }
        Ok(out)
    }

    /// The manifest's argument vector rendered, one argument per entry.
    pub fn render_args(&self, manifest: &CustomManifest) -> Result<Vec<String>, PlaceholderError> {
        manifest.args.iter().map(|arg| self.render(arg)).collect()
    }

    /// The manifest's environment rendered.
    pub fn render_env(
        &self,
        manifest: &CustomManifest,
    ) -> Result<BTreeMap<String, String>, PlaceholderError> {
        manifest
            .env
            .iter()
            .map(|(name, value)| Ok((name.clone(), self.render(value)?)))
            .collect()
    }
}

/// ADR 0031 §8: the command a resolved custom deployment runs, as
/// `inspect deployment --effective-config` shows it before a launch: the
/// executable and its arguments rendered from what resolution knows, the
/// leased port named `<leased port>` (a launch leases it) and the engine key
/// always `<redacted>`; and the environment names with their sources, never
/// their values. `None` when `effective` is not a custom engine's.
pub fn command_view(effective: &serde_json::Value) -> Option<serde_json::Value> {
    let profile = &effective["profile"];
    if profile["engine"].as_str() != Some(crate::engine_policy::Engine::Custom.name()) {
        return None;
    }
    let manifest: CustomManifest =
        serde_json::from_value(profile["custom"]["manifest"].clone()).ok()?;
    let model_dir = effective["model"]["resolved_path"]
        .as_str()
        .unwrap_or("<checkpoint directory>");
    let served = effective["routes"][0].as_str().unwrap_or_default();
    let context = effective["engine_config"]["common"]["context_length"].as_u64();
    let shown = |template: &str| {
        let mut out = String::new();
        for piece in parse_template(template).unwrap_or_default() {
            match piece {
                Piece::Text(text) => out.push_str(&text),
                Piece::Value(Placeholder::Host) => out.push_str(LOOPBACK_HOST),
                Piece::Value(Placeholder::Port) => out.push_str("<leased port>"),
                Piece::Value(Placeholder::ModelDir) => out.push_str(model_dir),
                Piece::Value(Placeholder::ServedName) => out.push_str(served),
                Piece::Value(Placeholder::ContextLength) => match context {
                    Some(tokens) => out.push_str(&tokens.to_string()),
                    None => out.push_str("<unset>"),
                },
                Piece::Value(Placeholder::ApiKey) => out.push_str(crate::engine_env::REDACTED),
            }
        }
        out
    };
    let env: serde_json::Map<String, serde_json::Value> = effective["engine_env"]["vars"]
        .as_object()
        .map(|vars| {
            vars.iter()
                .map(|(name, var)| (name.clone(), var[1].clone()))
                .collect()
        })
        .unwrap_or_default();
    Some(serde_json::json!({
        "executable": profile["executable"],
        "args": manifest.args.iter().map(|arg| shown(arg)).collect::<Vec<_>>(),
        "env_sources": env,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basal() -> &'static str {
        "kind: custom_engine\nschema_version: 1\napi: decisions\nversion: \"2.2.0\"\n\
         args: [--mode, eager, --host, \"{host}\", --port, \"{port}\", --model, \"{model_dir}\"]\n\
         health: /health\n"
    }

    // T44: the worked example of ADR 0031 §11 parses and digests stably.
    #[test]
    fn the_worked_example_parses() {
        let manifest = CustomManifest::parse(basal()).unwrap();
        assert_eq!(manifest.api, DeploymentApi::Decisions);
        assert_eq!(manifest.build_fingerprint(), "2.2.0");
        assert!(!manifest.uses_api_key());
        assert_eq!(
            manifest.digest(),
            CustomManifest::parse(basal()).unwrap().digest()
        );
        assert!(manifest.digest().starts_with("sha256:"));
    }

    // T44: each placeholder is one argument or a part of one, never split.
    #[test]
    fn rendering_substitutes_each_placeholder_into_one_argument() {
        let manifest = CustomManifest::parse(
            "kind: custom_engine\nschema_version: 1\napi: chat\n\
             args: [\"--listen={host}:{port}\", \"{model_dir}\", \"{{literal}}\", \"--name\", \"{served_name}\"]\n\
             env: {KEY: \"{api_key}\"}\n",
        )
        .unwrap();
        let values = PlaceholderValues {
            port: 40001,
            model_dir: "/m/a model; rm -rf /".into(),
            served_name: "x y".into(),
            context_length: None,
            api_key: Some("k".into()),
        };
        assert_eq!(
            values.render_args(&manifest).unwrap(),
            vec![
                "--listen=127.0.0.1:40001",
                "/m/a model; rm -rf /",
                "{literal}",
                "--name",
                "x y"
            ]
        );
        assert_eq!(values.render_env(&manifest).unwrap()["KEY"], "k");
        assert!(manifest.uses_api_key() && !manifest.api_key_in_args());
    }

    // T44: an unknown or missing required placeholder, a placeholder in
    // `health` or `idle`, and an open brace are refused.
    #[test]
    fn bad_manifests_are_refused() {
        let base = "kind: custom_engine\nschema_version: 1\napi: chat\n";
        for (rest, needle) in [
            ("args: [\"{host}\", \"{port}\", \"{model}\"]\n", "unknown placeholder"),
            ("args: [\"{host}\", \"{port}\"]\n", "{model_dir}"),
            ("args: [\"{host}\", \"{port}\", \"{model_dir}\"]\nhealth: \"/h/{port}\"\n", "placeholder"),
            ("args: [\"{host}\", \"{port}\", \"{model_dir}\"]\nidle: {url: \"/{api_key}\", gauges: [a]}\n", "placeholder"),
            ("args: [\"{host}\", \"{port}\", \"{model_dir}\"]\nhealth: http://x/h\n", "loopback"),
            ("args: [\"{host\", \"{port}\", \"{model_dir}\"]\n", "does not close"),
            ("args: [\"{host}\", \"{port}\", \"{model_dir}\"]\nextra: 1\n", "unknown field"),
            ("args: [\"{host}\", \"{port}\", \"{model_dir}\"]\nidle: {url: /m}\n", "exactly one"),
            ("args: [\"{host}\", \"{port}\", \"{model_dir}\"]\nenvironment: venv\n", "absolute"),
            ("args: [--host, {host}]\n", "quote"),
        ] {
            let error = CustomManifest::parse(&format!("{base}{rest}")).unwrap_err();
            assert_eq!(error.code, MANIFEST_INVALID, "{rest}");
            assert!(error.detail.contains(needle), "{rest}: {error}");
        }
        let reserved = CustomManifest::parse(&format!(
            "{base}args: [\"{{host}}\", \"{{port}}\", \"{{model_dir}}\"]\nenv: {{CUDA_VISIBLE_DEVICES: \"0\"}}\n"
        ))
        .unwrap_err();
        assert_eq!(reserved.code, "engine_env_reserved:CUDA_VISIBLE_DEVICES");
        let big = format!("{base}# {}\n", "x".repeat(MAX_MANIFEST_BYTES));
        assert!(CustomManifest::parse(&big)
            .unwrap_err()
            .detail
            .contains("64"));
    }

    // T44: an unset `{context_length}` is named.
    #[test]
    fn an_unset_context_length_is_named() {
        let values = PlaceholderValues {
            port: 1,
            model_dir: "/m".into(),
            served_name: "s".into(),
            context_length: None,
            api_key: None,
        };
        assert_eq!(
            values.render("{context_length}").unwrap_err().to_string(),
            "custom_placeholder_unset:context_length"
        );
    }

    // T44 (ADR 0031 §8): a frozen launch whose served name or checkpoint
    // directory could be read as an option is refused at render, in `args`
    // and in `env` values alike; the other placeholders are CapyCTL's own.
    #[test]
    fn a_dash_leading_or_control_value_is_refused_at_render() {
        let ok = PlaceholderValues {
            port: 1,
            model_dir: "/m/a-b".into(),
            served_name: "basal-2".into(),
            context_length: Some(4096),
            api_key: Some("k".into()),
        };
        assert_eq!(
            ok.render("--name={served_name} {model_dir}").unwrap(),
            "--name=basal-2 /m/a-b"
        );
        for (served_name, model_dir, code) in [
            (
                "--log-decisions=/tmp/x",
                "/m",
                "custom_value_invalid:served_name",
            ),
            ("-v", "/m", "custom_value_invalid:served_name"),
            ("a\nb", "/m", "custom_value_invalid:served_name"),
            ("a\u{7f}", "/m", "custom_value_invalid:served_name"),
            ("s", "--trust-remote-code", "custom_value_invalid:model_dir"),
            ("s", "/m\t--x", "custom_value_invalid:model_dir"),
        ] {
            let values = PlaceholderValues {
                served_name: served_name.into(),
                model_dir: model_dir.into(),
                ..ok.clone()
            };
            let template = if code.ends_with("served_name") {
                "{served_name}"
            } else {
                "{model_dir}"
            };
            assert_eq!(
                values.render(template).unwrap_err().to_string(),
                code,
                "{values:?}"
            );
            assert_eq!(
                values
                    .render(&format!("--x={template}"))
                    .unwrap_err()
                    .to_string(),
                code
            );
            // A template that never renders the value is unaffected.
            assert_eq!(values.render("{port}").unwrap(), "1");
        }
    }

    // T44: a profile's block is checked against its own digest.
    #[test]
    fn a_profile_block_must_match_its_digest() {
        let profile = CustomProfile::new(CustomManifest::parse(basal()).unwrap());
        let mut value = serde_json::to_value(&profile).unwrap();
        assert_eq!(CustomProfile::from_value(&value).unwrap(), profile);
        value["manifest"]["version"] = "9".into();
        assert!(CustomProfile::from_value(&value)
            .unwrap_err()
            .detail
            .contains("digests to"));
    }

    // T44 T21 T22 (ADR 0031 §3, §8): the installation digest the operator
    // approved round-trips with the block; an unpinned block serializes
    // without the key; a malformed pin is `custom_manifest_invalid`.
    #[test]
    fn a_profile_block_carries_its_pinned_installation_digest() {
        let mut profile = CustomProfile::new(CustomManifest::parse(basal()).unwrap());
        assert!(serde_json::to_value(&profile)
            .unwrap()
            .get("installation_digest")
            .is_none());
        let pinned = format!("sha256:{}", "a".repeat(64));
        profile.installation_digest = Some(pinned.clone());
        let value = serde_json::to_value(&profile).unwrap();
        assert_eq!(value["installation_digest"], pinned.as_str());
        let read = CustomProfile::from_value(&value).unwrap();
        assert_eq!(read.installation_digest.as_deref(), Some(pinned.as_str()));
        assert_eq!(read, profile);
        assert_eq!(
            serde_json::from_value::<CustomProfile>(value.clone()).unwrap(),
            profile
        );
        for bad in [
            serde_json::json!(format!("sha256:{}", "A".repeat(64))),
            serde_json::json!(format!("sha256:{}", "a".repeat(63))),
            serde_json::json!(format!("md5:{}", "a".repeat(64))),
            serde_json::json!("unmeasured"),
            serde_json::Value::Null,
            serde_json::json!(7),
        ] {
            let mut malformed = value.clone();
            malformed["installation_digest"] = bad.clone();
            let error = CustomProfile::from_value(&malformed).unwrap_err();
            assert_eq!(error.code, MANIFEST_INVALID, "{bad}");
            assert!(error.detail.contains("installation_digest"), "{bad}");
        }
    }

    // T44, T37 (ADR 0031 §8): the server shows the rendered command with the
    // key redacted and the environment names with their sources.
    #[test]
    fn the_command_view_redacts_the_key() {
        let manifest = CustomManifest::parse(
            "kind: custom_engine\nschema_version: 1\napi: chat\n\
             args: [--host, \"{host}\", --port, \"{port}\", \"{model_dir}\", --key, \"{api_key}\", \"--ctx={context_length}\"]\n",
        )
        .unwrap();
        let effective = serde_json::json!({
            "routes": ["m"],
            "model": {"resolved_path": "/models/m"},
            "engine_config": {"common": {"context_length": 4096}},
            "engine_env": {"vars": {"A": ["v", "manifest"], "B": ["w", "deployment"]}},
            "profile": {"engine": "custom", "executable": "/v/bin/serve",
                        "custom": {"manifest": manifest, "manifest_digest": "x"}},
        });
        let view = command_view(&effective).unwrap();
        assert_eq!(
            view["args"],
            serde_json::json!([
                "--host",
                "127.0.0.1",
                "--port",
                "<leased port>",
                "/models/m",
                "--key",
                "<redacted>",
                "--ctx=4096"
            ])
        );
        assert_eq!(
            view["env_sources"],
            serde_json::json!({"A": "manifest", "B": "deployment"})
        );
        assert!(command_view(&serde_json::json!({"profile": {"engine": "vllm"}})).is_none());
    }
}

//! ADR 0018 §2 (owner decision 2026-09-25): engine registration writes only
//! `engines.yaml`, an capyctl-owned file beside the role's configuration file.
//! The host or standalone document is never rewritten; the role merges the
//! two at load, and a profile name declared in both is refused. Writes hold
//! `engines.yaml.lock`, go through a temporary file, sync and rename, and
//! record the revision on the first line (a comment the parser ignores).
use crate::effective::InstallationDrift;
use crate::engine_policy::Engine;
use crate::{parse_strict, ConfigError, ConfigErrorCode, ConfigKind};
use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const ENGINES_FILE: &str = "engines.yaml";
pub const REVISION_HEADER: &str = "# capyctl-document-revision: ";
/// The largest host document a publication carries
/// (`capyctl_store::host_publication`, `config_json.len() > 32768` is refused).
pub const MAX_PUBLISHED_BYTES: usize = 32 * 1024;

fn io(path: &Path, error: impl std::fmt::Display) -> ConfigError {
    ConfigError::new(
        ConfigErrorCode::Io,
        path.display().to_string(),
        error.to_string(),
    )
}

/// ADR 0018 §2: `dir/x.yaml` → `dir/engines.yaml`.
pub fn engines_beside(role_document: &Path) -> PathBuf {
    // A bare relative name (`host.yaml`) has an empty parent, which names the
    // working directory; say so, so the directory can be opened and synced.
    role_document
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join(ENGINES_FILE)
}

/// ADR 0018 §2: beside the role document named with `--config`; without one,
/// `<config home>/capyctl/engines.yaml`, for a host and for standalone alike.
pub fn engines_path(role_document: Option<&Path>, config_home: &Path) -> PathBuf {
    match role_document {
        Some(document) => engines_beside(document),
        None => config_home.join("capyctl").join(ENGINES_FILE),
    }
}

/// `$XDG_CONFIG_HOME`, else `$HOME/.config` (absolute paths only).
pub fn config_home(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            env("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .filter(|p| p.is_absolute())
        })
}

/// ADR 0018 §2: the revision on the first line, or 0.
pub fn revision_of(text: &str) -> u64 {
    text.lines()
        .next()
        .and_then(|line| line.strip_prefix(REVISION_HEADER))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

/// The registered profiles of one role.
#[derive(Debug, Clone, PartialEq)]
pub struct EnginesFile {
    pub path: PathBuf,
    /// 0 when the file does not exist yet.
    pub revision: u64,
    pub profiles: Map<String, Value>,
}

impl EnginesFile {
    /// A missing file is an empty one at revision 0.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(path, &text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path: path.to_path_buf(),
                revision: 0,
                profiles: Map::new(),
            }),
            Err(e) => Err(io(path, e)),
        }
    }

    /// SPEC §15.3: strict, like every role document.
    pub fn parse(path: &Path, text: &str) -> Result<Self, ConfigError> {
        let value = parse_strict(ConfigKind::Engines, text)?;
        let profiles = value["runtime_profiles"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            path: path.to_path_buf(),
            revision: revision_of(text),
            profiles,
        })
    }

    /// JSON-shaped YAML, as `capyctl init` writes, under the revision line.
    pub fn render(&self, revision: u64) -> String {
        let body = serde_json::json!({"schema_version": 1, "kind": "engines", "runtime_profiles": self.profiles});
        format!(
            "{REVISION_HEADER}{revision}\n# Written by `capyctl engine add` and `remove`. The role merges it with its own document.\n{}\n",
            serde_json::to_string_pretty(&body).expect("profiles always encode")
        )
    }
}

/// ADR 0018 §2: `document` (a host document) with `engines`' profiles added.
/// A profile name the host document already declares is refused.
pub fn merge_into_host(document: &mut Value, engines: &EnginesFile) -> Result<(), ConfigError> {
    if engines.profiles.is_empty() {
        return Ok(());
    }
    let declared = document
        .as_object_mut()
        .ok_or_else(|| {
            ConfigError::new(ConfigErrorCode::UnsupportedCombination, "", "not a mapping")
        })?
        .entry("runtime_profiles")
        .or_insert_with(|| Value::Object(Map::new()));
    let declared = declared.as_object_mut().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            "runtime_profiles",
            "must be a mapping",
        )
    })?;
    for (name, profile) in &engines.profiles {
        if declared.contains_key(name) {
            return Err(ConfigError::new(
                ConfigErrorCode::UnsupportedCombination,
                format!("runtime_profiles.{name}"),
                format!(
                    "profile {name} is declared in both the role document and {}; remove one",
                    engines.path.display()
                ),
            ));
        }
        declared.insert(name.clone(), profile.clone());
    }
    Ok(())
}

/// An exclusive advisory lock on `<engines file>.lock` for one
/// read-modify-write. Dropping it releases the lock.
pub struct EnginesLock {
    _file: File,
    path: PathBuf,
    /// Who owns an engines file this write creates (`None`: the writer).
    owner: Option<(u32, u32)>,
}

pub fn lock_engines(path: &Path) -> Result<EnginesLock, ConfigError> {
    lock_engines_for(path, None)
}

/// As [`lock_engines`], naming who owns what this write creates. Controller
/// ruling C1: the CLI is the only writer of engines.yaml, and under the
/// system units it runs as root (`sudo capyctl engine …`); the file and its
/// lock are then created for the role's service user (`owner`, uid and gid),
/// mode 0600, so the service can read the file. An existing file keeps its
/// owner and mode (see [`write_engines`]).
pub fn lock_engines_for(
    path: &Path,
    owner: Option<(u32, u32)>,
) -> Result<EnginesLock, ConfigError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }
    let lock = PathBuf::from(format!("{}.lock", path.display()));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock)
        .map_err(|e| io(&lock, e))?;
    refuse_hardlink(&lock, &file, owner)?;
    if let Some(owner) = owner {
        give(&file, owner).map_err(|e| io(&lock, e))?;
    }
    file.lock().map_err(|e| io(&lock, e))?;
    Ok(EnginesLock {
        _file: file,
        path: path.to_path_buf(),
        owner,
    })
}

/// ADR 0018 §2 hardening (2026-09-25): before any `fchown`, refuse a path
/// that opened to anything but a regular file with exactly one link, owned
/// by root or by the state-dir owner this write is for. Without this check,
/// a hard link planted at `engines.yaml` or `engines.yaml.lock` before the
/// CLI runs as root would make root's `fchown` change that other file's
/// ownership too (they share one inode) — the check runs on the already
/// opened, `O_NOFOLLOW`-opened file descriptor, so there is no gap between
/// checking and using it.
fn refuse_hardlink(path: &Path, file: &File, owner: Option<(u32, u32)>) -> Result<(), ConfigError> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().map_err(|e| io(path, e))?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let allowed_uid = owner.map_or_else(|| unsafe { libc::geteuid() }, |(uid, _)| uid);
    if !meta.is_file() || meta.nlink() != 1 || (meta.uid() != 0 && meta.uid() != allowed_uid) {
        return Err(ConfigError::new(
            ConfigErrorCode::Io,
            path.display().to_string(),
            format!(
                "refusing to use {}: expected a regular file with one link, owned by root or uid {allowed_uid}; found {} link(s) owned by uid {}",
                path.display(),
                meta.nlink(),
                meta.uid()
            ),
        ));
    }
    Ok(())
}

/// `fchown` `file` to `(uid, gid)` unless it already has them (only root may
/// give a file away; a writer that already owns it changes nothing).
fn give(file: &File, (uid, gid): (u32, u32)) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    if (meta.uid(), meta.gid()) == (uid, gid) {
        return Ok(());
    }
    std::os::unix::fs::fchown(file, Some(uid), Some(gid))
}

/// ADR 0018 §2: write `file` under `lock` at the next revision and return it.
/// The revision is read again under the lock. With `host_document`, a name
/// the host document declares is refused and the merged document must fit a
/// publication. Nothing is written when a check fails.
pub fn write_engines(
    file: &EnginesFile,
    lock: &EnginesLock,
    host_document: Option<&Value>,
) -> Result<u64, ConfigError> {
    if lock.path != file.path {
        return Err(ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            file.path.display().to_string(),
            "the lock names another file",
        ));
    }
    if let Some(host) = host_document {
        let mut merged = host.clone();
        merge_into_host(&mut merged, file)?;
        let published = merged.to_string().len();
        if published > MAX_PUBLISHED_BYTES {
            return Err(ConfigError::new(
                ConfigErrorCode::UnsupportedCombination,
                file.path.display().to_string(),
                format!("the host document with these engines would be {published} bytes; a publication carries at most {MAX_PUBLISHED_BYTES} (32768)"),
            ));
        }
    }
    let current = EnginesFile::load(&file.path)?;
    let revision = current.revision + 1;
    let text = file.render(revision);
    // SPEC §15.3: validate before side effects.
    EnginesFile::parse(&file.path, &text)?;
    let dir = file
        .path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // review decision C1: a rewrite keeps the file's owner and mode (the
    // role reads it as its service user; the CLI may be running as root); a
    // new file goes to the lock's named owner, mode 0600.
    //
    // ADR 0018 §2 hardening: opened with `O_NOFOLLOW` (a symlink here makes
    // the open fail with ELOOP; the rename below replaces the link itself
    // rather than following it, so that case still just writes a fresh
    // file) and refused if it is a hard link to something else — otherwise
    // that file's owner would be copied onto the new engines.yaml.
    let (owner, mode) = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&file.path)
    {
        Ok(existing) => {
            refuse_hardlink(&file.path, &existing, lock.owner)?;
            use std::os::unix::fs::MetadataExt;
            let meta = existing.metadata().map_err(|e| io(&file.path, e))?;
            (Some((meta.uid(), meta.gid())), meta.mode() & 0o777)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (lock.owner, 0o600),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => (lock.owner, 0o600),
        Err(e) => return Err(io(&file.path, e)),
    };
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| io(dir, e))?;
    tmp.write_all(text.as_bytes())
        .map_err(|e| io(tmp.path(), e))?;
    if let Some(owner) = owner {
        give(tmp.as_file(), owner).map_err(|e| io(tmp.path(), e))?;
    }
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode))
        .map_err(|e| io(tmp.path(), e))?;
    tmp.as_file().sync_all().map_err(|e| io(tmp.path(), e))?;
    tmp.persist(&file.path)
        .map_err(|e| io(&file.path, e.error))?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io(dir, e))?;
    Ok(revision)
}

/// ADR 0018 §1: the versions the live matrix qualifies. Any other version is
/// shown `custom`; nothing is written for it.
pub const VERIFIED: &[(Engine, &str)] = &[(Engine::Vllm, "0.29.0"), (Engine::Sglang, "0.5.20")];

pub fn is_verified(engine: Engine, version: &str) -> bool {
    VERIFIED.iter().any(|(e, v)| *e == engine && *v == version)
}

/// ADR 0018 §5: the profile names standalone gives its environment-variable
/// installations (`CAPYCTL_VLLM_BIN` / `CAPYCTL_SGLANG_BIN`).
pub const ENVIRONMENT_PROFILES: &[&str] = &["local", "local-vllm", "local-sglang"];

/// ADR 0018 §1: short lowercase identifiers, safe in a JSON path and a
/// status table.
pub fn valid_profile_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
}

/// What `capyctl engine add` measured and the operator chose.
#[derive(Debug, Clone)]
pub struct ProfileSpec {
    pub engine: Engine,
    pub executable: PathBuf,
    pub build_fingerprint: String,
    pub deep_park: bool,
    pub installation_drift: InstallationDrift,
    pub args: Vec<String>,
    /// SPEC §13.3 amendment (owner decision 2026-09-25): the CUDA toolkit root
    /// `engine add` detected ([`detect_cuda_home`]); written as `cuda_home`.
    pub cuda_home: Option<PathBuf>,
}

/// The CUDA toolkit `capyctl engine add` records (SPEC §13.3 amendment, owner
/// decision 2026-09-25): `CUDA_HOME` when it names an absolute, normalized
/// directory holding `bin/nvcc`, else `/usr/local/cuda` when it holds
/// `bin/nvcc`, else none (the engine PATH then stays minimal). The operator
/// adding the engine is the host administrator approving it, as for the
/// executable.
pub fn detect_cuda_home(
    cuda_home_env: Option<&str>,
    has_nvcc: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let normalized = |path: &Path| {
        path.is_absolute()
            && path.components().all(|c| {
                matches!(
                    c,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
    };
    cuda_home_env
        .map(|value| PathBuf::from(value.trim_end_matches('/')))
        .filter(|path| normalized(path) && has_nvcc(&path.join("bin/nvcc")))
        .or_else(|| {
            let default = PathBuf::from("/usr/local/cuda");
            has_nvcc(&default.join("bin/nvcc")).then_some(default)
        })
}

/// ADR 0018 §1: the profile `engine add` writes. SPEC §13.3, ADR 0012: every
/// launch seals an inference key and a separate admin key, so both
/// references are named (the coordinator resolves them per launch). ADR 0008:
/// drift is stated only when refused, so a default profile is unchanged.
pub fn profile_document(spec: &ProfileSpec) -> Value {
    let mut security = serde_json::json!({
        "deep_park": if spec.deep_park { "enabled" } else { "disabled" },
        "trust_remote_code": false,
        "credential_ref": "secret://engine-key",
        "admin_credential_ref": "secret://admin-key",
    });
    if spec.installation_drift == InstallationDrift::Refuse {
        security["installation_drift"] = "refuse".into();
    }
    let mut profile = serde_json::json!({
        "engine": match spec.engine { Engine::Vllm => "vllm", Engine::Sglang => "sglang" },
        "revision": 1,
        "executable": spec.executable.to_string_lossy(),
        "build_fingerprint": spec.build_fingerprint,
        "args": spec.args,
        "env": {},
        "log_policy": {"max_file_bytes": "16MiB", "retained_files": 3},
        "security": security,
    });
    if let Some(cuda_home) = &spec.cuda_home {
        profile["cuda_home"] = cuda_home.to_string_lossy().into();
    }
    profile
}

/// ADR 0018 §1, SPEC §15.3: a profile is written only if its name is valid
/// and it passes the rules deployment resolution applies.
pub fn check_profile(name: &str, profile: &Value) -> Result<(), ConfigError> {
    if !valid_profile_name(name) {
        return Err(ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            "runtime_profiles",
            format!("profile name {name:?} must be 1-64 lowercase letters, digits, '-' or '_', starting with a letter or digit"),
        ));
    }
    crate::effective::check_runtime_profile(profile)
}

fn without_profiles(document: &Value) -> Value {
    let mut copy = document.clone();
    if let Some(map) = copy.as_object_mut() {
        map.remove("runtime_profiles");
        if let Some(host) = map.get_mut("host").and_then(Value::as_object_mut) {
            host.remove("runtime_profiles");
        }
    }
    copy
}

fn profile_names(document: &Value) -> std::collections::BTreeSet<String> {
    document["runtime_profiles"]
        .as_object()
        .or_else(|| document["host"]["runtime_profiles"].as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// ADR 0018 §3: whether `new` differs from `old` in runtime profiles alone.
pub fn only_profiles_differ(old: &Value, new: &Value) -> bool {
    without_profiles(old) == without_profiles(new)
}

/// Profiles in `old` that `new` no longer has.
pub fn removed_profiles(old: &Value, new: &Value) -> Vec<String> {
    profile_names(old)
        .difference(&profile_names(new))
        .cloned()
        .collect()
}

/// Profiles in `new` that `old` did not have.
pub fn added_profiles(old: &Value, new: &Value) -> Vec<String> {
    profile_names(new)
        .difference(&profile_names(old))
        .cloned()
        .collect()
}

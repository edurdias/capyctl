//! ADR 0018 §2 (owner decision 2026-09-25): engine registration writes only
//! `engines.yaml`, an mllm-owned file beside the role's configuration file.
//! The host or standalone document is never rewritten; the role merges the
//! two at load, and a profile name declared in both is refused. Writes hold
//! `engines.yaml.lock`, go through a temporary file, sync and rename, and
//! record the revision on the first line (a comment the parser ignores).
use crate::{parse_strict, ConfigError, ConfigErrorCode, ConfigKind};
use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const ENGINES_FILE: &str = "engines.yaml";
pub const REVISION_HEADER: &str = "# mllm-document-revision: ";
/// The largest host document a publication carries
/// (`mllm_store::host_publication`, `config_json.len() > 32768` is refused).
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
    role_document
        .parent()
        .unwrap_or(Path::new("."))
        .join(ENGINES_FILE)
}

/// ADR 0018 §2: beside the role document named with `--config`; without one,
/// `<config home>/mllm/engines.yaml`, for a host and for standalone alike.
pub fn engines_path(role_document: Option<&Path>, config_home: &Path) -> PathBuf {
    match role_document {
        Some(document) => engines_beside(document),
        None => config_home.join("mllm").join(ENGINES_FILE),
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

    /// JSON-shaped YAML, as `mllm init` writes, under the revision line.
    pub fn render(&self, revision: u64) -> String {
        let body = serde_json::json!({"schema_version": 1, "kind": "engines", "runtime_profiles": self.profiles});
        format!(
            "{REVISION_HEADER}{revision}\n# Written by `mllm engine add` and `remove`. The role merges it with its own document.\n{}\n",
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
}

pub fn lock_engines(path: &Path) -> Result<EnginesLock, ConfigError> {
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
    file.lock().map_err(|e| io(&lock, e))?;
    Ok(EnginesLock {
        _file: file,
        path: path.to_path_buf(),
    })
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
    let dir = file.path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| io(dir, e))?;
    tmp.write_all(text.as_bytes())
        .map_err(|e| io(tmp.path(), e))?;
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o600))
        .map_err(|e| io(tmp.path(), e))?;
    tmp.as_file().sync_all().map_err(|e| io(tmp.path(), e))?;
    tmp.persist(&file.path)
        .map_err(|e| io(&file.path, e.error))?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io(dir, e))?;
    Ok(revision)
}

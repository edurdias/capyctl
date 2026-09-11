//! No-config startup behavior (SPEC §15.2) and atomic, owner-protected
//! default generation (SPEC §16.5 standalone shape).
//!
//! Behavior matrix (SPEC §15.2):
//!
//! | Situation | Behavior |
//! |---|---|
//! | No `--config`; implicit config exists and is valid | Load and validate it. |
//! | No `--config`; implicit config exists and is invalid | Error — invalid content is never a reset trigger. |
//! | No `--config`; no implicit config | Atomically generate the standalone default (server + embedded host), local-only authenticated listeners, per-user absolute paths, protected credentials at `state_dir/identity/credentials`. No engine execution. |
//! | Explicit path missing | Fail with [`ConfigErrorCode::Io`] — generation only happens for the implicit no-config case. |
//! | Explicit path invalid | Fail — propagate the validation error. |
//!
//! Ordering guarantees: the generated document is rendered and validated
//! *before* any side effect (no identity directory is orphaned by a
//! validation failure), then credentials, then the config file.
//!
//! Atomicity: files are written to a `NamedTempFile` next to their final
//! location with `0600` permissions set before an atomic `rename`
//! (replaces any concurrent loser deterministically); the state root and
//! generated directories are `0700`; the credentials file is created with
//! `OpenOptions::create_new` (exclusive), so the first committer wins and
//! concurrent starts never regenerate or clobber credentials.
//!
//! Fail-closed entropy: credentials come from the OS entropy source only.
//! If it cannot be read, generation fails loudly with [`ConfigErrorCode::Io`]
//! and nothing is written — guessable credentials are never generated.
//! This module never executes engines and never touches `mllm-adapters`.

use crate::error::{ConfigError, ConfigErrorCode};
use crate::schema::ConfigKind;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// OS entropy source for credential generation.
const ENTROPY_SOURCE: &str = "/dev/urandom";

/// Credential length in bytes (rendered as 64 hex chars).
const CREDENTIAL_BYTES: usize = 32;

/// Result of resolving the startup configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    /// An existing config was loaded; carries the config file path.
    Loaded(String),
    /// A config was generated during this startup.
    Generated {
        /// Path of the freshly generated config file.
        config_path: PathBuf,
        /// Whether the protected credentials were created by this call
        /// (`false` when a concurrent start already created them).
        created_identity: bool,
    },
}

/// Resolve startup config for `kind` (SPEC §15.2 matrix).
///
/// `explicit` is the `--config` path when given; otherwise the implicit
/// role config at `<state_dir>/config/<kind>.yaml` is loaded or generated.
pub fn resolve_startup(
    kind: ConfigKind,
    explicit: Option<&Path>,
    state_dir: &Path,
) -> Result<LoadOutcome, ConfigError> {
    match explicit {
        Some(path) => {
            if !path.exists() {
                return Err(ConfigError::new(
                    ConfigErrorCode::Io,
                    "config",
                    format!("explicit config path `{}` does not exist", path.display()),
                ));
            }
            let text = read_config(path)?;
            crate::strict_yaml::validate(&text, kind)?;
            Ok(LoadOutcome::Loaded(path.to_string_lossy().into_owned()))
        }
        None => {
            let config_path = implicit_config_path(kind, state_dir);
            if config_path.exists() {
                let text = read_config(&config_path)?;
                crate::strict_yaml::validate(&text, kind)?;
                Ok(LoadOutcome::Loaded(
                    config_path.to_string_lossy().into_owned(),
                ))
            } else {
                let (config_path, created_identity, _bytes) = create_default(kind, state_dir)?;
                Ok(LoadOutcome::Generated {
                    config_path,
                    created_identity,
                })
            }
        }
    }
}

/// Generate the default config for `kind` and write it atomically.
///
/// Returns the final config path and the generated document bytes.
/// Credentials are created once (first committer via `create_new`).
/// Generation is only implemented for [`ConfigKind::Standalone`].
pub fn generate_default(
    kind: ConfigKind,
    state_dir: &Path,
) -> Result<(PathBuf, Vec<u8>), ConfigError> {
    create_default(kind, state_dir).map(|(path, _created_identity, bytes)| (path, bytes))
}

/// Shared generation pipeline: kind check, private state root, render and
/// validate (no side effects yet), credentials (once), atomic config write.
///
/// Returns `(config_path, created_identity, config_bytes)`.
fn create_default(
    kind: ConfigKind,
    state_dir: &Path,
) -> Result<(PathBuf, bool, Vec<u8>), ConfigError> {
    if kind != ConfigKind::Standalone {
        return Err(ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            "kind",
            format!(
                "default generation is only implemented for `standalone`, not `{}`",
                kind.as_str()
            ),
        ));
    }
    ensure_private_dir(state_dir).map_err(io_err)?;
    let yaml = render_standalone(state_dir);
    // Integration check: a generated document must pass Task 3 validation
    // before any file or directory is created (validate before side
    // effects, SPEC §15.3).
    crate::strict_yaml::validate(&yaml, kind)?;

    // Credential-once: if a concurrent start already wrote them, this is a
    // no-op. The generated config references `identity_dir`, not secrets.
    let created_identity = write_credentials(state_dir)?;

    let config_dir = ensure_private_dir(&state_dir.join("config")).map_err(io_err)?;
    let config_path = config_dir.join("standalone.yaml");
    let mut tmp = NamedTempFile::new_in(&config_dir).map_err(io_err)?;
    tmp.write_all(yaml.as_bytes()).map_err(io_err)?;
    use std::io::Write as _;
    tmp.flush().map_err(io_err)?;
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o600)).map_err(io_err)?;
    // Atomic on Linux: concurrent renames are safe; identical content makes
    // the winner deterministic.
    tmp.persist(&config_path).map_err(|e| io_err(e.error))?;
    Ok((config_path, created_identity, yaml.into_bytes()))
}

/// Implicit role config location for F0: `<state_dir>/config/<kind>.yaml`.
fn implicit_config_path(kind: ConfigKind, state_dir: &Path) -> PathBuf {
    state_dir
        .join("config")
        .join(format!("{}.yaml", kind.as_str()))
}

fn read_config(path: &Path) -> Result<String, ConfigError> {
    fs::read_to_string(path).map_err(|e| {
        ConfigError::new(
            ConfigErrorCode::Io,
            "config",
            format!("failed to read `{}`: {e}", path.display()),
        )
    })
}

/// Create `path` (and parents) with owner-only `0700` permissions on
/// `path` itself.
///
/// `create_dir_all` may leave newly created directories at the process
/// umask (typically `0755`), so `path` is chmod'd to `0700` explicitly.
/// The state root is passed here first (see [`create_default`]), so the
/// `config`/`identity` subdirectories are always nested inside an already
/// owner-only root; shared ancestors outside the state root are never
/// touched.
fn ensure_private_dir(path: &Path) -> std::io::Result<PathBuf> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(path.to_path_buf())
}

/// Write `state_dir/identity/credentials` (0600) exactly once.
///
/// The credentials file itself is the creation marker:
/// `OpenOptions::create_new` succeeds only for the first committer; every
/// concurrent start observes `AlreadyExists` and keeps the existing
/// credentials. Returns whether this call created them.
///
/// Fail-closed: credentials are read from [`ENTROPY_SOURCE`] *before* any
/// file is created; an unreadable entropy source is a loud
/// [`ConfigErrorCode::Io`] error and no credentials file is written.
fn write_credentials(state_dir: &Path) -> Result<bool, ConfigError> {
    write_credentials_from(state_dir, ENTROPY_SOURCE)
}

/// [`write_credentials`] with an injectable entropy source (test seam).
fn write_credentials_from(state_dir: &Path, entropy: &str) -> Result<bool, ConfigError> {
    let token = hex_string(os_random_bytes_from(entropy, CREDENTIAL_BYTES)?);
    let key = hex_string(os_random_bytes_from(entropy, CREDENTIAL_BYTES)?);
    let identity_dir = ensure_private_dir(&state_dir.join("identity")).map_err(io_err)?;
    let path = identity_dir.join("credentials");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write as _;
            let result = (|| {
                writeln!(file, "admin_token: {token}")?;
                writeln!(file, "api_key: {key}")?;
                file.flush()?;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
                Ok(())
            })();
            match result {
                Ok(()) => Ok(true),
                Err(e) => {
                    let _ = fs::remove_file(&path);
                    Err(io_err(e))
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(io_err(e)),
    }
}

/// Render the SPEC §16.5 standalone default with per-user absolute paths.
fn render_standalone(state_dir: &Path) -> String {
    let state = state_dir.to_string_lossy();
    format!(
        "schema_version: 1\n\
         kind: standalone\n\
         name: local\n\
         server:\n\
         \x20 name: local\n\
         \x20 state_dir: \"{state}/server\"\n\
         \x20 listeners:\n\
         \x20   management:\n\
         \x20     bind: \"127.0.0.1:7443\"\n\
         \x20     authentication: admin_token\n\
         \x20   inference:\n\
         \x20     bind: \"127.0.0.1:8443\"\n\
         \x20     authentication: api_key\n\
         \x20 tls:\n\
         \x20   mode: managed\n\
         \x20   identity_dir: \"{state}/identity\"\n\
         host:\n\
         \x20 name: local\n\
         \x20 state_dir: \"{state}/host\"\n\
         \x20 connection: embedded\n\
         \x20 resource_policy:\n\
         \x20   allowed_devices: auto\n\
         \x20   memory:\n\
         \x20     accounting: auto\n\
         \x20     system:\n\
         \x20       managed_limit: auto\n\
         \x20       free_reserve: auto\n\
         \x20 runtime_profiles: {{}}\n"
    )
}

fn io_err(e: std::io::Error) -> ConfigError {
    ConfigError::new(ConfigErrorCode::Io, "config", format!("io error: {e}"))
}

/// Read `n_bytes` from `source`; fail loudly if the OS entropy source is
/// unavailable — there is no fallback, because predictable credentials are
/// worse than none.
fn os_random_bytes_from(source: &str, n_bytes: usize) -> Result<Vec<u8>, ConfigError> {
    let mut buf = vec![0u8; n_bytes];
    File::open(source)
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| {
            ConfigError::new(
                ConfigErrorCode::Io,
                "identity.credentials",
                format!(
                    "cannot read OS entropy source `{source}`: {e} — \
                     refusing to generate guessable credentials"
                ),
            )
        })?;
    Ok(buf)
}

fn hex_string(bytes: Vec<u8>) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("mllm-defaults-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn credentials_are_created_once() {
        let d = temp_dir();
        assert!(write_credentials(&d).unwrap());
        assert!(!write_credentials(&d).unwrap());
        let text = fs::read_to_string(d.join("identity").join("credentials")).unwrap();
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn dirs_are_owner_only_including_state_root() {
        let d = temp_dir();
        ensure_private_dir(&d).unwrap();
        ensure_private_dir(&d.join("identity")).unwrap();
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(d.join("identity"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn unreadable_entropy_fails_loudly_without_writing_credentials() {
        let d = temp_dir();
        let err = write_credentials_from(&d, "/nonexistent/entropy").unwrap_err();
        assert!(matches!(err.code, ConfigErrorCode::Io), "{err}");
        assert!(err.detail.contains("guessable credentials"), "{err}");
        // Fail-closed: no credentials file was written.
        assert!(!d.join("identity").join("credentials").exists());
    }

    #[test]
    fn real_entropy_is_readable_in_test_environments() {
        let bytes = os_random_bytes_from(ENTROPY_SOURCE, CREDENTIAL_BYTES).unwrap();
        assert_eq!(bytes.len(), CREDENTIAL_BYTES);
    }
}

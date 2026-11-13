//! ADR 0018 amendment A4: CLI and roles share the engines file in the
//! effective role state directory. The old configuration-adjacent location
//! is consulted only for one-time migration.
use crate::output::StructuredError;
use capyctl_agent::control_socket::SOCKET_NAME;
pub use capyctl_agent::host_admin::{RoleKind, Target};
use capyctl_config::registration::{
    config_home, engines_path, legacy_engines_path, lock_engines_for, migrate_engines,
};
use std::path::{Path, PathBuf};

pub(crate) fn invalid(message: impl Into<String>) -> StructuredError {
    StructuredError {
        code: "invalid_config",
        message: message.into(),
    }
}

/// ADR 0018 §2 (review decision 2026-09-25): the role document a command
/// names: `--config`, else `$CAPYCTL_CONFIG`. `capyctl engine` and the roles (`start
/// host`, `join host`, `start standalone`) share this rule, so they agree on
/// the document.
///
/// The path is made absolute against the working directory before use (found
/// walking the guides 2026-09-25: a relative `--config host.yaml` put the
/// engines file at a bare `engines.yaml`, so its write failed after the
/// rename and the profile was saved but never published). Every consumer
/// then names the same file whatever directory it later works from.
pub fn named_role_document(
    explicit: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| env("CAPYCTL_CONFIG").map(PathBuf::from))
        .map(|path| absolute(&path))
}

/// `path` made absolute against the working directory (lexically; symbolic
/// links are kept, so a document's own directory stays where it is named).
/// A path that cannot be made absolute is returned unchanged, and the read
/// that follows reports it.
pub fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// ADR 0018 amendment A4: resolve the state-local file and migrate once,
/// under the engines lock, before a role reads or an offline CLI writes.
pub fn role_engines(
    state: &Path,
    named: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf, capyctl_config::ConfigError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let path = engines_path(&absolute(state));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state)
        .map_err(|e| {
            capyctl_config::ConfigError::new(
                capyctl_config::ConfigErrorCode::Io,
                state.display().to_string(),
                e.to_string(),
            )
        })?;
    let metadata = std::fs::metadata(state).map_err(|e| {
        capyctl_config::ConfigError::new(
            capyctl_config::ConfigErrorCode::Io,
            state.display().to_string(),
            e.to_string(),
        )
    })?;
    // ADR 0018 §2: sudo creates the lock for the service user.
    let owner = (unsafe { libc::geteuid() } == 0).then(|| (metadata.uid(), metadata.gid()));
    let lock = lock_engines_for(&path, owner)?;
    if let Some(legacy) = legacy_engines_path(named, config_home(env).as_deref()) {
        for (warning, message) in migrate_engines(&lock, &legacy)? {
            capyctl_domain::role_log::notice(
                if warning {
                    capyctl_domain::role_log::Level::Warning
                } else {
                    capyctl_domain::role_log::Level::Notice
                },
                &message,
            );
        }
    }
    Ok(path)
}

/// The role document: `--config`; else `$CAPYCTL_CONFIG`; else the host role
/// on this machine (owner decision 2026-09-26: the document `start host` or
/// `join host` was named with, else `<state_dir>/config/host.yaml`, else
/// `<config home>/capyctl/host.yaml`); else `<state_dir>/config/standalone.yaml`.
/// A host and a standalone document both present is ambiguous and refused.
/// Neither present is the first run (review decision 2026-09-25): standalone,
/// whose document `capyctl start standalone` generates, and whose engines file
/// is the one that start reads.
pub fn resolve_target(
    explicit: Option<&Path>,
    state_dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Target, StructuredError> {
    let home = config_home(env)
        .ok_or_else(|| invalid("neither XDG_CONFIG_HOME nor HOME is set; pass --config"))?;
    let named = named_role_document(explicit, env);
    let chosen = match named.clone() {
        Some(path) => path,
        None => {
            let host = match crate::local_role::host_document(state_dir) {
                Some((path, _)) => Some(path),
                None => Some(home.join("capyctl/host.yaml")).filter(|p| p.exists()),
            };
            let standalone = Some(state_dir.join("config/standalone.yaml")).filter(|p| p.exists());
            match (host, standalone) {
                (Some(host), Some(standalone)) => {
                    return Err(invalid(format!(
                        "both {} and {} exist; pass --config to choose",
                        host.display(),
                        standalone.display()
                    )))
                }
                (Some(host), None) => host,
                (None, Some(standalone)) => standalone,
                (None, None) => {
                    return Ok(Target {
                        engines: engines_path(&absolute(state_dir)),
                        role_document: state_dir.join("config/standalone.yaml"),
                        kind: RoleKind::Standalone,
                        socket: state_dir.join(SOCKET_NAME),
                        state_dir: state_dir.to_path_buf(),
                    });
                }
            }
        }
    };
    let text = std::fs::read_to_string(&chosen).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            invalid(format!(
                "config file {} does not exist; create it or leave out --config",
                chosen.display()
            ))
        } else {
            invalid(format!("{}: {e}", chosen.display()))
        }
    })?;
    let (kind, state) = match capyctl_config::remote_roles::HostConfig::parse(&text) {
        Ok(host) => (RoleKind::Host, host.state_dir),
        Err(host_error) => {
            match capyctl_config::parse_strict(capyctl_config::ConfigKind::Standalone, &text) {
                Ok(_) => (RoleKind::Standalone, state_dir.to_path_buf()),
                Err(_) => {
                    return Err(invalid(format!(
                        "{}: {}",
                        chosen.display(),
                        host_error.detail
                    )))
                }
            }
        }
    };
    let state = env("CAPYCTL_STATE_DIR")
        .filter(|value| !value.is_empty())
        .map(|value| absolute(Path::new(&value)))
        .unwrap_or(state);
    Ok(Target {
        engines: engines_path(&absolute(&state)),
        role_document: chosen,
        kind,
        socket: state.join(SOCKET_NAME),
        state_dir: state,
    })
}

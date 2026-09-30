//! ADR 0018 §2 (owner decision 2026-09-25): which role document, engines
//! file and role socket an `capyctl engine` command acts on. The engines file
//! sits beside the role document named with `--config` (or `$CAPYCTL_CONFIG`);
//! otherwise it is `<config home>/capyctl/engines.yaml`, for a host and for
//! standalone alike, which is where the role looks too.
use crate::output::StructuredError;
use capyctl_agent::control_socket::SOCKET_NAME;
use capyctl_config::registration::{config_home, engines_beside, engines_path};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleKind {
    Host,
    Standalone,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub role_document: PathBuf,
    pub kind: RoleKind,
    pub engines: PathBuf,
    pub state_dir: PathBuf,
    pub socket: PathBuf,
}

pub(crate) fn invalid(message: impl Into<String>) -> StructuredError {
    StructuredError {
        code: "invalid_config",
        message: message.into(),
    }
}

/// ADR 0018 §2 (review decision 2026-09-25): the role document a command
/// names: `--config`, else `$CAPYCTL_CONFIG`. `capyctl engine` and the roles (`start
/// host`, `join host`, `start standalone`) share this rule, so they agree on
/// the document and on the engines file beside it.
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

/// ADR 0018 §2: a role's engines file: beside its named document, else
/// `<config home>/capyctl/engines.yaml`; `None` when neither `XDG_CONFIG_HOME`
/// nor `HOME` is set.
pub fn role_engines(named: Option<&Path>, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    match named {
        Some(document) => Some(engines_beside(document)),
        None => config_home(env).map(|home| engines_path(None, &home)),
    }
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
    let mut named = named_role_document(explicit, env);
    let chosen = match named.clone() {
        Some(path) => path,
        None => {
            let host = match crate::local_role::host_document(state_dir) {
                // A host started with a named document keeps its engines
                // file beside it, as when it is named here.
                Some((path, true)) => {
                    named = Some(path.clone());
                    Some(path)
                }
                Some((path, false)) => Some(path),
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
                        engines: engines_path(None, &home),
                        role_document: state_dir.join("config/standalone.yaml"),
                        kind: RoleKind::Standalone,
                        socket: state_dir.join(SOCKET_NAME),
                        state_dir: state_dir.to_path_buf(),
                    });
                }
            }
        }
    };
    let text = std::fs::read_to_string(&chosen)
        .map_err(|e| invalid(format!("{}: {e}", chosen.display())))?;
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
    Ok(Target {
        engines: engines_path(named.as_deref(), &home),
        role_document: chosen,
        kind,
        socket: state.join(SOCKET_NAME),
        state_dir: state,
    })
}

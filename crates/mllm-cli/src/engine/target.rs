//! ADR 0018 §2 (owner decision 2026-09-25): which role document, engines
//! file and role socket an `mllm engine` command acts on. The engines file
//! sits beside the role document named with `--config` (or `$MLLM_CONFIG`);
//! otherwise it is `<config home>/mllm/engines.yaml`, for a host and for
//! standalone alike, which is where the role looks too.
use crate::output::StructuredError;
use mllm_agent::control_socket::SOCKET_NAME;
use mllm_config::registration::{config_home, engines_path};
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

/// The role document: `--config`; else `$MLLM_CONFIG`; else
/// `<config home>/mllm/host.yaml` if it exists; else
/// `<state_dir>/config/standalone.yaml`. Both implicit documents present is
/// ambiguous and refused. Neither present is the first run (controller ruling
/// 2026-09-25): standalone, whose document `mllm start standalone` generates,
/// and whose engines file is the one that start reads.
pub fn resolve_target(
    explicit: Option<&Path>,
    state_dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Target, StructuredError> {
    let home = config_home(env)
        .ok_or_else(|| invalid("neither XDG_CONFIG_HOME nor HOME is set; pass --config"))?;
    let named = explicit
        .map(Path::to_path_buf)
        .or_else(|| env("MLLM_CONFIG").map(PathBuf::from));
    let chosen = match &named {
        Some(path) => path.clone(),
        None => {
            let host = Some(home.join("mllm/host.yaml")).filter(|p| p.exists());
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
    let (kind, state) = match mllm_config::remote_roles::HostConfig::parse(&text) {
        Ok(host) => (RoleKind::Host, host.state_dir),
        Err(host_error) => {
            match mllm_config::parse_strict(mllm_config::ConfigKind::Standalone, &text) {
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

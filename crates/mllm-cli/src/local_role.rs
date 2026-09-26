//! Owner decision 2026-09-26: a client command run on the machine where a
//! role runs uses that role without `--config`. On the server machine it is
//! the server, on a standalone machine the standalone role, and on a host
//! machine the host (whose own commands are `mllm engine` and `mllm config
//! show`; a command that needs the server says so plainly).
//!
//! The management API a client command (`list`, `status`, `deploy`, `start`,
//! `stop`, `drain`, `revoke`, `invite`, ...) uses, first match wins:
//!
//! 1. `--config <role document>`;
//! 2. `MLLM_CONFIG`;
//! 3. the role running on this machine, found under the state root from what
//!    it records there: a server's or standalone role's credentials, the
//!    management address it serves on, and the document a server or host was
//!    started with when `--config` or `MLLM_CONFIG` named one (as the
//!    packaged units do).
//!
//! When both a server and a standalone role keep their state under the same
//! root, the one that answers an authenticated read is used; if both or
//! neither answer, the command is refused naming both and how to choose. A
//! host has no management API, so on a machine that runs only a host (or
//! with a host document named) the command is refused: this is a host; run
//! this on the server.
//!
//! Every role serves its management API on loopback only, so another machine
//! is managed by running the command there.

use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::output::StructuredError;

/// How long auto-detection waits for each recorded role to answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// The management address a role serves on when nothing else names one.
const DEFAULT_MANAGEMENT: &str = "127.0.0.1:7443";

fn invalid(message: impl Into<String>) -> StructuredError {
    StructuredError {
        code: "invalid_config",
        message: message.into(),
    }
}

/// The refusal for a server command run where only a host runs.
fn host_refusal() -> StructuredError {
    invalid(
        "This machine is an mllm host; run this command on the server. A host has no \
         management API: deployments, hosts and invitations are managed where the server \
         (or a standalone role) runs",
    )
}

/// Where a target came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `--config <role document>`.
    Config,
    /// `MLLM_CONFIG`.
    Environment,
    /// The role running on this machine.
    Detected,
}

/// The management API a client command uses.
#[derive(Debug, Clone)]
pub struct Target {
    /// `http://<address>/management/v1`.
    pub endpoint: String,
    pub address: SocketAddr,
    pub token: String,
    /// Where the command's request journal lives.
    pub journal_root: PathBuf,
    /// `server` or `standalone`.
    pub role: &'static str,
    pub source: Source,
}

fn endpoint(address: SocketAddr) -> String {
    format!("http://{address}/management/v1")
}

/// The management API for this run: see the module documentation.
pub fn resolve(state_root: &Path, config: Option<&Path>) -> Result<Target, StructuredError> {
    let env = std::env::var("MLLM_CONFIG")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| crate::engine::absolute(Path::new(&value)));
    resolve_with(state_root, config, env.as_deref())
}

fn resolve_with(
    state_root: &Path,
    config: Option<&Path>,
    env: Option<&Path>,
) -> Result<Target, StructuredError> {
    if let Some(path) = config {
        return document_target(state_root, path, Source::Config);
    }
    if let Some(path) = env {
        return document_target(state_root, path, Source::Environment);
    }
    detect(state_root)
}

/// A named role document: a server's, or a standalone one. A host document
/// names no management API.
fn document_target(
    state_root: &Path,
    path: &Path,
    source: Source,
) -> Result<Target, StructuredError> {
    let kind = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| mllm_config::parse_document(&text).ok())
        .and_then(|document| document["kind"].as_str().map(str::to_owned));
    match kind.as_deref() {
        Some("standalone") => return standalone_target(state_root, source),
        Some("host") => return Err(host_refusal()),
        _ => {}
    }
    let server = crate::remote_roles::server_context(Some(path), state_root)?;
    let (endpoint, token) = crate::remote_roles::management_context(&server)?;
    let address = endpoint
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .and_then(|address| address.parse().ok())
        .unwrap_or(server.management);
    Ok(Target {
        endpoint,
        address,
        token,
        journal_root: server.state_dir,
        role: "server",
        source,
    })
}

fn standalone_credentials(state_root: &Path) -> PathBuf {
    state_root.join("identity/credentials")
}

fn server_credentials(state_root: &Path) -> PathBuf {
    state_root.join("identity/server-credentials.json")
}

/// The standalone role under `state_root`: its credentials and the address
/// it recorded (or `MLLM_MANAGEMENT_ADDR`, else its document's).
fn standalone_target(state_root: &Path, source: Source) -> Result<Target, StructuredError> {
    let credentials = std::fs::read_to_string(standalone_credentials(state_root))
        .map_err(|_| invalid("Standalone management credentials are unavailable"))?;
    let token = credentials
        .lines()
        .find_map(|line| line.strip_prefix("admin_token: "))
        .ok_or_else(|| invalid("Standalone management credential is missing"))?
        .to_owned();
    let address = crate::roles::standalone_management_address(state_root)
        .map_err(|failure| invalid(failure.to_string()))?;
    Ok(Target {
        endpoint: endpoint(address),
        address,
        token,
        journal_root: state_root.to_owned(),
        role: "standalone",
        source,
    })
}

/// The server on this machine: the document it recorded, else its implicit
/// document when there is one, else its credentials and recorded address.
fn server_target(state_root: &Path, source: Source) -> Result<Target, StructuredError> {
    if let Some(document) = recorded_document(state_root, "server") {
        return document_target(state_root, &document, source);
    }
    let implicit = state_root.join("config/server.yaml");
    if std::fs::symlink_metadata(&implicit).is_ok() {
        return document_target(state_root, &implicit, source);
    }
    let token = crate::remote_roles::server_admin_token(&state_root.join("identity"))?;
    let address = crate::roles::management_override(None)
        .map_err(|e| invalid(e.to_string()))?
        .or_else(|| crate::roles::recorded_management_address(state_root))
        .unwrap_or_else(|| DEFAULT_MANAGEMENT.parse().expect("valid default"));
    Ok(Target {
        endpoint: endpoint(address),
        address,
        token,
        journal_root: state_root.to_owned(),
        role: "server",
        source,
    })
}

/// The management role running on this machine, from what it recorded under
/// `state_root`.
fn detect(state_root: &Path) -> Result<Target, StructuredError> {
    let standalone = std::fs::symlink_metadata(standalone_credentials(state_root)).is_ok();
    let server = std::fs::symlink_metadata(server_credentials(state_root)).is_ok()
        || recorded_document(state_root, "server").is_some();
    match (standalone, server) {
        (true, false) => standalone_target(state_root, Source::Detected),
        (false, true) => server_target(state_root, Source::Detected),
        (false, false) if host_document(state_root).is_some() => Err(host_refusal()),
        (false, false) => Err(invalid(format!(
            "No mllm server or standalone role keeps its state under {} on this machine. \
             Run this command where the server runs, start a standalone role here \
             (`mllm start standalone`), or name the role document with --config <file> \
             (or MLLM_CONFIG)",
            state_root.display()
        ))),
        (true, true) => {
            // One probe per run: a command that asks twice (its request,
            // then host names for its table) gets the same answer.
            static PROBED: std::sync::Mutex<Option<(PathBuf, Target)>> =
                std::sync::Mutex::new(None);
            if let Some((root, target)) = PROBED.lock().ok().and_then(|p| p.clone()) {
                if root == state_root {
                    return Ok(target);
                }
            }
            let candidates = [
                standalone_target(state_root, Source::Detected),
                server_target(state_root, Source::Detected),
            ];
            let answering: Vec<&Target> = candidates
                .iter()
                .filter_map(|candidate| candidate.as_ref().ok())
                .filter(|candidate| answers(candidate))
                .collect();
            if let [only] = answering[..] {
                if let Ok(mut probed) = PROBED.lock() {
                    *probed = Some((state_root.to_owned(), only.clone()));
                }
                return Ok(only.clone());
            }
            let describe = |candidate: &Result<Target, StructuredError>, role: &str| match candidate
            {
                Ok(target) => format!("{role} at {}", target.address),
                Err(_) => format!("{role} (its credentials cannot be read)"),
            };
            Err(invalid(format!(
                "Both a {} and a {} keep their state under {}, and {}; choose one with \
                 --config <server document> for the server or --config {} for standalone \
                 (or MLLM_CONFIG)",
                describe(&candidates[1], "server"),
                describe(&candidates[0], "standalone role"),
                state_root.display(),
                if answering.is_empty() {
                    "neither answers"
                } else {
                    "both answer"
                },
                state_root.join("config/standalone.yaml").display(),
            )))
        }
    }
}

/// Whether `target` answers an authenticated read within the probe bound.
fn answers(target: &Target) -> bool {
    let url = format!("{}/snapshot", target.endpoint);
    let token = target.token.clone();
    // Detection runs before or inside the command's own runtime; a blocking
    // request on a plain thread serves both.
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async move {
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(PROBE_TIMEOUT)
                .build()
                .ok()?;
            let response = client.get(url).bearer_auth(token).send().await.ok()?;
            Some(response.status().is_success())
        })
    })
    .join()
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// Where a server (`start server`) or host (`start host`, `join host`)
/// records the role document `--config` or `MLLM_CONFIG` named, so commands
/// run later on this machine without it find the role.
fn recorded_path(state_root: &Path, role: &str) -> PathBuf {
    state_root.join("run").join(format!("{role}-document"))
}

/// The document `role` recorded under `state_root`, while it exists.
fn recorded_document(state_root: &Path, role: &str) -> Option<PathBuf> {
    std::fs::read_to_string(recorded_path(state_root, role))
        .ok()
        .map(|text| PathBuf::from(text.trim_end_matches('\n')))
        .filter(|path| path.is_absolute() && path.is_file())
}

/// A host role's document on this machine: the one `start host` or `join
/// host` recorded (`true`: it was named, so its `engines.yaml` sits beside
/// it), else the implicit `<state root>/config/host.yaml`.
pub fn host_document(state_root: &Path) -> Option<(PathBuf, bool)> {
    if let Some(path) = recorded_document(state_root, "host") {
        return Some((path, true));
    }
    Some(state_root.join("config/host.yaml"))
        .filter(|path| path.is_file())
        .map(|path| (path, false))
}

/// Record the document `role` (`server` or `host`) was named with (`None`:
/// its implicit one, and any earlier record is removed). `<state root>/run`
/// is 0700, the file 0600 and replaced atomically. A failure is reported,
/// never fatal: `--config` still names the document.
pub fn record_document(state_root: &Path, role: &str, named: Option<&Path>) {
    let path = recorded_path(state_root, role);
    let Some(document) = named else {
        let _ = std::fs::remove_file(&path);
        return;
    };
    let written = (|| -> std::io::Result<()> {
        let dir = path.parent().expect("a parent");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let temporary = dir.join(format!(".{role}-document.tmp"));
        let _ = std::fs::remove_file(&temporary);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        writeln!(file, "{}", document.display())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)
    })();
    if let Err(error) = written {
        eprintln!(
            "warning: could not record the {role} document in {} ({error}); commands run \
             without --config will not find it",
            path.display()
        );
    }
}

/// The document of `role` on this machine: the one it recorded, else its
/// implicit `<state root>/config/<role>.yaml`.
pub fn role_document(state_root: &Path, role: crate::grammar::Role) -> PathBuf {
    use crate::grammar::Role;
    let implicit = |name: &str| state_root.join("config").join(format!("{name}.yaml"));
    match role {
        Role::Server => {
            recorded_document(state_root, "server").unwrap_or_else(|| implicit("server"))
        }
        Role::Host => host_document(state_root)
            .map(|(path, _)| path)
            .unwrap_or_else(|| implicit("host")),
        Role::Standalone => implicit("standalone"),
    }
}

/// The role a role-level command (`config show`) acts on when neither
/// `--config`, `MLLM_CONFIG` nor `--role` names one: the only role whose
/// document or credentials are on this machine, with its document. `Ok(None)`
/// when there is none (the first run); refused, naming them, when there is
/// more than one.
pub fn detected_role(
    state_root: &Path,
) -> Result<Option<(crate::grammar::Role, PathBuf)>, StructuredError> {
    use crate::grammar::Role;
    let credentials = |path: PathBuf| std::fs::symlink_metadata(path).is_ok();
    let mut found: Vec<(Role, PathBuf)> = Vec::new();
    for (role, present) in [
        (Role::Server, credentials(server_credentials(state_root))),
        (Role::Host, false),
        (
            Role::Standalone,
            credentials(standalone_credentials(state_root)),
        ),
    ] {
        let document = role_document(state_root, role);
        if present || document.is_file() {
            found.push((role, document));
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(invalid(format!(
            "More than one mllm role keeps its state under {} ({}); choose one with \
             --role <server|host|standalone> or --config <file>",
            state_root.display(),
            found
                .iter()
                .map(|(role, _)| format!("{role:?}").to_lowercase())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn root() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("mllm-local-role-")
            .tempdir_in(std::env::var_os("HOME").expect("HOME"))
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    // T01: a named host document names no management API; the refusal says
    // plainly to run the command on the server.
    #[test]
    fn a_host_document_is_refused_as_a_host() {
        let dir = root();
        let host = dir.path().join("host.yaml");
        std::fs::write(&host, "schema_version: 1\nkind: host\nname: gpu\n").unwrap();
        let refused = resolve_with(dir.path(), Some(&host), None).unwrap_err();
        assert!(
            refused.message.contains("run this command on the server"),
            "{}",
            refused.message
        );
        let refused = resolve_with(dir.path(), None, Some(&host)).unwrap_err();
        assert!(refused.message.contains("mllm host"), "{}", refused.message);
    }

    // T01: a host started with a named document is found through its record;
    // the implicit document is found without one.
    #[test]
    fn the_host_document_is_the_recorded_one_else_the_implicit_one() {
        let dir = root();
        assert!(host_document(dir.path()).is_none());
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        let implicit = dir.path().join("config/host.yaml");
        std::fs::write(&implicit, "kind: host\n").unwrap();
        assert_eq!(host_document(dir.path()), Some((implicit.clone(), false)));
        let named = dir.path().join("etc-host.yaml");
        std::fs::write(&named, "kind: host\n").unwrap();
        record_document(dir.path(), "host", Some(&named));
        assert_eq!(host_document(dir.path()), Some((named, true)));
        record_document(dir.path(), "host", None);
        assert_eq!(host_document(dir.path()), Some((implicit, false)));
    }
}

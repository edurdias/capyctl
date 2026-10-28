//! ADR 0040 §10: host administration. The logic behind `capyctl engine`
//! (detect, add, add --custom, remove), `capyctl prune sources` and `capyctl
//! config show`, shared by the CLI on the box, which keeps its prompts, and
//! by a role's management routes. Nothing here prompts or prints to a
//! terminal; a caller that has an operator at a terminal asks there.
//!
//! The engines files are written only here (ADR 0018 §2, review decision
//! C1); the running role is asked to publish what was written through a
//! [`RoleChannel`]: the control socket for the CLI, the role's own control
//! handler in-process.
mod config;
mod custom;
mod engines;
mod model_store;

pub use config::{effective_config, ConfigLayers};
pub use custom::{
    confirm_custom, preview_custom, CustomEngine, CustomOperation, CustomPreview, ManifestInput,
    MAX_REMOTE_MANIFEST_BYTES,
};
pub use engines::{
    add_builtin, candidates, detected, preview_builtin, remove, BuiltinAdd, BuiltinApproval,
    BuiltinPreview, Toolchain,
};
pub use model_store::{list_copies, model_store, prune_copies};

use crate::control_socket::{request, ClientError, ControlRequest};
use capyctl_config::custom::CustomManifest;
use capyctl_config::registration::{
    custom_engines_beside, lock_engines_for, write_engines, EnginesFile,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// ADR 0040 §11: a confirmation whose digest no longer matches what the
/// host measures now, or whose typed profile name is not the profile's.
pub const CONFIRMATION_STALE: &str = "confirmation_stale";
/// ADR 0040 §10: a built-in add that names an executable path outside the
/// detected candidates, or allows remote code, without a previewed digest.
pub const CONFIRMATION_REQUIRED: &str = "confirmation_required";

/// How long publishing waits for the role (ADR 0018 §3).
pub const ADD_REPLY: Duration = Duration::from_secs(60);
/// review decision I4: how long `remove` waits for the role. The role
/// answers a retirement within its own bound (`RETIRE_BOUND`, 960 s, which
/// covers the server's 900 s drain window); this adds a clear margin for the
/// role's measuring and the socket, so the caller does not give up on a role
/// that is still about to answer.
pub const REMOVE_REPLY: Duration =
    Duration::from_secs(crate::host_control::RETIRE_BOUND.as_secs() + 240);

/// A refusal with its closed code, worded as `capyctl engine` words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAdminError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for HostAdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for HostAdminError {}

pub(crate) fn error(code: &'static str, message: impl Into<String>) -> HostAdminError {
    HostAdminError {
        code,
        message: message.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleKind {
    Host,
    Standalone,
}

/// ADR 0018 §2: the role document, engines file and role socket a host
/// administration operation acts on.
#[derive(Debug, Clone)]
pub struct Target {
    pub role_document: PathBuf,
    pub kind: RoleKind,
    pub engines: PathBuf,
    pub state_dir: PathBuf,
    pub socket: PathBuf,
}

/// ADR 0018 §3: how an operation asks the running role to publish, retire
/// or reload. The CLI speaks over the control socket ([`ControlSocket`]); a
/// role answering its own management routes calls its control handler. The
/// operations are generic over it, so an in-process channel whose answer is
/// `Send` keeps their futures `Send`.
pub trait RoleChannel: Sync {
    fn ask(
        &self,
        request: ControlRequest,
        timeout: Duration,
    ) -> impl Future<Output = Result<Value, ClientError>>;
}

/// The owner-only control socket at a path (ADR 0018 §3).
pub struct ControlSocket<'a>(pub &'a Path);

impl RoleChannel for ControlSocket<'_> {
    async fn ask(&self, ask: ControlRequest, timeout: Duration) -> Result<Value, ClientError> {
        request(self.0, &ask, timeout).await
    }
}

/// A role's reply code as a closed code.
fn closed(code: &str) -> &'static str {
    match code {
        "publish_rejected" => "publish_rejected",
        "profile_in_use" => "profile_in_use",
        "agent_unreachable" => "agent_unreachable",
        "profile_exists" => "profile_exists",
        "invalid_config" => "invalid_config",
        _ => "internal",
    }
}

/// ADR 0040 §11: the confirmation digest: SHA-256 over a domain label, a
/// NUL, and the canonical JSON of `fields` (object keys sorted, no
/// whitespace), as `sha256:<64 hex>`.
pub(crate) fn confirmation_digest(domain: &str, fields: &Value) -> String {
    fn canonical(value: &Value, out: &mut Vec<u8>) {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push(b'{');
                for (i, key) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend(serde_json::to_vec(key).expect("a string encodes"));
                    out.push(b':');
                    canonical(&map[key], out);
                }
                out.push(b'}');
            }
            Value::Array(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    canonical(item, out);
                }
                out.push(b']');
            }
            scalar => out.extend(serde_json::to_vec(scalar).expect("a scalar encodes")),
        }
    }
    let mut bytes = Vec::with_capacity(1024);
    bytes.extend_from_slice(domain.as_bytes());
    bytes.push(0);
    canonical(fields, &mut bytes);
    format!("sha256:{}", hex::encode(Sha256::digest(&bytes)))
}

/// The role document, strictly parsed (never written). A standalone document
/// that does not exist yet (the first run; start generates it) declares
/// nothing.
pub fn role_document(target: &Target) -> Result<Value, HostAdminError> {
    if target.kind == RoleKind::Standalone && !target.role_document.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(&target.role_document).map_err(|e| {
        error(
            "invalid_config",
            format!("{}: {e}", target.role_document.display()),
        )
    })?;
    let kind = match target.kind {
        RoleKind::Host => capyctl_config::ConfigKind::Host,
        RoleKind::Standalone => capyctl_config::ConfigKind::Standalone,
    };
    capyctl_config::parse_strict(kind, &text)
        .map_err(|e| error("invalid_config", format!("{}: {}", e.path, e.detail)))
}

/// ADR 0031 amendment A1: custom-engines.yaml beside the target's engines.yaml.
pub fn custom_engines(target: &Target) -> PathBuf {
    custom_engines_beside(&target.engines)
}

/// The capyctl-owned file `name` is registered in (engines.yaml or, ADR 0031
/// amendment A1, custom-engines.yaml), if any.
fn registered_in(target: &Target, name: &str) -> Result<Option<EnginesFile>, HostAdminError> {
    for path in [target.engines.clone(), custom_engines(target)] {
        let file = EnginesFile::load(&path).map_err(|e| error("invalid_config", e.detail))?;
        if file.profiles.contains_key(name) {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

/// Whether `name` is registered in engines.yaml or custom-engines.yaml, or
/// declared in the role document (an older host may have registered a
/// profile named `local`).
fn registered_or_declared(target: &Target, name: &str) -> Result<bool, HostAdminError> {
    Ok(registered_in(target, name)?.is_some() || declared_by_operator(target)?.contains_key(name))
}

/// ADR 0018 §1, ADR 0031 §1: `profile_exists`, across both capyctl-owned
/// files and the role document.
fn refuse_existing(target: &Target, name: &str) -> Result<(), HostAdminError> {
    if registered_or_declared(target, name)? {
        return Err(error(
            "profile_exists",
            format!("profile {name} exists; use --name, or remove it first"),
        ));
    }
    Ok(())
}

/// ADR 0018 §2: the profiles the operator declared in the role document.
pub fn declared_by_operator(
    target: &Target,
) -> Result<serde_json::Map<String, Value>, HostAdminError> {
    let document = role_document(target)?;
    let profiles = match target.kind {
        RoleKind::Host => &document["runtime_profiles"],
        RoleKind::Standalone => &document["host"]["runtime_profiles"],
    };
    Ok(profiles.as_object().cloned().unwrap_or_default())
}

/// The role document a write checks the profile set against: the host's,
/// none for standalone.
fn host_document(target: &Target) -> Result<Option<Value>, HostAdminError> {
    match target.kind {
        RoleKind::Host => Ok(Some(role_document(target)?)),
        RoleKind::Standalone => Ok(None),
    }
}

/// ADR 0031 §7: what `engine key` reads for a custom engine.
pub fn engine_key(manifest: &CustomManifest) -> &'static str {
    if manifest.uses_api_key() {
        "per-launch"
    } else {
        "none"
    }
}

/// review decision C1: who the role runs as, when this process is root and
/// the role's state directory belongs to another user: a new engines.yaml
/// goes to that user so the role can read it. `None` otherwise (the writer
/// owns it).
fn service_owner(target: &Target) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let meta = std::fs::symlink_metadata(&target.state_dir).ok()?;
    (meta.file_type().is_dir() && meta.uid() != 0).then(|| (meta.uid(), meta.gid()))
}

/// `file` (engines.yaml or custom-engines.yaml) without `name`, under its
/// lock, keeping its owner and mode.
fn write_without(
    target: &Target,
    file: &Path,
    name: &str,
) -> Result<u64, capyctl_config::ConfigError> {
    let lock = lock_engines_for(file, service_owner(target))?;
    let mut engines = EnginesFile::load(file)?;
    engines.profiles.remove(name);
    write_engines(&engines, &lock, None)
}

/// ADR 0018 §3: ask the running role to publish what was written to
/// `written` at `revision`; `out` is the operation's result.
pub async fn publish(
    channel: &impl RoleChannel,
    target: &Target,
    mut out: Value,
    revision: u64,
    written: &Path,
) -> Result<Value, HostAdminError> {
    match channel.ask(ControlRequest::Add, ADD_REPLY).await {
        Ok(reply) if reply["ok"] == true => {
            out["published"] = reply["published"].clone();
            Ok(out)
        }
        Ok(reply) => Err(error(
            closed(reply["code"].as_str().unwrap_or("")),
            format!(
                "{} (the profile is written at revision {revision} and shows as not published)",
                reply["message"].as_str().unwrap_or("refused")
            ),
        )),
        // Owner decision 2026-09-25 (first-run walk): no role is running,
        // which is the normal first run (standalone refuses to start with no
        // engine). The profile is saved and the role publishes it when it
        // starts, so this is a success with a notice, not an error. SPEC §8,
        // ADR 0018 §3.
        Err(ClientError::NotRunning(_)) => {
            out["published"] = json!("role_not_running");
            // Release live check 0.1.1: a role started with another
            // --state-dir or --config listens on another socket, so it looks
            // absent here. Say which socket was tried and how to reach it.
            out["notice"] = json!(format!(
                "saved to {} (revision {revision}); start capyctl (`{}`) to use it\n\
                 no capyctl answered on {}; if it is already running with another \
                 --state-dir or --config, restart it to publish this profile, and pass \
                 the same option to `capyctl engine`",
                written.display(),
                start_command(target),
                target.socket.display()
            ));
            Ok(out)
        }
        // A role is there but did not take or answer the request: that is a
        // fault the operator must look at.
        Err(e) => Err(error(
            "agent_unreachable",
            format!(
                "{e}; {} is written (revision {revision}); it takes effect when the role starts",
                written.display()
            ),
        )),
    }
}

fn start_command(target: &Target) -> &'static str {
    match target.kind {
        RoleKind::Standalone => "capyctl start standalone",
        RoleKind::Host => "capyctl start host",
    }
}

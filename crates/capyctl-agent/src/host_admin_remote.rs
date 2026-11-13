//! ADR 0040 §10: a host's half of host administration on a server. The
//! server carries one [`host_admin`] operation in a `HostAdminRequest`; the
//! host's session hands it to a [`HostAdminHandler`], which runs it on a task
//! of its own (never on the session loop) and answers with a
//! `HostAdminReply` naming the same request id.
use crate::control_socket::{ClientError, ControlHandler, ControlRequest};
use crate::engines::ScanRoots;
use crate::host_admin::{
    self, BuiltinAdd, BuiltinApproval, CustomEngine, CustomOperation, HostAdminError,
    ManifestInput, RoleChannel, Target, Toolchain,
};
use crate::host_control::HostControl;
use crate::installation::InstallationMeasurer;
use capyctl_config::effective::InstallationDrift;
use capyctl_config::model_settings::ModelOverrides;
use capyctl_config::toolchain::ToolchainSearch;
use capyctl_protocol::pb::{self, host_admin_request::Op};
use prost::Message;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// ADR 0040 §10: the closed code of a request that arrived past its
/// deadline; the host starts nothing then.
pub const DEADLINE_EXCEEDED: &str = "deadline_exceeded";
/// More operations than [`MAX_RUNNING`] at once on this host.
pub const HOST_ADMIN_BUSY: &str = "host_admin_busy";
/// The answer would not fit one host-to-server message.
pub const REPLY_TOO_LARGE: &str = "reply_too_large";
/// The most host administration operations a host runs at once.
pub const MAX_RUNNING: usize = 4;

pub type ReplyFuture = Pin<Box<dyn Future<Output = pb::HostAdminReply> + Send>>;

/// ADR 0040 §10: runs one host administration operation. A host whose
/// session has a handler declares `host_admin`; one without it never does.
pub trait HostAdminHandler: Send + Sync {
    fn handle(self: Arc<Self>, request: pb::HostAdminRequest) -> ReplyFuture;
}

/// ADR 0040 §9: pins model sources and sizes their weights on this host's
/// terms (its `model_sources` endpoint and token file).
pub trait SourcePinner: Send + Sync {
    fn pin_and_size(
        &self,
        sources: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Vec<pb::PinnedSource>> + Send + '_>>;
}

/// ADR 0018 §3: the host role's own control handler as the channel an
/// operation publishes, retires or reloads through.
pub struct InProcessRole(pub Arc<HostControl>);

impl RoleChannel for InProcessRole {
    async fn ask(&self, request: ControlRequest, _: Duration) -> Result<Value, ClientError> {
        Ok(self.0.handle(request).await)
    }
}

/// The host role's operations, over its own files and control handler.
pub struct HostAdminService {
    pub host_id: String,
    pub target: Target,
    /// Detection roots before a request's extra directories.
    pub roots: ScanRoots,
    pub toolchain: ToolchainSearch,
    pub cuda_home_env: Option<String>,
    pub measurer: Arc<InstallationMeasurer>,
    pub role: InProcessRole,
    /// The model settings the environment sets (`model_store`).
    pub model_env: ModelOverrides,
    pub pinner: Arc<dyn SourcePinner>,
}

impl HostAdminHandler for HostAdminService {
    fn handle(self: Arc<Self>, request: pb::HostAdminRequest) -> ReplyFuture {
        Box::pin(async move {
            let request_id = request.request_id.clone();
            let reply = if request.deadline_unix_ms <= capyctl_protocol::now_unix_ms() {
                refusal(DEADLINE_EXCEEDED, "the request arrived past its deadline")
            } else {
                match request.op {
                    Some(Op::PinAndSize(pin)) => pb::HostAdminReply {
                        pin_and_size: Some(pb::PinAndSizeResult {
                            sources: self.pinner.pin_and_size(pin.sources).await,
                        }),
                        ..Default::default()
                    },
                    Some(op) => match self.run(op).await {
                        Ok(value) => pb::HostAdminReply {
                            result_json: value.to_string(),
                            ..Default::default()
                        },
                        Err(e) => refusal(e.code, &e.message),
                    },
                    None => refusal("invalid_request", "the request names no operation"),
                }
            };
            bounded(pb::HostAdminReply {
                request_id,
                ..reply
            })
        })
    }
}

impl HostAdminService {
    async fn run(&self, op: Op) -> Result<Value, HostAdminError> {
        let toolchain = Toolchain {
            cuda_home_env: self.cuda_home_env.clone(),
            search: &self.toolchain,
        };
        match op {
            Op::Detect(detect) => Ok(host_admin::detected(&self.roots_with(detect.extra_dirs))),
            Op::PreviewBuiltin(engine) => {
                let request = self.builtin(engine)?;
                let preview =
                    host_admin::preview_builtin(&self.target, &request, &toolchain, &self.measurer)
                        .await?;
                Ok(
                    json!({"command": preview.command, "summary": preview.summary,
                    "digest": preview.digest}),
                )
            }
            Op::AddBuiltin(add) => {
                let request = self.builtin(add.engine.unwrap_or_default())?;
                let roots = self.roots_with(add.extra_dirs);
                // ADR 0040 §10: a previewed digest confirms; otherwise the
                // add must name a detected candidate.
                let approval = if add.digest.is_empty() {
                    BuiltinApproval::Detected(&roots)
                } else {
                    BuiltinApproval::Confirmed {
                        digest: &add.digest,
                        typed_name: &add.typed_name,
                    }
                };
                host_admin::add_builtin(
                    &self.target,
                    &request,
                    &toolchain,
                    approval,
                    &self.measurer,
                    &self.role,
                )
                .await
            }
            Op::Remove(remove) => {
                host_admin::remove(&self.target, &remove.name, remove.drain, &self.role).await
            }
            Op::PreviewCustom(spec) => {
                let request = self.custom(spec)?;
                let preview =
                    host_admin::preview_custom(&self.target, &request, &self.measurer).await?;
                Ok(
                    json!({"command": preview.command, "summary": preview.summary,
                    "digest": preview.digest}),
                )
            }
            Op::ConfirmCustom(confirm) => {
                let request = self.custom(confirm.engine.unwrap_or_default())?;
                host_admin::confirm_custom(
                    &self.target,
                    &request,
                    &confirm.digest,
                    &confirm.typed_name,
                    &self.measurer,
                    &self.role,
                )
                .await
            }
            Op::ModelStoreList(list) => {
                let store = host_admin::model_store(&self.target.role_document, &self.model_env)?;
                host_admin::list_copies(&store, &list.referenced.into_iter().collect())
            }
            Op::ModelStorePrune(prune) => {
                let store = host_admin::model_store(&self.target.role_document, &self.model_env)?;
                let referenced: BTreeSet<String> = prune.referenced.into_iter().collect();
                host_admin::prune_copies(&store, &referenced, prune.apply)
            }
            Op::PinAndSize(_) => unreachable!("answered by the pinner"),
        }
    }

    fn roots_with(&self, extra: Vec<String>) -> ScanRoots {
        ScanRoots {
            extra: extra.into_iter().map(PathBuf::from).collect(),
            ..self.roots.clone()
        }
    }

    fn builtin(&self, engine: pb::BuiltinEngine) -> Result<BuiltinAdd, HostAdminError> {
        Ok(BuiltinAdd {
            host_id: self.host_id.clone(),
            path: PathBuf::from(engine.path),
            name: (!engine.name.is_empty()).then_some(engine.name),
            deep_park: deep_park(&engine.deep_park)?,
            drift: drift(&engine.drift)?,
            args: engine.args,
            approved_options: engine.approved_options,
            approved_paths: engine
                .approved_paths
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            env: engine.env.into_iter().collect(),
            approved_env: engine.approved_env,
            remote_code: engine.remote_code,
        })
    }

    fn custom(&self, spec: pb::CustomEngineSpec) -> Result<CustomEngine, HostAdminError> {
        let operation = match spec.operation.as_str() {
            "add" => CustomOperation::Add,
            "change" => CustomOperation::Change,
            _ => return Err(invalid("operation must be add or change")),
        };
        Ok(CustomEngine {
            host_id: self.host_id.clone(),
            operation,
            name: spec.name,
            executable: PathBuf::from(spec.executable),
            manifest: ManifestInput::Remote(spec.manifest),
            deep_park: deep_park(&spec.deep_park)?,
            drift: drift(&spec.drift)?,
            env: spec.env.into_iter().collect(),
            approved_env: spec.approved_env,
        })
    }
}

fn invalid(message: &str) -> HostAdminError {
    HostAdminError {
        code: "invalid_config",
        message: message.to_owned(),
    }
}

fn deep_park(value: &str) -> Result<Option<bool>, HostAdminError> {
    match value {
        "" => Ok(None),
        "enabled" => Ok(Some(true)),
        "disabled" => Ok(Some(false)),
        _ => Err(invalid("deep_park must be enabled or disabled")),
    }
}

fn drift(value: &str) -> Result<InstallationDrift, HostAdminError> {
    match value {
        "" | "warn" => Ok(InstallationDrift::Warn),
        "refuse" => Ok(InstallationDrift::Refuse),
        _ => Err(invalid("drift must be warn or refuse")),
    }
}

/// A refusal with its closed code.
pub fn refusal(code: &str, message: &str) -> pb::HostAdminReply {
    pb::HostAdminReply {
        error_code: code.to_owned(),
        error_message: message.to_owned(),
        ..Default::default()
    }
}

/// An answer that fits one host-to-server message, or its refusal.
fn bounded(reply: pb::HostAdminReply) -> pb::HostAdminReply {
    // Room for the frame around the reply.
    if reply.encoded_len() + 64 <= capyctl_protocol::HOST_MESSAGE_BYTES {
        return reply;
    }
    pb::HostAdminReply {
        request_id: reply.request_id,
        ..refusal(
            REPLY_TOO_LARGE,
            "the answer exceeds one control-session message",
        )
    }
}

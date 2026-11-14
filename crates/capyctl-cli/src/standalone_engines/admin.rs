//! ADR 0040 §§10–11: management calls the shared operations in-process.
use super::StandaloneControl;
use capyctl_agent::{
    control_socket::{ClientError, ControlRequest},
    engines::ScanRoots,
    host_admin::{
        self, BuiltinAdd, BuiltinApproval, CustomEngine, CustomOperation, HostAdminError,
        ManifestInput, RoleChannel, Target, Toolchain,
    },
    installation::InstallationMeasurer,
};
use capyctl_config::effective::InstallationDrift;
use capyctl_management::host_admin::{Failure, HostAdminSource, Operation};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration,
};

pub struct StandaloneAdmin {
    pub control: Arc<StandaloneControl>,
    pub target: Target,
    pub owner: capyctl_controller::ownership::SharedCoordinatorState,
}

fn failure(error: HostAdminError) -> Failure {
    Failure {
        code: error.code.to_owned(),
        message: error.message,
    }
}
fn invalid(message: impl Into<String>) -> Failure {
    Failure {
        code: "invalid_request".into(),
        message: message.into(),
    }
}
fn parse<T: serde::de::DeserializeOwned>(body: Value) -> Result<T, Failure> {
    serde_json::from_value(body).map_err(|error| invalid(error.to_string()))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    deep_park: Option<bool>,
    drift: Option<String>,
    env: BTreeMap<String, String>,
    approved_env: Vec<String>,
}
fn drift(value: Option<&str>) -> Result<InstallationDrift, Failure> {
    match value {
        None | Some("warn") => Ok(InstallationDrift::Warn),
        Some("refuse") => Ok(InstallationDrift::Refuse),
        _ => Err(invalid("drift must be warn or refuse")),
    }
}
#[derive(Deserialize)]
struct Add {
    path: PathBuf,
    name: Option<String>,
    #[serde(default)]
    paths: Vec<PathBuf>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    approved_options: Vec<String>,
    #[serde(default)]
    approved_paths: Vec<PathBuf>,
    #[serde(default)]
    allow_remote_code: bool,
    #[serde(default)]
    preview: bool,
    digest: Option<String>,
    confirm_name: Option<String>,
    #[serde(flatten)]
    settings: Settings,
}
#[derive(Deserialize)]
struct Custom {
    operation: String,
    name: String,
    executable: PathBuf,
    manifest: String,
    digest: Option<String>,
    confirm_name: Option<String>,
    #[serde(flatten)]
    settings: Settings,
}

// ADR 0040 §10: the outer operation owns the mutation lock; asking the
// control handler recursively would deadlock on that same lock.
struct LockedChannel<'a>(&'a StandaloneControl);
impl RoleChannel for LockedChannel<'_> {
    async fn ask(&self, request: ControlRequest, _timeout: Duration) -> Result<Value, ClientError> {
        Ok(match request {
            ControlRequest::Add => self.0.reload().await,
            ControlRequest::Remove { profile, drain } => {
                self.0.retire_for_removal(&profile, drain).await
            }
            ControlRequest::List => self.0.list(),
        })
    }
}

impl HostAdminSource for StandaloneAdmin {
    fn execute(
        &self,
        host: String,
        operation: Operation,
    ) -> Pin<Box<dyn Future<Output = Result<Value, Failure>> + Send + '_>> {
        Box::pin(async move {
            if host != self.control.host_id && host != "standalone" {
                return Err(Failure {
                    code: "host_not_found".into(),
                    message: "No such host".into(),
                });
            }
            // ADR 0040 §10: serialize the entire write-and-publication operation
            // with control-socket mutations, not just its final reload.
            let _one = self.control.mutation.lock().await;
            let channel = LockedChannel(&self.control);
            let measurer = Arc::new(InstallationMeasurer::new());
            match operation {
                Operation::Detect { paths } => tokio::task::spawn_blocking(move || {
                    host_admin::detected(&ScanRoots::from_env(paths))
                })
                .await
                .map_err(|_| invalid("Engine detection failed")),
                Operation::Add(body) => {
                    let add: Add = parse(body)?;
                    let roots = ScanRoots::from_env(vec![]);
                    let request = BuiltinAdd {
                        host_id: self.control.host_id.clone(),
                        path: add.path,
                        name: add.name,
                        deep_park: add.settings.deep_park,
                        drift: drift(add.settings.drift.as_deref())?,
                        args: add.args,
                        approved_options: add.approved_options,
                        approved_paths: add.approved_paths,
                        env: add.settings.env,
                        approved_env: add.settings.approved_env,
                        remote_code: add.allow_remote_code,
                    };
                    let search = capyctl_config::toolchain::ToolchainSearch::default();
                    let toolchain = Toolchain {
                        cuda_home_env: std::env::var("CUDA_HOME").ok(),
                        search: &search,
                    };
                    if add.preview {
                        let preview = host_admin::preview_builtin(
                            &self.target,
                            &request,
                            &toolchain,
                            &measurer,
                        )
                        .await
                        .map_err(failure)?;
                        return Ok(
                            json!({"command":preview.command,"summary":preview.summary,"digest":preview.digest}),
                        );
                    }
                    // ADR 0040 §10 / KTD11: paths may aid preview, but may
                    // not turn an unconfirmed request into permission to run.
                    if !add.paths.is_empty()
                        && (add.digest.as_deref().unwrap_or_default().is_empty()
                            || add.confirm_name.as_deref().unwrap_or_default().is_empty())
                    {
                        return Err(Failure {
                            code: host_admin::CONFIRMATION_REQUIRED.into(),
                            message:
                                "extra search directories require a preview digest and typed name"
                                    .into(),
                        });
                    }
                    let approval = match (&add.digest, &add.confirm_name) {
                        (Some(digest), Some(typed_name)) => {
                            BuiltinApproval::Confirmed { digest, typed_name }
                        }
                        (None, None) => BuiltinApproval::Detected(&roots),
                        _ => {
                            return Err(invalid(
                                "digest and confirm_name must be supplied together",
                            ))
                        }
                    };
                    host_admin::add_builtin(
                        &self.target,
                        &request,
                        &toolchain,
                        approval,
                        &measurer,
                        &channel,
                    )
                    .await
                    .map_err(failure)
                }
                Operation::Remove { name, body } => {
                    let drain = body.get("drain").and_then(Value::as_bool).unwrap_or(false);
                    host_admin::remove(&self.target, &name, drain, &channel)
                        .await
                        .map_err(failure)
                }
                Operation::Preview(body) | Operation::Confirm(body) => {
                    let custom: Custom = parse(body)?;
                    let request = CustomEngine {
                        host_id: self.control.host_id.clone(),
                        operation: match custom.operation.as_str() {
                            "add" => CustomOperation::Add,
                            "change" => CustomOperation::Change,
                            _ => return Err(invalid("operation must be add or change")),
                        },
                        name: custom.name,
                        executable: custom.executable,
                        manifest: ManifestInput::Remote(custom.manifest),
                        deep_park: custom.settings.deep_park,
                        drift: drift(custom.settings.drift.as_deref())?,
                        env: custom.settings.env,
                        approved_env: custom.settings.approved_env,
                    };
                    if let Some(digest) = custom.digest {
                        let typed_name = custom
                            .confirm_name
                            .ok_or_else(|| invalid("confirm_name is required"))?;
                        host_admin::confirm_custom(
                            &self.target,
                            &request,
                            &digest,
                            &typed_name,
                            &measurer,
                            &channel,
                        )
                        .await
                        .map_err(failure)
                    } else {
                        let preview = host_admin::preview_custom(&self.target, &request, &measurer)
                            .await
                            .map_err(failure)?;
                        Ok(
                            json!({"command":preview.command,"summary":preview.summary,"digest":preview.digest}),
                        )
                    }
                }
                operation @ (Operation::ModelStore | Operation::Prune(_)) => {
                    let listing = matches!(operation, Operation::ModelStore);
                    let apply = match operation {
                        Operation::Prune(body) => {
                            body.get("apply").and_then(Value::as_bool).unwrap_or(false)
                        }
                        _ => false,
                    };
                    let owner = self.owner.clone();
                    let store = self
                        .control
                        .host
                        .models
                        .policy
                        .root(&self.control.host.models.models_root)
                        .to_path_buf();
                    tokio::task::spawn_blocking(move || {
                        // KTD11: compute references for this request, not from
                        // the last listing the browser happened to display.
                        let referenced = owner
                            .lock()
                            .map_err(|_| invalid("Cannot read model references"))?
                            .store()
                            .referenced_model_sources()
                            .map_err(|_| invalid("Cannot read model references"))?
                            .into_iter()
                            .collect();
                        if listing {
                            host_admin::list_copies(&store, &referenced).map_err(failure)
                        } else {
                            host_admin::prune_copies(&store, &referenced, apply).map_err(failure)
                        }
                    })
                    .await
                    .map_err(|_| invalid("Model-store operation failed"))?
                }
                Operation::Config => {
                    // ADR 0040 §10: standalone has no host.yaml. Its effective
                    // host is the running, synthesized document, not defaults
                    // from an unrelated absent host configuration file.
                    let document = self.control.host.document();
                    let empty = capyctl_config::setting_overrides::NamedLayer::default();
                    let overrides = capyctl_config::setting_overrides::SettingOverrides::none(
                        capyctl_config::ConfigKind::Host,
                    );
                    Ok(host_admin::effective_config(
                        capyctl_config::ConfigKind::Host,
                        &self.target.role_document,
                        Some(&document),
                        host_admin::ConfigLayers {
                            env: &empty,
                            flags: &empty,
                            overrides: &overrides,
                        },
                        std::env::var_os("HOME")
                            .as_deref()
                            .map(std::path::Path::new),
                        &self.target.state_dir,
                    ))
                }
            }
        })
    }
}

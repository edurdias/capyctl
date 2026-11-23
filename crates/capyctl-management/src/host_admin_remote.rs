//! ADR 0040 §§9–11: server operations execute on the chosen host, never on
//! the server's filesystem or with the server's source credentials.
use crate::{
    check::{PinFuture, SourceProvider},
    configuration::ConfigurationFailure,
    host_admin::{Failure, HostAdminSource, Operation},
};
use capyctl_controller::{
    agent_sessions::AgentSessions, host_admin_remote::request, ownership::SharedCoordinatorState,
};
use capyctl_protocol::pb::{self, host_admin_request::Op};
use capyctl_store::managed_configuration::HostTarget;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};

// KTD11 / ADR 0040 §10: an add is synchronous, with at least 150 seconds.
const ADD_DEADLINE: Duration = Duration::from_secs(180);
const READ_DEADLINE: Duration = Duration::from_secs(30);

pub struct RemoteHostAdmin {
    owner: SharedCoordinatorState,
    sessions: Arc<AgentSessions>,
}

enum Prepared {
    Config(Value),
    Remote {
        host: String,
        referenced: Vec<String>,
    },
}

impl RemoteHostAdmin {
    pub fn new(owner: SharedCoordinatorState, sessions: Arc<AgentSessions>) -> Arc<Self> {
        Arc::new(Self { owner, sessions })
    }

    async fn run(&self, selector: String, operation: Operation) -> Result<Value, Failure> {
        let owner = self.owner.clone();
        let config = matches!(operation, Operation::Config);
        let model_store = matches!(operation, Operation::ModelStore | Operation::Prune(_));
        let prepared = tokio::task::spawn_blocking(move || {
            let owner = owner.lock().map_err(|_| internal())?;
            let store = owner.store();
            let host = store
                .enrolled_hosts()
                .map_err(|_| internal())?
                .into_iter()
                .find(|host| {
                    !host.revoked && (host.host_id == selector || host.host_name == selector)
                })
                .ok_or_else(|| Failure {
                    code: "host_not_found".into(),
                    message: "No such host".into(),
                })?;
            if config {
                // ADR 0040 §10: the approved document is readable even offline
                // or on an older host; no message, server defaults or round trip.
                let publication = store
                    .host_publication(&host.host_id)
                    .map_err(|_| internal())?
                    .ok_or_else(|| Failure {
                        code: "observation_stale".into(),
                        message: "The host has not published its configuration".into(),
                    })?;
                let document: Value =
                    serde_json::from_str(&publication.config_json).map_err(|_| internal())?;
                let settings: Vec<Value> = capyctl_config::setting_overrides::leaves(&document)
                    .into_iter()
                    .map(|(path, value)| json!({"path":path,"value":value,"source":"yaml"}))
                    .collect();
                return Ok(Prepared::Config(json!({
                    "role":"host", "document":null, "settings":settings
                })));
            }
            // KTD11: references are recomputed for each listing or prune, not
            // accepted from a preceding browser listing or cached on the server.
            let referenced = if model_store {
                store.referenced_model_sources().map_err(|_| internal())?
            } else {
                Vec::new()
            };
            Ok(Prepared::Remote {
                host: host.host_id,
                referenced,
            })
        })
        .await
        .map_err(|_| internal())??;
        let (host, referenced) = match prepared {
            Prepared::Config(config) => return Ok(config),
            Prepared::Remote { host, referenced } => (host, referenced),
        };
        let op = to_wire(operation, referenced)?;
        online(&self.sessions, &host)?;
        let deadline = deadline(&op);
        // ADR 0017: the correlated helper gates capability, version and live
        // session before sending. ADR 0040 §10: exactly one attempt, no retry.
        let reply = request(&self.sessions, &host, op, deadline)
            .await
            .map_err(|reason| Failure {
                code: reason.code().to_owned(),
                message: "The host administration request could not be answered".into(),
            })?;
        answered(&reply)?;
        serde_json::from_str(&reply.result_json).map_err(|_| internal())
    }
}

impl HostAdminSource for RemoteHostAdmin {
    fn execute(
        &self,
        host: String,
        operation: Operation,
    ) -> Pin<Box<dyn Future<Output = Result<Value, Failure>> + Send + '_>> {
        Box::pin(self.run(host, operation))
    }
}

impl SourceProvider for RemoteHostAdmin {
    fn pin_and_size(&self, host: &HostTarget, sources: Vec<String>) -> PinFuture {
        let sessions = self.sessions.clone();
        let host = host.host_id.clone();
        Box::pin(async move {
            online(&sessions, &host).map_err(ConfigurationFailure::HostAdmin)?;
            // ADR 0040 §9: only the host interprets sources and supplies its
            // endpoint/token. Pinning may take as long as an engine add.
            let reply = request(
                &sessions,
                &host,
                Op::PinAndSize(pb::PinAndSize { sources }),
                ADD_DEADLINE,
            )
            .await
            .map_err(|reason| {
                ConfigurationFailure::HostAdmin(Failure {
                    code: reason.code().to_owned(),
                    message: "The host could not pin the model sources".into(),
                })
            })?;
            answered(&reply).map_err(ConfigurationFailure::HostAdmin)?;
            reply
                .pin_and_size
                .map(|pin| pin.sources)
                .ok_or(ConfigurationFailure::Internal)
        })
    }
}

fn internal() -> Failure {
    Failure {
        code: "internal".into(),
        message: "Host administration is unavailable".into(),
    }
}

fn online(sessions: &AgentSessions, host: &str) -> Result<(), Failure> {
    // ADR 0040 §10: offline is observation_stale even if the last session
    // predates capability reporting. Never wait for a request deadline.
    if sessions
        .inspect(host)
        .is_some_and(|session| session.online && session.reconciled)
    {
        Ok(())
    } else {
        Err(Failure {
            code: "observation_stale".into(),
            message: "The host has no reconciled control session".into(),
        })
    }
}

fn answered(reply: &pb::HostAdminReply) -> Result<(), Failure> {
    if reply.error_code.is_empty() {
        Ok(())
    } else {
        Err(Failure {
            code: reply.error_code.clone(),
            message: reply.error_message.clone(),
        })
    }
}

fn deadline(op: &Op) -> Duration {
    match op {
        Op::AddBuiltin(_)
        | Op::ConfirmCustom(_)
        | Op::PreviewBuiltin(_)
        | Op::PreviewCustom(_)
        | Op::Remove(_) => ADD_DEADLINE,
        _ => READ_DEADLINE,
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Settings {
    deep_park: Option<bool>,
    drift: String,
    env: HashMap<String, String>,
    approved_env: Vec<String>,
}

impl Settings {
    fn deep_park(&self) -> String {
        match self.deep_park {
            Some(true) => "enabled",
            Some(false) => "disabled",
            None => "",
        }
        .into()
    }
}

#[derive(Deserialize)]
struct Add {
    path: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    approved_options: Vec<String>,
    #[serde(default)]
    approved_paths: Vec<String>,
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
    executable: String,
    manifest: String,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    confirm_name: String,
    #[serde(flatten)]
    settings: Settings,
}

fn parse<T: serde::de::DeserializeOwned>(body: Value) -> Result<T, Failure> {
    serde_json::from_value(body).map_err(|_| Failure {
        code: "invalid_request".into(),
        message: "Invalid host administration request".into(),
    })
}

fn to_wire(operation: Operation, referenced: Vec<String>) -> Result<Op, Failure> {
    Ok(match operation {
        Operation::Detect { paths } => Op::Detect(pb::DetectEngines {
            extra_dirs: paths
                .into_iter()
                .map(|p| p.to_string_lossy().into())
                .collect(),
        }),
        Operation::Add(body) => {
            let add: Add = parse(body)?;
            if add.digest.is_some() != add.confirm_name.is_some() {
                return Err(Failure {
                    code: "invalid_request".into(),
                    message: "digest and confirm_name must be supplied together".into(),
                });
            }
            let engine = pb::BuiltinEngine {
                path: add.path,
                name: add.name,
                deep_park: add.settings.deep_park(),
                drift: add.settings.drift,
                args: add.args,
                approved_options: add.approved_options,
                approved_paths: add.approved_paths,
                env: add.settings.env,
                approved_env: add.settings.approved_env,
                remote_code: add.allow_remote_code,
            };
            if add.preview {
                Op::PreviewBuiltin(engine)
            } else {
                Op::AddBuiltin(pb::AddBuiltinEngine {
                    engine: Some(engine),
                    extra_dirs: add.paths,
                    digest: add.digest.unwrap_or_default(),
                    typed_name: add.confirm_name.unwrap_or_default(),
                })
            }
        }
        Operation::Remove { name, body } => Op::Remove(pb::RemoveEngine {
            name,
            drain: body["drain"].as_bool().unwrap_or(false),
        }),
        operation @ (Operation::Preview(_) | Operation::Confirm(_)) => {
            let confirm = matches!(operation, Operation::Confirm(_));
            let (Operation::Preview(body) | Operation::Confirm(body)) = operation else {
                unreachable!()
            };
            let custom: Custom = parse(body)?;
            let engine = pb::CustomEngineSpec {
                operation: custom.operation,
                name: custom.name,
                executable: custom.executable,
                manifest: custom.manifest,
                deep_park: custom.settings.deep_park(),
                drift: custom.settings.drift,
                env: custom.settings.env,
                approved_env: custom.settings.approved_env,
            };
            if confirm {
                Op::ConfirmCustom(pb::ConfirmCustomEngine {
                    engine: Some(engine),
                    digest: custom.digest,
                    typed_name: custom.confirm_name,
                })
            } else {
                Op::PreviewCustom(engine)
            }
        }
        Operation::ModelStore => Op::ModelStoreList(pb::ModelStoreList { referenced }),
        Operation::Prune(body) => Op::ModelStorePrune(pb::ModelStorePrune {
            referenced,
            apply: body["apply"].as_bool().unwrap_or(false),
        }),
        Operation::Config => unreachable!("configuration is read from the approved document"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // U9 / KTD11: HTTP settings retain their meaning across the protocol.
    #[test]
    fn builtin_settings_and_confirmation_are_carried_to_the_host() {
        let Op::AddBuiltin(add) = to_wire(
            Operation::Add(json!({
                "path":"/engines/vllm", "name":"chat", "paths":["/engines"],
                "args":["--limit-mm-per-prompt","{}"],
                "approved_options":["limit-mm-per-prompt"],
                "approved_paths":["/models"], "allow_remote_code":true,
                "digest":"preview-digest", "confirm_name":"chat",
                "deep_park":false, "drift":"refuse", "env":{"CUSTOM":"value"},
                "approved_env":["CUSTOM"]
            })),
            vec![],
        )
        .unwrap() else {
            panic!("expected builtin add")
        };
        assert_eq!(add.extra_dirs, ["/engines"]);
        assert_eq!(add.digest, "preview-digest");
        assert_eq!(add.typed_name, "chat");
        let engine = add.engine.unwrap();
        assert_eq!(engine.path, "/engines/vllm");
        assert_eq!(engine.name, "chat");
        assert_eq!(engine.deep_park, "disabled");
        assert_eq!(engine.drift, "refuse");
        assert_eq!(engine.args, ["--limit-mm-per-prompt", "{}"]);
        assert_eq!(engine.approved_options, ["limit-mm-per-prompt"]);
        assert_eq!(engine.approved_paths, ["/models"]);
        assert_eq!(engine.env["CUSTOM"], "value");
        assert_eq!(engine.approved_env, ["CUSTOM"]);
        assert!(engine.remote_code);
    }

    #[test]
    fn builtin_previews_and_partial_confirmations_do_not_become_adds() {
        let preview = to_wire(
            Operation::Add(json!({"path":"/engine","preview":true})),
            vec![],
        )
        .unwrap();
        assert!(matches!(preview, Op::PreviewBuiltin(_)));
        for body in [
            json!({"path":"/engine","digest":"digest"}),
            json!({"path":"/engine","confirm_name":"name"}),
        ] {
            assert_eq!(
                to_wire(Operation::Add(body), vec![]).unwrap_err().code,
                "invalid_request"
            );
        }
    }

    // U9 / KTD12: confirmation's digest and typed name are not interchangeable
    // with the profile name, and preview can never perform a confirmation.
    #[test]
    fn custom_settings_and_confirmation_are_carried_to_the_host() {
        let body = json!({
            "operation":"change", "name":"custom", "executable":"/engine",
            "manifest":"manifest", "digest":"digest", "confirm_name":"typed",
            "deep_park":true, "drift":"warn", "env":{"SETTING":"yes"},
            "approved_env":["SETTING"]
        });
        let Op::ConfirmCustom(confirm) = to_wire(Operation::Confirm(body.clone()), vec![]).unwrap()
        else {
            panic!("expected custom confirmation")
        };
        assert_eq!(confirm.digest, "digest");
        assert_eq!(confirm.typed_name, "typed");
        let spec = confirm.engine.unwrap();
        assert_eq!(spec.operation, "change");
        assert_eq!(spec.name, "custom");
        assert_eq!(spec.executable, "/engine");
        assert_eq!(spec.manifest, "manifest");
        assert_eq!(spec.deep_park, "enabled");
        assert_eq!(spec.drift, "warn");
        assert_eq!(spec.env["SETTING"], "yes");
        assert_eq!(spec.approved_env, ["SETTING"]);
        assert_eq!(
            to_wire(Operation::Preview(body), vec![]).unwrap(),
            Op::PreviewCustom(spec)
        );
    }

    #[test]
    fn detection_retirement_and_model_references_are_carried_to_the_host() {
        assert_eq!(
            to_wire(
                Operation::Detect {
                    paths: vec!["/extra".into()]
                },
                vec![]
            )
            .unwrap(),
            Op::Detect(pb::DetectEngines {
                extra_dirs: vec!["/extra".into()]
            })
        );
        assert_eq!(
            to_wire(
                Operation::Remove {
                    name: "chat".into(),
                    body: json!({"drain":true})
                },
                vec![]
            )
            .unwrap(),
            Op::Remove(pb::RemoveEngine {
                name: "chat".into(),
                drain: true
            })
        );
        let references = vec!["sources/current".to_owned()];
        assert_eq!(
            to_wire(Operation::ModelStore, references.clone()).unwrap(),
            Op::ModelStoreList(pb::ModelStoreList {
                referenced: references.clone()
            })
        );
        assert_eq!(
            to_wire(Operation::Prune(json!({"apply":true})), references.clone()).unwrap(),
            Op::ModelStorePrune(pb::ModelStorePrune {
                referenced: references,
                apply: true
            })
        );
    }

    // U9 / KTD11: the server's add budget must not inherit the short read budget.
    #[test]
    fn adds_and_confirmations_have_a_synchronous_budget_of_at_least_150_seconds() {
        for op in [
            Op::AddBuiltin(Default::default()),
            Op::ConfirmCustom(Default::default()),
            Op::PreviewBuiltin(Default::default()),
            Op::PreviewCustom(Default::default()),
        ] {
            assert_eq!(deadline(&op), Duration::from_secs(180));
        }
        assert_eq!(
            deadline(&Op::Detect(Default::default())),
            Duration::from_secs(30)
        );
        assert_eq!(
            deadline(&Op::Remove(Default::default())),
            Duration::from_secs(180)
        );
    }
}

//! ADR 0018 §3, §4: the host's answers to `capyctl engine add`, `remove` and
//! `list` over the control socket. Add re-reads host.yaml merged with its
//! engines.yaml and publishes it live; remove retires the profile on the
//! server and answers once the server confirmed it. review decision C1: the
//! role never writes engines.yaml (under the system units `/etc` is read-only
//! to it); the CLI writes it after the confirmation, then asks for the reload
//! that publishes the removal. Nothing here reaches an engine.
use crate::control_socket::{ControlHandler, ControlRequest};
use crate::journal::HostJournal;
use crate::profiles::ProfileSet;
use crate::session::{ProfileUpdates, PublishOutcome, RetireOutcome};
use capyctl_config::registration::{only_profiles_differ, removed_profiles};
use capyctl_config::remote_roles::HostConfig;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How long `add` waits for the server's verdict on a publication.
pub const PUBLISH_BOUND: Duration = Duration::from_secs(30);
/// How long `remove` waits for a terminal retirement answer (a drain stops
/// deployments first).
pub const RETIRE_BOUND: Duration = Duration::from_secs(960);

pub struct HostControl {
    /// The role's host.yaml.
    document: PathBuf,
    /// The role's engines.yaml, resolved by the same rule as `capyctl engine`
    /// (ADR 0018 §2).
    engines: PathBuf,
    /// The document the role started with. Only runtime profiles change live.
    running: HostConfig,
    updates: Arc<ProfileUpdates>,
    journal: Arc<HostJournal>,
    /// One add or remove at a time; `list` never waits for them.
    mutation: tokio::sync::Mutex<()>,
}

fn refused(code: &str, message: impl Into<String>) -> Value {
    json!({"ok": false, "code": code, "message": message.into()})
}

fn invalid(error: capyctl_config::ConfigError) -> Value {
    refused(
        "invalid_config",
        format!("{}: {}", error.path, error.detail),
    )
}

impl HostControl {
    pub fn new(
        document: PathBuf,
        engines: PathBuf,
        running: HostConfig,
        updates: Arc<ProfileUpdates>,
        journal: Arc<HostJournal>,
    ) -> Arc<Self> {
        Arc::new(Self {
            document,
            engines,
            running,
            updates,
            journal,
            mutation: tokio::sync::Mutex::new(()),
        })
    }

    /// The document on disk, measured against the accepted set's inventory.
    async fn measured(&self) -> Result<ProfileSet, Value> {
        // Owner decision 2026-09-25: the generic overrides the host started
        // with apply to the reloaded document too.
        let loaded =
            HostConfig::load_with_overrides(&self.document, &self.engines, &self.running.overrides)
                .map_err(invalid)?;
        // Owner decisions 2026-09-25: the models directory, the model-source
        // policy and the engine settings (runtime directory, port range and
        // the `local_engine` profiles) were resolved at start (flags,
        // environment, document, defaults) and change only with a restart,
        // like every setting outside runtime_profiles.
        let mut document = loaded.document;
        capyctl_config::engine_settings::carry_start_settings(
            &self.running.document,
            &mut document,
        );
        let config = HostConfig::parse(&document.to_string()).map_err(invalid)?;
        // ADR 0018 §3: everything outside runtime_profiles needs a restart.
        if !only_profiles_differ(&self.running.document, &config.document) {
            return Err(refused(
                "publish_rejected",
                "the document changed outside runtime_profiles; only runtime profiles change live; restart the role to apply the rest",
            ));
        }
        let base = self.updates.profiles().accepted().inventory.clone();
        tokio::task::spawn_blocking(move || ProfileSet::measure(config, &base))
            .await
            .map_err(|_| refused("internal", "measuring the profiles failed"))
    }

    /// Re-read the document and publish it if it changed (profiles only).
    async fn reload(&self) -> Value {
        let set = match self.measured().await {
            Ok(set) => set,
            Err(reply) => return reply,
        };
        let accepted = self.updates.profiles().accepted();
        if set.fingerprint == accepted.fingerprint {
            return json!({"ok": true, "published": "unchanged"});
        }
        let dropped = removed_profiles(&accepted.config.document, &set.config.document);
        match self.updates.publish(set, PUBLISH_BOUND).await {
            PublishOutcome::Accepted => json!({"ok": true, "published": "published"}),
            PublishOutcome::Rejected(reason) => refused("publish_rejected", reason),
            // ADR 0017: a server without live_profile_update; the next role
            // start publishes the document.
            PublishOutcome::RestartRequired => {
                json!({"ok": true, "published": "restart_required"})
            }
            PublishOutcome::NotConnected => self.hold_for_next_session(&dropped).await,
            PublishOutcome::SessionEnded => refused(
                "agent_unreachable",
                "the control session ended before the server answered; retry",
            ),
            PublishOutcome::Busy => refused(
                "publish_rejected",
                "another publication is in progress; retry",
            ),
        }
    }

    /// No session: the next one publishes the accepted set, so the document
    /// becomes the accepted set now. Owner decision 2026-09-25: a document
    /// that drops a published profile never does; only `remove` drops one,
    /// after the server confirms.
    async fn hold_for_next_session(&self, dropped: &[String]) -> Value {
        if !dropped.is_empty() {
            return refused(
                "agent_unreachable",
                format!(
                    "the host has no control session, and the document no longer declares {}; nothing was published",
                    dropped.join(", ")
                ),
            );
        }
        // Measured again: `publish` consumed the set it was given.
        let set = match self.measured().await {
            Ok(set) => set,
            Err(reply) => return reply,
        };
        let profiles = self.updates.profiles();
        let id = format!("local-{}", ulid::Ulid::new());
        if profiles.stage(&id, set).is_err() {
            return refused(
                "publish_rejected",
                "another publication is in progress; retry",
            );
        }
        profiles.settle(&id, true);
        json!({"ok": true, "published": "pending_session"})
    }

    /// Whether host.yaml itself (not engines.yaml) declares `profile`.
    fn declared_by_operator(&self, profile: &str) -> bool {
        std::fs::read_to_string(&self.document)
            .ok()
            .and_then(|text| {
                capyctl_config::parse_strict(capyctl_config::ConfigKind::Host, &text).ok()
            })
            .is_some_and(|document| document["runtime_profiles"].get(profile).is_some())
    }

    /// ADR 0018 §4, phase one for `capyctl engine remove`: retire `profile` on
    /// the server if this host publishes it. `{"retired": true}` once the
    /// server confirmed nothing on this host uses it; `{"retired": false}`
    /// when the host never published it. Nothing is written here (controller
    /// ruling C1): the CLI rewrites engines.yaml, then asks for a reload.
    async fn retire(&self, profile: &str, drain: bool) -> Value {
        // ADR 0018 §2: a profile the operator declared in host.yaml stays
        // theirs to edit; capyctl never removes it.
        if self.declared_by_operator(profile) {
            return refused(
                "invalid_config",
                format!(
                    "{profile} is declared in {}; edit that file and restart the role",
                    self.document.display()
                ),
            );
        }
        let published = self
            .updates
            .profiles()
            .accepted()
            .config
            .profiles
            .contains_key(profile);
        if !published {
            return json!({"ok": true, "retired": false});
        }
        // Owner decision 2026-09-25 (design rule 3): never without the
        // server's confirmation that nothing on this host uses it.
        match self.updates.retire(profile, drain, RETIRE_BOUND).await {
            RetireOutcome::Confirmed => json!({"ok": true, "retired": true}),
            RetireOutcome::InUse(deployments) | RetireOutcome::Holding(deployments) => {
                json!({"ok": false, "code": "profile_in_use", "deployments": deployments,
                    "message": "deployments on this host use the profile; stop them, or use --drain"})
            }
            RetireOutcome::Refused(reason) => refused("publish_rejected", reason),
            RetireOutcome::RestartRequired => refused(
                "publish_rejected",
                "the server does not support live profile updates; stop the deployments that use it and the role, remove the profile, then restart the role",
            ),
            RetireOutcome::NotConnected => refused(
                "agent_unreachable",
                "the host has no control session; nothing was removed",
            ),
            // review decision I4: the request may have reached the server.
            RetireOutcome::SessionEnded => refused(
                "agent_unreachable",
                format!("the control session ended, or the server did not answer in time, before the retirement of {profile} was confirmed; the outcome is unknown: run `capyctl engine list`, and `capyctl engine remove {profile}` again to finish (a retry resumes the same removal)"),
            ),
        }
    }

    fn list(&self) -> Value {
        let accepted = self.updates.profiles().accepted();
        let mut users = serde_json::Map::new();
        for claimed in self.journal.claimed_launches("").unwrap_or_default() {
            if let capyctl_protocol::execution::MemberAction::LaunchSingle(plan) =
                &claimed.command.action
            {
                let name = serde_json::from_str::<Value>(&plan.deployment_config)
                    .ok()
                    .and_then(|d| d["name"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| claimed.command.identity.deployment_id.clone());
                if let Some(list) = users
                    .entry(plan.profile_name.clone())
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                {
                    list.push(name.into());
                }
            }
        }
        let mut profiles = serde_json::Map::new();
        for status in &accepted.inventory.profiles {
            let probe = match accepted.installations.last_capabilities(&status.name) {
                Some(report) if report.available("deep_park") == Some(false) => {
                    "capability_missing"
                }
                Some(_) => "available",
                None => "unknown",
            };
            let declared = accepted
                .config
                .profiles
                .get(&status.name)
                .cloned()
                .unwrap_or(Value::Null);
            profiles.insert(
                status.name.clone(),
                json!({
                    "engine": declared["engine"], "executable": declared["executable"],
                    "build_fingerprint": status.build_fingerprint,
                    "installation": {"version": status.installation_version,
                        "digest": status.installation_digest, "state": status.installation_state},
                    "deep_park": declared["security"]["deep_park"], "deep_park_probe": probe,
                }),
            );
        }
        json!({"ok": true, "connected": self.updates.connected(),
            "live_profile_update": self.updates.server_supports(),
            "accepted": profiles, "users": users})
    }
}

#[async_trait::async_trait]
impl ControlHandler for HostControl {
    async fn handle(&self, request: ControlRequest) -> Value {
        match request {
            ControlRequest::Add => {
                let _one = self.mutation.lock().await;
                self.reload().await
            }
            ControlRequest::Remove { profile, drain } => {
                let _one = self.mutation.lock().await;
                self.retire(&profile, drain).await
            }
            ControlRequest::List => self.list(),
        }
    }
}

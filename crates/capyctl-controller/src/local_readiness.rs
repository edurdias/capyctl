//! SPEC §§4.3, 6.1, 13.2 (owner decision P3): readiness of adopted embedded
//! engines after a standalone role restart.
//!
//! A restarted standalone role adopts the Ready engines its previous run left
//! running (`coordinator::local_adoption`). Session start closed dispatch for
//! every deployment, and it stays closed until this supervisor proves, freshly and
//! locally, that the adopted engine is the recorded one and still serves:
//!
//! - every recorded process is alive with its recorded pid, start ticks and boot;
//! - the engine answers an authenticated model list naming the deployment's
//!   route, using the per-launch key sealed when it launched — a key only that
//!   launch was ever given, so an answer is from that engine and not from whatever
//!   else might hold the port;
//! - ADR 0030 §3: a decisions deployment also answers the fixed decision probe,
//!   complete and well-formed, on the same key (it generates no text, and its
//!   readiness is a decision, never a model list alone);
//! - every recorded process is still alive after the answer;
//! - ADR 0033 §3: a launch's LMCache server is alive, answers `GET /status`
//!   and listens on loopback only (an exposed one is stopped), and none is
//!   re-proved while the host's settings say `lmcache: off` or
//!   `retention: zero` (§§9, 10), so its dispatch stays closed.
//!
//! Only then does the store reopen dispatch, with a readiness receipt in the
//! journal. A failed proof leaves the deployment charged, closed and owned; an
//! operator Stop still terminates exactly the recorded group. Nothing here
//! restarts, releases or adopts an engine.
//!
//! W12, SPEC §10: request leases the crashed run left on an adopted launch keep
//! dispatch closed until, after a passing proof, the engine's own metrics show
//! nothing running or waiting; only then are they closed as abandoned.
use crate::ownership::SharedCoordinatorState;
use capyctl_domain::completion::{Presence, ProcessIdentity};
use capyctl_store::ordinary_lifecycle::local_recovery::LocalReadyLaunch;
use capyctl_store::ordinary_lifecycle::recovery::RemoteReadinessEvidence;
use capyctl_store::ordinary_lifecycle::retired_leases::QuiescenceEvidence;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

/// How often supervision looks for adopted launches awaiting proof.
const PASS_INTERVAL: Duration = Duration::from_millis(250);
/// The bound on one authenticated model-list probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// ADR 0030 §3: the bound on the fixed decision probe of an adopted
/// decisions launch (an engine's first decision can be slow).
const DECISION_TIMEOUT: Duration = Duration::from_secs(60);
/// ADR 0031 §7: how long an exposed custom engine's processes get on
/// SIGTERM before SIGKILL.
const EXPOSED_GRACE: Duration = Duration::from_secs(5);
/// The first wait after a failed proof, doubled per failure up to the cap.
const FIRST_RETRY: Duration = Duration::from_millis(500);
const MAX_RETRY: Duration = Duration::from_secs(60);

struct ProbeState {
    running: bool,
    next_at: Instant,
    backoff: Duration,
    reported: bool,
}

/// What a probe needs that the launch listing does not carry.
impl ProbeState {
    /// Record a finished probe. Returns whether a failure is to be journaled
    /// (the first of a run).
    ///
    /// Review finding 14: a proof the store accepted without reopening the gate
    /// (a switch or an engine exit closed it: W10 gap (a)) used to be probed
    /// again on the very next pass, four times a second, forever. A passing
    /// proof now backs off like a failing one; a launch whose gate did reopen
    /// no longer awaits proof, and its state is pruned on the next pass.
    fn settle(&mut self, passed: bool, now: Instant) -> bool {
        self.running = false;
        self.next_at = now + self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_RETRY);
        if passed || self.reported {
            return false;
        }
        self.reported = true;
        true
    }
}

struct Target {
    url: reqwest::Url,
    key: Option<String>,
    served: String,
    /// ADR 0030 §1: the API the deployment serves.
    decisions: bool,
    /// ADR 0031: a custom engine's manifest; its proof is the port, the
    /// health path, the API probe and the loopback check, never a model list.
    custom: Option<capyctl_config::custom::CustomProfile>,
}

pub struct LocalReadiness {
    owner: SharedCoordinatorState,
    client: reqwest::Client,
    probes: Mutex<BTreeMap<String, ProbeState>>,
    /// Review finding 15: every probe task, owned by the supervision loop.
    children: crate::supervised::Children,
    /// ADR 0033 §§9, 10: the standalone host's settings say `lmcache: off`
    /// or `retention: zero`; no launch with a server is re-proved.
    lmcache_forbidden: bool,
}

impl LocalReadiness {
    pub fn new(owner: SharedCoordinatorState) -> Arc<Self> {
        Self::on_host(owner, false)
    }

    /// ADR 0033 §§9, 10: `lmcache_forbidden` when the host's settings say
    /// `lmcache: off` or `retention: zero`.
    pub fn on_host(owner: SharedCoordinatorState, lmcache_forbidden: bool) -> Arc<Self> {
        Arc::new(Self {
            owner,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(PROBE_TIMEOUT)
                .build()
                .expect("a plain HTTP client builds"),
            probes: Mutex::new(BTreeMap::new()),
            children: Default::default(),
            lmcache_forbidden,
        })
    }

    /// Run supervision until the task is aborted. Aborting it aborts every
    /// probe it started.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        self.spawn_until(crate::supervised::never())
    }

    /// Run supervision until `cancel` reads true (or the task is aborted).
    /// On cancel, every probe in flight is aborted and joined before the task
    /// returns, so nothing it started still holds the coordinator's state.
    pub fn spawn_until(
        self: Arc<Self>,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let children = self.clone();
            let _abort = crate::supervised::OnDrop(move || children.children.abort_all());
            loop {
                let supervisor = self.clone();
                if tokio::task::spawn_blocking(move || supervisor.pass())
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::select! {
                    _ = crate::supervised::cancelled(&mut cancel) => break,
                    _ = tokio::time::sleep(PASS_INTERVAL) => {}
                }
            }
            self.children.join_all().await;
        })
    }

    /// One supervision pass: probe every launch awaiting proof, and forget
    /// the probe state of launches that no longer await one (review finding
    /// 13: the map was never pruned).
    pub fn pass(self: &Arc<Self>) {
        let launches = self.awaiting();
        if let Ok(mut probes) = self.probes.lock() {
            let awaiting: std::collections::BTreeSet<&str> =
                launches.iter().map(|l| l.binding_id.as_str()).collect();
            probes.retain(|binding, state| state.running || awaiting.contains(binding.as_str()));
        }
        for launch in launches {
            self.start_probe(launch);
        }
    }

    /// This session's Ready embedded launches whose dispatch is closed.
    fn awaiting(&self) -> Vec<LocalReadyLaunch> {
        let Ok(owner) = self.owner.lock() else {
            return Vec::new();
        };
        owner
            .store()
            .local_ready_launches(owner.session())
            .map(|launches| {
                launches
                    .into_iter()
                    .filter(|launch| !launch.dispatch_enabled)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn start_probe(self: &Arc<Self>, launch: LocalReadyLaunch) {
        let now = Instant::now();
        {
            let Ok(mut probes) = self.probes.lock() else {
                return;
            };
            let state = probes
                .entry(launch.binding_id.clone())
                .or_insert_with(|| ProbeState {
                    running: false,
                    next_at: now,
                    backoff: FIRST_RETRY,
                    reported: false,
                });
            if state.running || now < state.next_at {
                return;
            }
            state.running = true;
        }
        let supervisor = self.clone();
        self.children.track(tokio::spawn(async move {
            let outcome = supervisor.prove(&launch).await;
            let Ok(mut probes) = supervisor.probes.lock() else {
                return;
            };
            let Some(state) = probes.get_mut(&launch.binding_id) else {
                return;
            };
            let report = state.settle(outcome.is_ok(), Instant::now());
            drop(probes);
            if let (Err(reason), true) = (outcome, report) {
                if let Ok(owner) = supervisor.owner.lock() {
                    let _ = owner.store().record_journal(
                        None,
                        Some(&launch.operation_id),
                        Some("readiness_unproven"),
                        &format!(
                            "deployment {}: {reason}; dispatch stays closed and the \
                             engine stays owned",
                            launch.fence.deployment_id
                        ),
                    );
                }
            }
        }));
    }

    /// Where the adopted engine listens, the key it was launched with and the
    /// name it serves, read as one answer from the store.
    fn target(&self, launch: &LocalReadyLaunch) -> Result<Target, String> {
        let owner = self
            .owner
            .lock()
            .map_err(|_| "coordinator state unavailable".to_owned())?;
        let store = owner.store();
        let binding = store
            .retained_binding(&launch.binding_id)
            .map_err(|error| format!("the runtime binding is unreadable: {error}"))?
            .filter(|binding| {
                binding.id == launch.binding_id && binding.incarnation == launch.incarnation
            })
            .ok_or_else(|| "the recorded runtime binding changed".to_owned())?;
        let url = crate::port::engine_url(&binding.endpoint)
            .ok_or_else(|| "the recorded endpoint names no address".to_owned())?;
        let key = store
            .engine_key(
                &binding.id,
                &binding.incarnation,
                capyctl_store::secrets::SecretRole::Inference,
            )
            .map_err(|error| format!("the recorded engine key is unreadable: {error}"))?
            .map(hex::encode);
        let served = store
            .effective_routes(&launch.fence.deployment_id)
            .map_err(|error| format!("the deployment's routes are unreadable: {error}"))?
            .into_iter()
            .next()
            .ok_or_else(|| "the deployment serves no route".to_owned())?;
        // ADR 0030 §1: the frozen revision's API; omitted is chat, and a
        // value this build cannot read proves nothing.
        let effective = store
            .effective_configuration(&launch.fence.deployment_id)
            .map_err(|error| format!("the deployment's revision is unreadable: {error}"))?
            .map(|configuration| configuration.effective)
            .unwrap_or(serde_json::Value::Null);
        let decisions = match effective["api"].clone() {
            serde_json::Value::Null => false,
            api => !serde_json::from_value::<capyctl_config::effective::DeploymentApi>(api)
                .map_err(|_| "the deployment's api is unreadable".to_owned())?
                .is_chat(),
        };
        // ADR 0031: a custom engine's frozen manifest, re-validated.
        let custom = match effective["profile"]["engine"].as_str() {
            Some(engine) if engine == capyctl_config::engine_policy::Engine::Custom.name() => Some(
                capyctl_config::custom::CustomProfile::from_value(&effective["profile"]["custom"])
                    .map_err(|_| "the custom engine's manifest is unreadable".to_owned())?,
            ),
            _ => None,
        };
        Ok(Target {
            url,
            key,
            served,
            decisions,
            custom,
        })
    }

    /// ADR 0031 §5, §7: a custom engine's proof: the port accepts a
    /// connection, the health path answers 200 when stated, the API's own
    /// probe is answered with the launch's key, then every live owned
    /// process listens on loopback only. One that does not is stopped, with
    /// gone evidence, and the proof fails `custom_listener_exposed`.
    async fn prove_custom(
        &self,
        launch: &LocalReadyLaunch,
        target: &Target,
        custom: &capyctl_config::custom::CustomProfile,
    ) -> Result<(), String> {
        use capyctl_adapters::traits::{ChatForward, EngineAdapter, MemberRef, Readiness};
        let adapter = custom_adapter(target, custom);
        let member = MemberRef {
            deployment_id: launch.fence.deployment_id.clone(),
            member_id: launch.binding_id.clone(),
        };
        let ready = tokio::time::timeout(PROBE_TIMEOUT, adapter.check_readiness(&member))
            .await
            .map_err(|_| "the engine's port or health path did not answer in time".to_owned())?
            .map_err(|error| format!("the engine's readiness could not be read: {error}"))?;
        if ready != Readiness::Ready {
            return Err("the engine's port or health path is not answering".into());
        }
        if target.decisions {
            adapter
                .decision_probe(&target.served, DECISION_TIMEOUT)
                .await
                .map_err(|error| {
                    format!("the engine did not answer the decision probe: {error}")
                })?;
        } else {
            let body = serde_json::json!({
                "model": target.served,
                "messages": [{"role": "user", "content": "Say ready."}],
                "max_tokens": 8,
                "temperature": 0,
            });
            let answer = tokio::time::timeout(DECISION_TIMEOUT, adapter.forward_chat(&body))
                .await
                .map_err(|_| "the engine did not answer the chat probe in time".to_owned())?
                .map_err(|error| format!("the engine did not answer the chat probe: {error}"))?;
            if !capyctl_adapters::forward::probe_answered(&answer) {
                return Err("the engine answered the chat probe with empty content".into());
            }
        }
        let live = alive(&launch.identities)?;
        let checked = tokio::task::spawn_blocking(move || {
            capyctl_adapters::custom::check_loopback(std::path::Path::new("/proc"), &live, &|id| {
                capyctl_launchers::process_absence::presence(id) == Presence::Alive
            })
        })
        .await
        .map_err(|_| "the listener check did not run".to_owned())?;
        match checked {
            Ok(()) => Ok(()),
            Err(exposed @ capyctl_adapters::custom::ListenerCheck::Exposed(_)) => {
                let recorded = launch.identities.clone();
                let stopped = tokio::task::spawn_blocking(move || {
                    capyctl_launchers::owned_launch::terminate_recorded(&recorded, EXPOSED_GRACE)
                })
                .await
                .map_err(|_| format!("{exposed}; the stop did not run"))?;
                Err(match stopped {
                    Ok(_) => format!("{exposed}; the engine was stopped"),
                    Err(error) => format!("{exposed}; the stop is unproven: {error}"),
                })
            }
            Err(unreadable) => Err(unreadable.to_string()),
        }
    }

    /// ADR 0033 §§3, 9, 10 (lifecycle design §5): re-prove the launch's
    /// LMCache server (leader alive, `GET /status` answering, its group on
    /// loopback; an exposed one is stopped), unless the host's settings
    /// forbid LMCache.
    async fn prove_lmcache(
        &self,
        launch: &LocalReadyLaunch,
        server: &[ProcessIdentity],
    ) -> Result<(), String> {
        if self.lmcache_forbidden {
            return Err("the host's settings say `lmcache: off` or `retention: zero`; \
                        the launch's LMCache server is not re-proved"
                .into());
        }
        let grant = {
            let owner = self
                .owner
                .lock()
                .map_err(|_| "coordinator state unavailable".to_owned())?;
            owner
                .store()
                .launch_lmcache_grant(&launch.binding_id)
                .map_err(|error| format!("the LMCache grant is unreadable: {error}"))?
                .ok_or_else(|| "the launch records a server but no LMCache grant".to_owned())?
        };
        // What the proof reads: the grant's ports and the host's `/proc`.
        // Nothing is started, so the start fields stay empty.
        let plan = capyctl_adapters::lmcache::ServerPlan {
            lmcache_bin: std::path::PathBuf::new(),
            rpc_port: grant.rpc_port,
            http_port: grant.http_port,
            pool_gib: u64::try_from(grant.pool_bytes >> 30).unwrap_or(0),
            path: String::new(),
            home: None,
            cuda_visible_devices: None,
            log: None,
            config_dir: std::path::PathBuf::new(),
            start_bound: Duration::from_millis(capyctl_config::lmcache::SERVER_START_BOUND_MS),
            proc_root: std::path::PathBuf::from("/proc"),
            required: false,
        };
        let tools: Arc<dyn capyctl_adapters::traits::OwnedProcessLaunch> = Arc::new(
            capyctl_launchers::DurableProcessLaunch::new(Arc::new(ProveOnly)),
        );
        capyctl_adapters::lmcache::prove_server(tools, &plan, server)
            .await
            .map(|_| ())
            .map_err(|(reason, detail)| {
                format!(
                    "the LMCache server was not re-proved ({}): {detail}",
                    reason.code()
                )
            })
    }

    async fn prove(&self, launch: &LocalReadyLaunch) -> Result<(), String> {
        alive(&launch.identities)?;
        let target = self.target(launch)?;
        // ADR 0033 §3: the server first, so an engine connected to a server
        // that is gone or exposed is never re-admitted.
        let server = capyctl_domain::completion::lmcache_members(&launch.identities);
        if !server.is_empty() {
            self.prove_lmcache(launch, &server).await?;
        }
        match &target.custom {
            Some(custom) => self.prove_custom(launch, &target, custom).await?,
            None => self.prove_listed(&target).await?,
        }
        // SPEC §6.1: the answer must come from the recorded group, so the group is
        // proven alive on both sides of it.
        let live = alive(&launch.identities)?;
        let observed_at_ms = capyctl_protocol::now_unix_ms();
        self.settle_retired_leases(launch, &target, observed_at_ms)
            .await?;
        let evidence =
            RemoteReadinessEvidence {
                binding_id: launch.binding_id.clone(),
                incarnation: launch.incarnation.clone(),
                identities: live,
                observed_at_ms,
                receipt: match &target.custom {
                    Some(_) => format!(
                    "every recorded engine process alive with its recorded start ticks and boot; \
                     the custom engine's port and health answered, it answered the {} probe for \
                     {}, and every owned listener is on loopback",
                    if target.decisions { "fixed decision" } else { "chat" },
                    target.served,
                ),
                    None => format!(
                    "every recorded engine process alive with its recorded start ticks and boot; \
                     the engine answered an authenticated model list naming {}{}",
                    target.served,
                    if target.decisions {
                        " and the fixed decision probe"
                    } else {
                        ""
                    }
                ),
                },
            };
        let owner = self
            .owner
            .lock()
            .map_err(|_| "coordinator state unavailable".to_owned())?;
        owner
            .store()
            .reverify_local_dispatch(
                owner.session(),
                &launch.step_id,
                &evidence,
                capyctl_protocol::now_unix_ms(),
            )
            .map_err(|error| format!("readiness evidence was refused: {error}"))
    }

    /// The known engines' proof: an authenticated model list naming the
    /// route, and for a decisions deployment the fixed decision.
    async fn prove_listed(&self, target: &Target) -> Result<(), String> {
        let mut request = self.client.get(
            target
                .url
                .join("v1/models")
                .map_err(|_| "the model list URL is invalid")?,
        );
        if let Some(key) = &target.key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|_| "the engine did not answer the model probe".to_owned())?;
        if !response.status().is_success() {
            return Err(format!(
                "the engine refused the authenticated model probe ({})",
                response.status()
            ));
        }
        let listed: serde_json::Value = response
            .json()
            .await
            .map_err(|_| "the engine's model list is not JSON".to_owned())?;
        let serves = listed["data"]
            .as_array()
            .is_some_and(|models| models.iter().any(|model| model["id"] == target.served));
        if !serves {
            return Err("the engine's model list does not name the deployment's route".into());
        }
        // ADR 0030 §3, SPEC §6.1: a decision model is usable only once it
        // answers the fixed decision; the model list alone is not readiness.
        if target.decisions {
            capyctl_adapters::decision_probe::probe_keyed(
                &self.client,
                &target.url,
                target.key.as_deref(),
                &target.served,
                DECISION_TIMEOUT,
            )
            .await
            .map_err(|error| format!("the engine did not answer the decision probe: {error}"))?;
        }
        Ok(())
    }

    /// SPEC §10 (W12): a standalone crash with requests in flight left their
    /// leases on the adopted launch. The process that forwarded them is gone,
    /// but the engine may still be working on them. They close as abandoned
    /// only once the engine's own metrics, read with its per-launch key after
    /// the fresh model check, show nothing running or waiting.
    async fn settle_retired_leases(
        &self,
        launch: &LocalReadyLaunch,
        target: &Target,
        probed_at: i64,
    ) -> Result<(), String> {
        let retired = {
            let owner = self
                .owner
                .lock()
                .map_err(|_| "coordinator state unavailable".to_owned())?;
            owner
                .store()
                .retired_request_leases(owner.session(), &launch.step_id)
                .map_err(|error| format!("retired request leases are unreadable: {error}"))?
        };
        if retired == 0 {
            return Ok(());
        }
        let unobserved = || {
            format!(
                "{retired} request lease(s) of the retired session stay charged: the engine \
                 was not observed quiescent"
            )
        };
        let receipt = match &target.custom {
            // ADR 0031 §6: a custom engine's only account of its work is its
            // manifest's idle signal, read now with its key. Without one the
            // leases are never settled while the engine runs: they stay
            // charged until the verified exit of its next stop (a role
            // restart is not one), and the store keeps dispatch closed
            // while they remain.
            Some(custom) => {
                use capyctl_adapters::traits::{EngineAdapter, EngineWork, MemberRef};
                if custom.manifest.idle.is_none() {
                    return Ok(());
                }
                let member = MemberRef {
                    deployment_id: launch.fence.deployment_id.clone(),
                    member_id: launch.binding_id.clone(),
                };
                if custom_adapter(target, custom)
                    .idle_before_signal(&member)
                    .await
                    != Some(EngineWork::Idle)
                {
                    return Err(unobserved());
                }
                "the custom engine's idle signal, read with its per-launch key after a fresh \
                 probe, reported no running and no waiting requests"
            }
            None => {
                let mut request = self.client.get(
                    target
                        .url
                        .join("metrics")
                        .map_err(|_| "the metrics URL is invalid")?,
                );
                if let Some(key) = &target.key {
                    request = request.bearer_auth(key);
                }
                let response = request.send().await.map_err(|_| unobserved())?;
                if !response.status().is_success() {
                    return Err(unobserved());
                }
                let body = response.bytes().await.map_err(|_| unobserved())?;
                if body.len() > capyctl_agent::load::MAX_METRICS_BYTES {
                    return Err(unobserved());
                }
                // Missing or ambiguous gauges are unknown load, never zero.
                let load = std::str::from_utf8(&body)
                    .ok()
                    .and_then(capyctl_agent::load::parse_engine_load)
                    .ok_or_else(unobserved)?;
                if load.running != 0 || load.waiting != 0 {
                    return Err(unobserved());
                }
                "the engine's metrics, read with its per-launch key after a fresh model \
                 check, reported 0 running and 0 waiting requests"
            }
        };
        // The quiescent engine is still the recorded group.
        let live = alive(&launch.identities)?;
        let evidence = QuiescenceEvidence {
            binding_id: launch.binding_id.clone(),
            incarnation: launch.incarnation.clone(),
            identities: live,
            readiness_observed_at_ms: probed_at,
            quiescent_at_ms: capyctl_protocol::now_unix_ms(),
            receipt: receipt.into(),
        };
        let owner = self
            .owner
            .lock()
            .map_err(|_| "coordinator state unavailable".to_owned())?;
        owner
            .store()
            .abandon_retired_request_leases(
                owner.session(),
                &launch.step_id,
                &evidence,
                capyctl_protocol::now_unix_ms(),
            )
            .map(|_| ())
            .map_err(|error| format!("quiescence evidence was refused: {error}"))
    }
}

/// ADR 0031: an adapter that can only question an adopted custom engine,
/// with its manifest's API, health path and idle signal and its sealed key.
fn custom_adapter(
    target: &Target,
    custom: &capyctl_config::custom::CustomProfile,
) -> capyctl_adapters::custom::CustomAdapter {
    capyctl_adapters::custom::CustomAdapter::new(
        target.url.clone(),
        custom.manifest.build_fingerprint(),
        target.served.clone(),
        target.key.clone(),
    )
    .with_api(custom.manifest.api)
    .with_health(custom.manifest.health.clone())
    .with_idle(custom.manifest.idle.clone())
}

/// The re-proof never launches: it only observes the recorded server and
/// stops an exposed one.
struct ProveOnly;

impl capyctl_launchers::LaunchAssociation for ProveOnly {
    fn persist_api_identity(
        &self,
        _identity: &ProcessIdentity,
    ) -> Result<(), capyctl_launchers::AssociationError> {
        Err(capyctl_launchers::AssociationError::Uncertain(
            "a readiness proof never launches".into(),
        ))
    }
}

/// Every recorded engine process is alive as recorded; `Unknown` is not alive.
/// Returns the recorded processes alive now. ADR 0027: a helper may have exited
/// (an idle compile worker does); it is left out rather than refused.
fn alive(identities: &[ProcessIdentity]) -> Result<Vec<ProcessIdentity>, String> {
    if identities.is_empty() {
        return Err("the launch recorded no processes".into());
    }
    let mut live = Vec::new();
    for identity in identities {
        match capyctl_launchers::process_absence::presence(identity) {
            Presence::Alive => live.push(identity.clone()),
            _ if identity.is_helper() => {}
            Presence::Gone => {
                return Err(format!(
                    "recorded {} process {} is gone",
                    identity.role, identity.pid
                ))
            }
            Presence::Unknown => {
                return Err(format!(
                    "recorded {} process {} could not be observed",
                    identity.role, identity.pid
                ))
            }
        }
    }
    Ok(live)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn supervisor() -> (tempfile::TempDir, Arc<LocalReadiness>) {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = Arc::new(Mutex::new(
            crate::ownership::OwnedCoordinatorState::open(state.path()).unwrap(),
        ));
        (state, LocalReadiness::new(owner))
    }

    fn idle(next_at: Instant) -> ProbeState {
        ProbeState {
            running: false,
            next_at,
            backoff: FIRST_RETRY,
            reported: false,
        }
    }

    /// A child that runs until aborted and records that it was dropped.
    fn child(dropped: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        tokio::spawn(async move {
            let _flag = Flag(dropped);
            std::future::pending::<()>().await
        })
    }

    // SPEC §§4.3, 13.2 (review finding 13): the probe state of a launch that
    // no longer awaits proof is forgotten on the next pass, so the map is
    // bounded by the launches still awaiting one.
    // T20
    #[tokio::test]
    async fn a_pass_forgets_launches_that_no_longer_await_proof() {
        let (_state, supervisor) = supervisor();
        supervisor
            .probes
            .lock()
            .unwrap()
            .insert("released-binding".into(), idle(Instant::now()));
        supervisor.pass();
        assert!(supervisor.probes.lock().unwrap().is_empty());
    }

    // SPEC §6.1, W10 gap (a) (review finding 14): a proof the store accepted
    // while a switch or an engine exit keeps the gate closed is not repeated on
    // the next 250 ms pass; it backs off exactly like a failure, without a
    // failure journal entry.
    // T20
    #[test]
    fn a_passing_proof_that_left_the_gate_closed_backs_off() {
        let now = Instant::now();
        let mut state = idle(now);
        state.running = true;
        assert!(!state.settle(true, now));
        assert!(!state.running);
        assert_eq!(state.next_at, now + FIRST_RETRY);
        assert_eq!(state.backoff, FIRST_RETRY * 2);
        // A failure is journaled once per run, and keeps doubling.
        assert!(state.settle(false, now));
        assert!(!state.settle(false, now));
        assert_eq!(state.backoff, FIRST_RETRY * 8);
    }

    // SPEC §4.3 (review finding 15): probes are owned by supervision. An
    // aborted supervisor aborts them; a cancelled one joins them before it
    // returns, so nothing it started still holds the coordinator's state.
    #[tokio::test]
    async fn probes_end_with_their_supervisor() {
        let (_state, supervisor) = supervisor();
        let aborted = Arc::new(AtomicBool::new(false));
        supervisor.children.track(child(aborted.clone()));
        let handle = supervisor.clone().spawn();
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.abort();
        let _ = handle.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !aborted.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("an aborted supervisor aborts its probes");

        let joined = Arc::new(AtomicBool::new(false));
        supervisor.children.track(child(joined.clone()));
        let (cancel, receiver) = tokio::sync::watch::channel(false);
        let handle = supervisor.clone().spawn_until(receiver);
        cancel.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
        assert!(joined.load(Ordering::SeqCst), "joined before returning");
        assert_eq!(supervisor.children.running(), 0);
    }
}

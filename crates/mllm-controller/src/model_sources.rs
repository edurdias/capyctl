//! ADR 0008: materializing declared remote model sources on their hosts.
//!
//! A revision whose `model.source` is `huggingface` or `http` is accepted
//! with its source pending on the host it resolved on. This supervisor asks
//! that host to materialize it (`MaterializeSource`, answered at once with the
//! source's state) and records every answer, so status shows the download's
//! progress, and activation and the checkpoint digest (ADR 0014 §7) wait for a
//! verified copy. A placement on another host materializes there first
//! ([`ensure_materialized`]), before any launch is sent, exactly as a first
//! placement measures its digest.
//!
//! Nothing here launches, releases or reserves engine resources. The store
//! bytes a download needs are the host's own filesystem accounting (SPEC §7:
//! the model store is a charged resource owner there). A host that cannot be
//! reached leaves the source pending; a host without the capability is never
//! sent the action.
use crate::{agent_sessions::AgentSessions, ownership::SharedCoordinatorState};
use mllm_adapters::traits::RuntimeError;
use mllm_domain::group::{CommandIdentity, MemberKey};
use mllm_protocol::{
    execution::{MaterializeSourcePlan, MemberAction, MemberCommand},
    pb,
};
use mllm_store::model_sources::{PendingSource, SourceReport, SourceState};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

/// How often the supervisor looks for pending sources.
const PASS_INTERVAL: Duration = Duration::from_secs(1);
/// How often a running download's progress is asked for.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// The bound on one request: the host answers at once, never after the
/// download.
const REQUEST_DEADLINE: Duration = Duration::from_secs(60);
const FIRST_RETRY: Duration = Duration::from_secs(5);
const MAX_RETRY: Duration = Duration::from_secs(300);

/// No answer: the host is offline, lacks the capability, or the command did
/// not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unavailable;

pub type ReportFuture = Pin<Box<dyn Future<Output = Result<SourceReport, Unavailable>> + Send>>;

/// Where a pending source is materialized.
pub trait SourceHost: Send + Sync + 'static {
    /// Whether the host can be asked now.
    fn reachable(&self, host: &str) -> bool;
    fn request(&self, pending: PendingSource) -> ReportFuture;
}

/// What a MaterializeSource result says, once `validate_result` bound its
/// shape to the plan.
pub fn report_from(result: &pb::MemberExecutionResult) -> Result<SourceReport, Unavailable> {
    let evidence = result.source.as_ref().ok_or(Unavailable)?;
    let state = match evidence.state.as_str() {
        "pending" => SourceState::Pending,
        "downloading" => SourceState::Downloading,
        "verified" => SourceState::Verified,
        "failed" => SourceState::Failed,
        _ => return Err(Unavailable),
    };
    Ok(SourceReport {
        state,
        bytes_done: evidence.bytes_done,
        bytes_total: evidence.bytes_total,
        reason: (state == SourceState::Failed).then(|| evidence.reason.clone()),
    })
}

/// The `MaterializeSource` command for one deployment revision on one host.
#[allow(clippy::too_many_arguments)]
pub fn source_command(
    controller_id: &str,
    host_id: &str,
    deployment_id: &str,
    revision: i64,
    generation: i64,
    profile_fingerprint: &str,
    plan: MaterializeSourcePlan,
    deadline_ms: i64,
) -> MemberCommand {
    let id = ulid::Ulid::new().to_string();
    let mut command = MemberCommand {
        identity: CommandIdentity {
            controller_id: controller_id.into(),
            member: MemberKey {
                host_id: host_id.into(),
                member_id: "head".into(),
            },
            deployment_id: deployment_id.into(),
            operation_id: id.clone(),
            command_id: id.clone(),
            step_id: id,
            generation,
            revision,
            deadline_ms,
            payload_digest: [0; 32],
            expected_state: "source".into(),
            profile_fingerprint: profile_fingerprint.into(),
            instance_index: 0,
        },
        action: MemberAction::MaterializeSource(plan),
    };
    command.identity.payload_digest = command.canonical_digest();
    command
}

/// Record a host's answer under the owner.
pub fn record(
    owner: &SharedCoordinatorState,
    deployment: &str,
    revision: i64,
    host: &str,
    source_key: &str,
    report: &SourceReport,
) -> Result<(), Unavailable> {
    let owner = owner.lock().map_err(|_| Unavailable)?;
    owner
        .store()
        .record_model_source(
            owner.session(),
            deployment,
            revision,
            host,
            source_key,
            report,
            mllm_protocol::now_unix_ms(),
        )
        .map_err(|_| Unavailable)
}

/// Remote hosts, through their authenticated sessions.
pub struct RemoteSources {
    owner: SharedCoordinatorState,
    sessions: Arc<AgentSessions>,
    controller_id: String,
}

impl RemoteSources {
    pub fn new(
        owner: SharedCoordinatorState,
        sessions: Arc<AgentSessions>,
        controller_id: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            owner,
            sessions,
            controller_id,
        })
    }

    fn command(&self, pending: &PendingSource) -> Result<MemberCommand, Unavailable> {
        let owner = self.owner.lock().map_err(|_| Unavailable)?;
        let publication = owner
            .store()
            .host_publication(&pending.host_id)
            .ok()
            .flatten()
            .filter(|publication| publication.host_id == pending.host_id)
            .ok_or(Unavailable)?;
        let source = owner
            .store()
            .host_configuration_source(&pending.deployment_id, pending.revision, &pending.host_id)
            .ok()
            .flatten()
            .ok_or(Unavailable)?;
        let local =
            mllm_config::remote_resources::local_deployment_document(&pending.host_id, &source)
                .map_err(|_| Unavailable)?;
        let plan = MaterializeSourcePlan::new(&local.to_string(), &publication.fingerprint)
            .filter(|plan| plan.source_key == pending.source_key)
            .ok_or(Unavailable)?;
        Ok(source_command(
            &self.controller_id,
            &pending.host_id,
            &pending.deployment_id,
            pending.revision,
            pending.generation,
            &pending.effective.profile.build_fingerprint,
            plan,
            mllm_protocol::now_unix_ms() + REQUEST_DEADLINE.as_millis() as i64,
        ))
    }
}

impl SourceHost for RemoteSources {
    fn reachable(&self, host: &str) -> bool {
        // ADR 0017: never a drain-only host.
        self.sessions.current_session(host).is_some()
            && self.sessions.supports_model_sources(host)
            && self.sessions.preflight(host, &[], true).is_ok()
    }
    fn request(&self, pending: PendingSource) -> ReportFuture {
        let command = self.command(&pending);
        let sessions = self.sessions.clone();
        Box::pin(async move {
            let result = sessions.execute(command?).await.map_err(|_| Unavailable)?;
            report_from(&result)
        })
    }
}

/// ADR 0008: before a launch on `host`, its copy of the revision's remote
/// source must be verified. The host is asked (starting the download if
/// needed) and polled until it answers `verified`, a failure, or `deadline_ms`
/// passes. Every answer is recorded. A failure or a timeout refuses the launch
/// before anything was sent (`model_source:<reason>`, `model_source_pending`);
/// a download still running keeps running on the host for the next attempt.
/// A deployment with a local source returns at once.
#[allow(clippy::too_many_arguments)]
pub async fn ensure_materialized(
    owner: &SharedCoordinatorState,
    sessions: &Arc<AgentSessions>,
    controller_id: &str,
    host: &str,
    deployment_id: &str,
    revision: i64,
    generation: i64,
    profile_fingerprint: &str,
    deployment_config: &str,
    host_policy_fingerprint: &str,
    deadline_ms: i64,
) -> Result<(), RuntimeError> {
    let Some(plan) = MaterializeSourcePlan::new(deployment_config, host_policy_fingerprint) else {
        return Ok(());
    };
    if !sessions.supports_model_sources(host) {
        // ADR 0017: the typed refusal for a host without the feature.
        return Err(RuntimeError::Refused(mllm_protocol::capabilities::missing(
            mllm_protocol::capabilities::MODEL_SOURCES,
        )));
    }
    loop {
        let now = mllm_protocol::now_unix_ms();
        let command = source_command(
            controller_id,
            host,
            deployment_id,
            revision,
            generation,
            profile_fingerprint,
            plan.clone(),
            (now + REQUEST_DEADLINE.as_millis() as i64).min(deadline_ms),
        );
        let report = match sessions.execute(command).await {
            Ok(result) => report_from(&result).ok(),
            Err(_) => None,
        };
        if let Some(report) = &report {
            let _ = record(
                owner,
                deployment_id,
                revision,
                host,
                &plan.source_key,
                report,
            );
            match report.state {
                SourceState::Verified => return Ok(()),
                SourceState::Failed => {
                    return Err(RuntimeError::Refused(format!(
                        "model_source:{}",
                        report.reason.as_deref().unwrap_or("failed")
                    )))
                }
                SourceState::Pending | SourceState::Downloading => {}
            }
        }
        let wait = POLL_INTERVAL.as_millis() as i64;
        if mllm_protocol::now_unix_ms() + wait >= deadline_ms {
            return Err(RuntimeError::Refused("model_source_pending".into()));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

struct Attempt {
    next_at: Instant,
    backoff: Duration,
}

/// The background supervisor: at most one request per source at a time,
/// polling running downloads and backing off after a retryable failure.
pub struct SourceMaterializer {
    owner: SharedCoordinatorState,
    host: Arc<dyn SourceHost>,
    running: Mutex<BTreeSet<(String, i64, String)>>,
    attempts: Mutex<BTreeMap<(String, i64, String), Attempt>>,
}

impl SourceMaterializer {
    pub fn new(owner: SharedCoordinatorState, host: Arc<dyn SourceHost>) -> Arc<Self> {
        Arc::new(Self {
            owner,
            host,
            running: Mutex::default(),
            attempts: Mutex::default(),
        })
    }

    /// Run supervision until `cancel` reads true (or the task is aborted).
    /// Requests in flight are aborted and joined; the host's download keeps
    /// running and is reported by the next run.
    pub fn spawn_until(
        self: Arc<Self>,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let children = Arc::new(crate::supervised::Children::default());
            let aborting = children.clone();
            let _abort = crate::supervised::OnDrop(move || aborting.abort_all());
            let mut tick = tokio::time::interval(PASS_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = crate::supervised::cancelled(&mut cancel) => break,
                    _ = tick.tick() => {}
                }
                for request in self.clone().pass() {
                    children.track(request);
                }
            }
            children.join_all().await;
        })
    }

    /// One pass: ask about every due pending source whose host is reachable
    /// and has no request running for it.
    pub fn pass(self: Arc<Self>) -> Vec<tokio::task::JoinHandle<()>> {
        let pending = match self.owner.lock() {
            Ok(owner) => owner.store().pending_model_sources().unwrap_or_default(),
            Err(_) => return Vec::new(),
        };
        let now = Instant::now();
        let mut started = Vec::new();
        for source in pending {
            let key = (
                source.deployment_id.clone(),
                source.revision,
                source.host_id.clone(),
            );
            let due = self
                .attempts
                .lock()
                .map(|attempts| attempts.get(&key).is_none_or(|a| a.next_at <= now))
                .unwrap_or(false);
            if !due || !self.host.reachable(&source.host_id) {
                continue;
            }
            let Ok(mut running) = self.running.lock() else {
                continue;
            };
            if !running.insert(key.clone()) {
                continue;
            }
            drop(running);
            let this = self.clone();
            started.push(tokio::spawn(async move {
                let answer = this.host.request(source.clone()).await;
                let next = match &answer {
                    Ok(report) => {
                        let recorded = record(
                            &this.owner,
                            &source.deployment_id,
                            source.revision,
                            &source.host_id,
                            &source.source_key,
                            report,
                        );
                        match (report.state, recorded) {
                            (_, Err(_)) => Some(None),
                            (SourceState::Verified, _) => None,
                            (SourceState::Pending | SourceState::Downloading, _) => {
                                Some(Some(POLL_INTERVAL))
                            }
                            (SourceState::Failed, _) => Some(None),
                        }
                    }
                    Err(Unavailable) => Some(None),
                };
                if let Ok(mut attempts) = this.attempts.lock() {
                    match next {
                        None => {
                            attempts.remove(&key);
                        }
                        Some(Some(poll)) => {
                            attempts.insert(
                                key.clone(),
                                Attempt {
                                    next_at: Instant::now() + poll,
                                    backoff: FIRST_RETRY,
                                },
                            );
                        }
                        Some(None) => {
                            let backoff = attempts
                                .get(&key)
                                .map_or(FIRST_RETRY, |a| (a.backoff * 2).min(MAX_RETRY));
                            attempts.insert(
                                key.clone(),
                                Attempt {
                                    next_at: Instant::now() + backoff,
                                    backoff,
                                },
                            );
                        }
                    }
                }
                if let Ok(mut running) = this.running.lock() {
                    running.remove(&key);
                }
            }));
        }
        started
    }
}

#[cfg(test)]
mod tests;

//! SPEC §13: transport sessions never release ownership or grant execution authority.
use crate::enrollment::EnrollmentAuthority;
use mllm_protocol::pb::{
    self, agent_control_server::AgentControl, agent_to_server, server_to_agent,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
const CAPACITY: usize = 16;
/// SPEC §4.3, §13: queue slots commands and heartbeats never take, so the
/// session's own replies (SessionReady, a drain acknowledgement) always fit.
const REPLY_RESERVE: usize = 4;
/// How long the session waits for room for one of its own replies.
const REPLY_BOUND: Duration = Duration::from_secs(5);
/// SPEC §13: a command already delivered on a live session is redelivered on
/// that session after this, doubling up to `REDELIVER_MAX`; a new session gets
/// it at once. A lost or ignored result is still asked for again.
const REDELIVER_FIRST: Duration = Duration::from_secs(2);
const REDELIVER_MAX: Duration = Duration::from_secs(8);
/// Queue a command or heartbeat only while the reply reserve stays free.
fn send_command(
    outgoing: &mpsc::Sender<Result<pb::ServerToAgent, Status>>,
    message: pb::ServerToAgent,
) -> bool {
    outgoing.capacity() > REPLY_RESERVE && outgoing.try_send(Ok(message)).is_ok()
}
/// Send one of the session's own replies, waiting a bounded time for room.
async fn send_reply(
    outgoing: &mpsc::Sender<Result<pb::ServerToAgent, Status>>,
    message: pb::ServerToAgent,
) -> Result<(), Box<Status>> {
    match tokio::time::timeout(REPLY_BOUND, outgoing.send(Ok(message))).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(Box::new(Status::resource_exhausted("host response queue unavailable"))),
    }
}
#[derive(Clone, serde::Serialize)]
pub struct HostSessionView {
    pub host_id: String,
    pub session_id: String,
    pub online: bool,
    pub reconciled: bool,
    pub eligible: bool,
    /// Owner decision 4 (2026-09-22): a drain of this host has a Stop that has
    /// not settled; the host is not eligible for placement while it stands.
    pub drain_pending: bool,
    /// Owner decision 2026-09-23: the host's session is up but its heartbeats
    /// have been silent past the suspend bound; dispatch to it is suspended.
    pub unresponsive: bool,
    pub domains: Vec<DomainView>,
    pub profiles: Vec<ProfileView>,
}
#[derive(Clone, serde::Serialize)]
pub struct DomainView {
    pub domain_id: String,
    pub kind: String,
    pub observed_bytes: i64,
    pub observed_at_unix: i64,
    pub capacity_bytes: i64,
    pub available_bytes: i64,
    pub observed_at_unix_ms: i64,
    /// ADR 0007: per-process resident memory sampled beside this domain's
    /// availability (`process_residency`). Evidence for admission only; never
    /// shown in status.
    #[serde(skip)]
    pub residents: Vec<mllm_domain::resources::ProcessResident>,
}
impl DomainView {
    /// The view of one reported domain. Residents outside their bounds are
    /// dropped as a whole: no credit is safe, a partial one is not.
    fn of(d: &pb::DomainObservation) -> Self {
        let valid = d.residents.len() <= crate::agent_sessions::MAX_RESIDENTS
            && d.residents.iter().all(|r| {
                r.pid != 0
                    && r.start_ticks != 0
                    && r.boot_id.len() == 36
                    && r.resident_bytes >= 0
                    && r.resident_bytes <= d.capacity_bytes
            });
        Self {
            domain_id: d.domain_id.clone(),
            kind: d.kind.clone(),
            observed_bytes: d.observed_bytes,
            observed_at_unix: d.observed_at_unix,
            capacity_bytes: d.capacity_bytes,
            available_bytes: d.available_bytes,
            observed_at_unix_ms: d.observed_at_unix_ms.min(mllm_protocol::now_unix_ms()),
            residents: if valid {
                d.residents
                    .iter()
                    .map(|r| mllm_domain::resources::ProcessResident {
                        pid: r.pid,
                        boot_id: r.boot_id.clone(),
                        start_ticks: r.start_ticks,
                        bytes: r.resident_bytes,
                    })
                    .collect()
            } else {
                Vec::new()
            },
        }
    }
}
/// ADR 0007: the most processes one domain report may attribute memory to.
pub const MAX_RESIDENTS: usize = 256;
#[derive(Clone, serde::Serialize)]
pub struct ProfileView {
    pub name: String,
    pub build_fingerprint: String,
    pub eligibility: String,
    /// ADR 0008 (owner decision 2026-09-23): the installation's fingerprint
    /// registered at agent start, whether a launch found it drifted since, and
    /// the capabilities its launch-time probe found missing.
    pub installation: InstallationView,
}
#[derive(Clone, serde::Serialize)]
pub struct InstallationView {
    pub version: String,
    pub digest: String,
    /// "" (older host) | measured | unmeasured | drifted
    pub state: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub observed_digest: String,
    pub capabilities_missing: Vec<String>,
}
impl ProfileView {
    fn of(p: &pb::RuntimeProfileStatus) -> Self {
        Self {
            name: p.name.clone(),
            build_fingerprint: p.build_fingerprint.clone(),
            eligibility: p.eligibility.clone(),
            installation: InstallationView {
                version: p.installation_version.clone(),
                digest: p.installation_digest.clone(),
                state: p.installation_state.clone(),
                observed_digest: p.installation_observed_digest.clone(),
                capabilities_missing: p.capabilities_missing.clone(),
            },
        }
    }
}
/// ADR 0008: the installation fields a host publishes are closed and bounded.
fn installation_fields_valid(p: &pb::RuntimeProfileStatus) -> bool {
    let digest = |value: &str| {
        value.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    };
    (p.installation_version.len() <= 128 && p.installation_version.bytes().all(|b| b.is_ascii_graphic()))
        && (p.installation_digest.is_empty() || digest(&p.installation_digest))
        && matches!(p.installation_state.as_str(), "" | "measured" | "unmeasured" | "drifted")
        && (p.installation_observed_digest.is_empty()
            || p.installation_observed_digest == "unmeasured"
            || digest(&p.installation_observed_digest))
        && p.capabilities_missing.len() <= 8
        && p.capabilities_missing.iter().all(|c| matches!(c.as_str(), "core" | "deep_park" | "metrics" | "observation"))
}
/// ADR 0008: the drifts `new` reports that `old` did not (a new drift, or a
/// drift to a different observed digest): (installation, registered, observed).
fn newly_drifted(
    new: &[pb::RuntimeProfileStatus],
    old: &[pb::RuntimeProfileStatus],
) -> Vec<(String, String, String)> {
    new.iter()
        .zip(old)
        .filter(|(new, previous)| {
            new.installation_state == "drifted"
                && !new.installation_digest.is_empty()
                && (previous.installation_state != "drifted"
                    || previous.installation_observed_digest != new.installation_observed_digest)
        })
        .map(|(new, _)| {
            (
                new.name.clone(),
                new.installation_digest.clone(),
                new.installation_observed_digest.clone(),
            )
        })
        .collect()
}
/// ADR 0008: what a host registered for its installations stays fixed for the
/// session; drift and probe results may change as its launches find them.
fn same_registration(new: &[pb::RuntimeProfileStatus], old: &[pb::RuntimeProfileStatus]) -> bool {
    new.len() == old.len()
        && new.iter().zip(old).all(|(a, b)| {
            a.name == b.name
                && a.build_fingerprint == b.build_fingerprint
                && a.eligibility == b.eligibility
                && a.reason == b.reason
                && a.installation_version == b.installation_version
                && a.installation_digest == b.installation_digest
        })
}
/// SPEC §7: freshness is judged on the controller clock. An enrolled host's
/// clock may lead the controller's by at most this bound (the same bound host
/// publication applies); a lead within it is not a future observation.
pub const HOST_CLOCK_LEAD_MS: i64 = 500;
/// A host timestamp on the controller clock: refused beyond the tolerated lead,
/// otherwise never later than `now`, so no ledger or evidence check downstream
/// ever sees a future time from an ordinary cross-host clock difference.
pub fn controller_time(observed: i64, now: i64) -> Option<i64> {
    (observed >= 0 && observed <= now.saturating_add(HOST_CLOCK_LEAD_MS))
        .then(|| observed.min(now))
}
/// SPEC §13.1: results carry evidence and errors, not only success. A launch
/// the host reports as `launched`, not usable, with every recorded process
/// verified gone, is an engine that exited before readiness: a terminal result
/// the host will only replay. Waiting for another would hold the Initialize
/// until its deadline. The claim stays retained; the caller treats the launch
/// as unproven and settles it on verified absence (SPEC §13.2). A process that
/// is still alive (loading) or unknown keeps the launch in progress.
pub(crate) fn launch_ended_before_readiness(result: &pb::MemberExecutionResult) -> bool {
    result.state == "launched"
        && !result.model_usable
        && !result.processes.is_empty()
        && result.processes.iter().all(|process| process.presence == "gone")
}
fn bounded_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
}
struct Session {
    view: HostSessionView,
    peer: Vec<u8>,
    outgoing: Option<mpsc::Sender<Result<pb::ServerToAgent, Status>>>,
    cancel: tokio::sync::watch::Sender<bool>,
    inventory: Option<pb::ReportInventory>,
    history: Vec<pb::JournalSummary>,
    /// SPEC §4.2: the accepted inventory carries an approved preparation with a
    /// resolvable profile. Eligibility also needs the session to reconcile.
    prepared: bool,
    /// SPEC §4.3: the host announced a graceful shutdown on this session. Its
    /// engines' readiness no longer stands for dispatch.
    draining: bool,
    /// Owner decision 2026-09-23: the host declared heartbeats in its Connect.
    /// A peer without them is never sent one and never suspended for silence.
    heartbeats: bool,
    /// Silent past the suspend bound. The session's readiness no longer stands
    /// for dispatch until a fresh probe re-proves it (same as a reconnect).
    unresponsive: bool,
}
/// Owner decision 2026-09-23: application heartbeats on the control session.
/// The server sends one every `interval` to a host that declared heartbeats and
/// tracks when it last heard anything from that host's session. After
/// `suspend_after` of silence it suspends dispatch to the host (accounting kept,
/// nothing released); after `lost_after` it ends the session, which takes the
/// existing lost-session path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeartbeatPolicy {
    pub interval: Duration,
    pub suspend_after: Duration,
    pub lost_after: Duration,
}
impl Default for HeartbeatPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            suspend_after: Duration::from_secs(5),
            lost_after: Duration::from_secs(30),
        }
    }
}
/// Retain host-scoped ownership in the same controller Store before a result
/// becomes lifecycle evidence. Implementations must never infer local PID state.
pub trait RemoteEvidenceObserver: Send + Sync {
    fn retain(&self, command: &mllm_protocol::execution::MemberCommand, result: &pb::MemberExecutionResult) -> Result<(), Box<Status>>;
}
struct PendingProvision {
    identity: pb::CommandIdentity,
    acknowledged: tokio::sync::watch::Sender<Option<ProvisionOutcome>>,
}
/// What an authenticated host answered to a private ingress provision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionOutcome {
    /// The key is in the host's protected storage.
    Provisioned,
    /// SPEC §13: the host's own policy refused the launch before any effect,
    /// with one closed category (for example `checkpoint_mismatch`). Nothing was
    /// stored or started; the launch must not be sent.
    Refused(String),
}
struct ProvisionGuard {
    pending: Arc<Mutex<BTreeMap<String, PendingProvision>>>, id: String,
}
impl Drop for ProvisionGuard {
    fn drop(&mut self) { if let Ok(mut pending) = self.pending.lock() { pending.remove(&self.id); } }
}
struct PendingCommand {
    command: mllm_protocol::execution::MemberCommand,
    /// The terminal result and the authenticated host session it arrived on.
    result: tokio::sync::watch::Sender<Option<(String, pb::MemberExecutionResult)>>,
    observer: Option<Arc<dyn RemoteEvidenceObserver>>,
}
struct PendingGuard {
    pending: Arc<Mutex<BTreeMap<String, PendingCommand>>>,
    id: String,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() { pending.remove(&self.id); }
    }
}
#[derive(Clone)]
pub struct AgentSessions {
    authority: Arc<EnrollmentAuthority>,
    sessions: Arc<Mutex<BTreeMap<String, Session>>>,
    pending: Arc<Mutex<BTreeMap<String, PendingCommand>>>,
    provisions: Arc<Mutex<BTreeMap<String, PendingProvision>>>,
    /// Bumped whenever any host session connects, reconciles or ends.
    changes: Arc<tokio::sync::watch::Sender<u64>>,
    /// SPEC §10, ADR 0013 §10 (D9): latest engine load per instance, in memory only.
    load: Arc<crate::load_table::LoadTable>,
    /// SPEC §17 (M80): host-reported latency distributions, in memory only.
    latency: Arc<crate::latency_table::LatencyTable>,
    /// SPEC §4.3: suspends dispatch to a draining host's engines before the host
    /// is acknowledged. Installed at wiring; blocking store work.
    drain_hook: Arc<Mutex<Option<DrainHook>>>,
    /// Owner decision 2026-09-23: suspends dispatch to a host whose heartbeats
    /// went silent, and forgets the readiness its session proved.
    unresponsive_hook: Arc<Mutex<Option<DrainHook>>>,
    /// SPEC §13.2 (W13): closes and settles an instance whose engine the host
    /// reported exited. Installed at wiring; blocking store work.
    exit_hook: Arc<Mutex<Option<ExitHook>>>,
    heartbeat: HeartbeatPolicy,
    /// Every session's serve task, tracked so the server's shutdown joins
    /// them ([`AgentSessions::shutdown`]) instead of leaving them detached
    /// with a hook (an exit settlement, a drain suspension) still running
    /// against the coordinator's owned state.
    tasks: Arc<crate::supervised::Children>,
}
/// Suspends dispatch to every engine on the named host.
pub type DrainHook = Arc<dyn Fn(&str) + Send + Sync>;
/// Handles one validated `MemberExit` from the named host's current session.
pub type ExitHook = Arc<dyn Fn(&str, &mllm_protocol::reports::MemberExit) + Send + Sync>;
impl AgentSessions {
    pub fn new(authority: Arc<EnrollmentAuthority>) -> Arc<Self> {
        Self::with_heartbeats(authority, HeartbeatPolicy::default())
    }
    /// As `new`, with the server's configured heartbeat bounds.
    pub fn with_heartbeats(
        authority: Arc<EnrollmentAuthority>,
        heartbeat: HeartbeatPolicy,
    ) -> Arc<Self> {
        Arc::new(Self {
            authority,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            provisions: Arc::new(Mutex::new(BTreeMap::new())),
            changes: Arc::new(tokio::sync::watch::channel(0).0),
            load: Arc::new(crate::load_table::LoadTable::new()),
            latency: Arc::new(crate::latency_table::LatencyTable::new()),
            drain_hook: Arc::new(Mutex::new(None)),
            unresponsive_hook: Arc::new(Mutex::new(None)),
            exit_hook: Arc::new(Mutex::new(None)),
            heartbeat,
            tasks: Default::default(),
        })
    }

    /// End every host session and join its serve task, aborting any still
    /// running after `bound`. Each session is cancelled first and ends
    /// through its ordinary teardown (marked offline; SPEC §13: every claim
    /// is retained), after any hook it is running has finished. For the
    /// server role's shutdown, once its listeners have stopped.
    pub async fn shutdown(&self, bound: Duration) {
        if let Ok(sessions) = self.sessions.lock() {
            for session in sessions.values() {
                let _ = session.cancel.send(true);
            }
        }
        self.tasks.join_within(bound).await;
    }
    /// Owner decision 2026-09-23: what suspends dispatch to a host whose
    /// heartbeats went silent past the suspend bound. Without one the readiness
    /// supervisor still closes dispatch, since the session stops counting as
    /// current while the host is unresponsive.
    pub fn on_host_unresponsive(&self, hook: DrainHook) {
        if let Ok(mut installed) = self.unresponsive_hook.lock() {
            *installed = Some(hook);
        }
    }
    /// SPEC §13.2 (W13): what closes and settles an instance whose engine its
    /// host reported exited. Without one, an exit report is accepted and
    /// dropped; the readiness supervisor still finds the engine unusable at its
    /// next probe.
    pub fn on_member_exit(&self, hook: ExitHook) {
        if let Ok(mut installed) = self.exit_hook.lock() {
            *installed = Some(hook);
        }
    }
    /// Whether the host's session is up but silent past the suspend bound.
    pub fn unresponsive(&self, host: &str) -> bool {
        self.sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(host).map(|s| s.view.online && s.unresponsive))
            .unwrap_or(false)
    }
    /// SPEC §4.3: what suspends a draining host's dispatch before its drain is
    /// acknowledged. Without one, the host is acknowledged once its session
    /// stops counting as current, which the readiness supervisor acts on.
    pub fn on_host_draining(&self, hook: DrainHook) {
        if let Ok(mut installed) = self.drain_hook.lock() {
            *installed = Some(hook);
        }
    }
    /// ADR 0013 §10: the router's read of host-reported engine load. Samples
    /// are routing hints only, never readiness or release evidence.
    pub fn load_table(&self) -> Arc<crate::load_table::LoadTable> {
        self.load.clone()
    }
    /// SPEC §17 (M80): host ingress and engine latency distributions, as the
    /// hosts reported them with their load. Observability only.
    pub fn latency_table(&self) -> Arc<crate::latency_table::LatencyTable> {
        self.latency.clone()
    }
    /// SPEC §13.2: a readiness supervisor must learn of a host session loss at
    /// once, not at its next poll, so dispatch closes before more work is sent.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }
    fn changed(&self) {
        self.changes.send_modify(|generation| *generation = generation.wrapping_add(1));
    }
    /// The host's current authenticated session, only once it has reconciled,
    /// and not while the host is draining (SPEC §4.3): a draining host's session
    /// no longer proves readiness for dispatch. Nor while its heartbeats are
    /// silent past the suspend bound (owner decision 2026-09-23).
    pub fn current_session(&self, host: &str) -> Option<String> {
        self.sessions
            .lock()
            .ok()?
            .get(host)
            .filter(|s| s.view.online && s.view.reconciled && !s.draining && !s.unresponsive)
            .map(|s| s.view.session_id.clone())
    }
    pub fn snapshot(&self) -> Vec<HostSessionView> {
        // Read before the session lock: the store lock is never taken inside it.
        let pending = self.authority.hosts_with_pending_drain();
        self.sessions
            .lock()
            .map(|s| {
                s.values()
                    .map(|s| with_drain(s.view.clone(), pending.as_ref()))
                    .collect()
            })
            .unwrap_or_default()
    }
    /// The host's session view as status reports it: not eligible while a
    /// drain of it is pending (owner decision 4).
    pub fn inspect(&self, host: &str) -> Option<HostSessionView> {
        let pending = self.authority.hosts_with_pending_drain();
        self.view(host).map(|view| with_drain(view, pending.as_ref()))
    }
    /// The raw session view, without any store read.
    fn view(&self, host: &str) -> Option<HostSessionView> {
        self.sessions.lock().ok()?.get(host).map(|s| s.view.clone())
    }
    /// U5 supplies validated commands from the existing coordinator; transport cannot mint grants.
    pub fn dispatch(&self, host: &str, command: pb::ExecuteMember) -> Result<(), Box<Status>> {
        self.dispatch_to(host, command).map(|_| ())
    }
    /// As `dispatch`, naming the session the command was queued on.
    fn dispatch_to(&self, host: &str, command: pb::ExecuteMember) -> Result<String, Box<Status>> {
        let typed = mllm_protocol::execution::MemberCommand::try_from(pb::ServerToAgent {
            msg: Some(server_to_agent::Msg::ExecuteMember(command.clone())),
        })
        .map_err(|_| Status::invalid_argument("invalid command"))?;
        typed
            .verify_digest()
            .map_err(|_| Status::invalid_argument("invalid command"))?;
        let (session_id, peer) = {
            let sessions = self.sessions.lock().map_err(|_| denied())?;
            let session = sessions.get(host).ok_or_else(denied)?;
            (session.view.session_id.clone(), session.peer.clone())
        };
        // Never retain the session mutex while entering the coordinator/store.
        self.authority
            .authorize_certificate(&peer, host, now())
            .map_err(|_| denied())?;
        let sessions = self.sessions.lock().map_err(|_| denied())?;
        let session = sessions.get(host).ok_or_else(denied)?;
        if session.view.session_id != session_id
            || !session.view.online
            || !session.view.reconciled
            || command.identity.as_ref().is_none_or(|id| {
                id.host_id != host
                    || id.controller_id != self.authority.controller_id()
                    || id.deadline_unix_ms <= mllm_protocol::now_unix_ms()
            })
        {
            return Err(denied().into());
        }
        let queued = send_command(
            session.outgoing.as_ref().ok_or_else(denied)?,
            pb::ServerToAgent {
                msg: Some(server_to_agent::Msg::ExecuteMember(command)),
            },
        );
        if !queued {
            return Err(Box::new(Status::resource_exhausted("host command queue unavailable")));
        }
        Ok(session_id)
    }
    /// The host's live, reconciled session, if any.
    fn live_session(&self, host: &str) -> Option<String> {
        self.sessions
            .lock()
            .ok()?
            .get(host)
            .filter(|s| s.view.online && s.view.reconciled)
            .map(|s| s.view.session_id.clone())
    }
    /// At-least-once delivery of the same immutable command. A reconnect never
    /// changes its ID/digest or creates a new effect ticket on the host.
    /// SPEC §13: timeout/disconnect provides no absence or completion evidence.
    pub async fn execute(
        &self, command: mllm_protocol::execution::MemberCommand,
    ) -> Result<pb::MemberExecutionResult, Box<Status>> {
        self.execute_observed(command, None).await
    }
    pub async fn execute_observed(
        &self, command: mllm_protocol::execution::MemberCommand,
        observer: Option<Arc<dyn RemoteEvidenceObserver>>,
    ) -> Result<pb::MemberExecutionResult, Box<Status>> {
        self.execute_on_session(command, observer).await.map(|(_, result)| result)
    }
    /// As `execute_observed`, also naming the authenticated host session the
    /// terminal result arrived on. Readiness evidence belongs to that session.
    pub async fn execute_on_session(
        &self, command: mllm_protocol::execution::MemberCommand,
        observer: Option<Arc<dyn RemoteEvidenceObserver>>,
    ) -> Result<(String, pb::MemberExecutionResult), Box<Status>> {
        command.verify_digest().map_err(|_| Status::invalid_argument("invalid command"))?;
        let host = command.identity.member.host_id.clone();
        let id = command.identity.command_id.clone();
        let deadline = command.identity.deadline_ms;
        let wire = command.to_wire();
        let (send, mut receive) = tokio::sync::watch::channel(None);
        {
            let mut pending = self.pending.lock().map_err(|_| denied())?;
            if pending.len() >= 128 || pending.contains_key(&id) {
                return Err(Status::resource_exhausted("command observer capacity unavailable").into());
            }
            pending.insert(id.clone(), PendingCommand { command, result: send, observer });
        }
        let _guard = PendingGuard { pending: self.pending.clone(), id };
        let mut sessions = self.changes.subscribe();
        let mut retry = tokio::time::interval(Duration::from_millis(500));
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // SPEC §13: the session the command was last queued on, and when it is
        // next redelivered there. A session change redelivers at once.
        let mut delivered: Option<(String, tokio::time::Instant)> = None;
        let mut backoff = REDELIVER_FIRST;
        loop {
            let remaining = deadline - mllm_protocol::now_unix_ms();
            if remaining <= 0 { return Err(Status::deadline_exceeded("host effect remains unresolved").into()); }
            tokio::select! {
                _ = retry.tick() => {},
                _ = sessions.changed() => {},
                changed = receive.changed() => {
                    changed.map_err(|_| Status::unavailable("host effect remains unresolved"))?;
                    if let Some(result) = receive.borrow().clone() { return Ok(result); }
                    continue;
                },
                _ = tokio::time::sleep(Duration::from_millis(remaining as u64)) => {
                    return Err(Status::deadline_exceeded("host effect remains unresolved").into());
                }
            }
            let Some(current) = self.live_session(&host) else { continue };
            let due = match &delivered {
                Some((session, next)) if *session == current => tokio::time::Instant::now() >= *next,
                _ => true,
            };
            if !due { continue; }
            // Queue failure or an offline host does not imply the effect failed.
            if let Ok(session) = self.dispatch_to(&host, wire.clone()) {
                if delivered.as_ref().is_some_and(|(previous, _)| *previous == session) {
                    backoff = (backoff * 2).min(REDELIVER_MAX);
                } else {
                    backoff = REDELIVER_FIRST;
                }
                delivered = Some((session, tokio::time::Instant::now() + backoff));
            }
        }
    }

    /// `received_at` is when the message arrived on the session, on the
    /// controller clock: freshness is judged there, not after lock waits.
    fn receive_result(&self, host: &str, session: &str, mut result: pb::MemberExecutionResult, received_at: i64) -> Result<(), Box<Status>> {
        let identity = result.identity.as_ref().ok_or_else(denied)?;
        if identity.host_id != host { return Err(denied().into()); }
        let pending = self.pending.lock().map_err(|_| denied())?;
        let Some(expected) = pending.get(&identity.command_id) else {
            // A cancelled observer does not authorize adopting late evidence.
            return Ok(());
        };
        mllm_protocol::execution::validate_result(&expected.command, &result).map_err(|_| denied())?;
        // SPEC §13: a result too old (or too far ahead) to be evidence is not
        // evidence, but it is no protocol violation either. It is ignored and
        // the session stays up; the command's redelivery asks for it again.
        let Some(observed) = controller_time(result.observed_at_unix_ms, received_at)
            .filter(|observed| received_at - observed <= 2_000)
        else {
            eprintln!("host {host} control session {session}: ignored a result whose observation is not fresh");
            return Ok(());
        };
        result.observed_at_unix_ms = observed;
        if let Some(observer) = &expected.observer {
            observer.retain(&expected.command, &result)?;
        }
        // Accepted/attempted is retained uncertainty, never a completed effect.
        let terminal = match &expected.command.action {
            mllm_protocol::execution::MemberAction::LaunchSingle(_) => result.model_usable
                || (result.state == "completed" && !result.claim_retained)
                || launch_ended_before_readiness(&result),
            _ => matches!(result.state.as_str(), "launched" | "completed" | "tombstone"),
        };
        // The session table is read after the result's store work, never
        // across it: evidence names only a session that is still the host's.
        let current = self
            .sessions
            .lock()
            .map_err(|_| denied())?
            .get(host)
            .is_some_and(|s| s.view.session_id == session);
        if terminal && current {
            let _ = expected.result.send(Some((session.to_owned(), result)));
        }
        Ok(())
    }

    /// Private credential provisioning is acknowledged only after protected host
    /// storage commits. The command and result journals never receive the key.
    pub async fn provision_ingress(&self, command: &mllm_protocol::execution::MemberCommand, gate_key: [u8; 32]) -> Result<ProvisionOutcome, Box<Status>> {
        command.verify_digest().map_err(|_| denied())?;
        if !matches!(command.action, mllm_protocol::execution::MemberAction::LaunchSingle(_)) || gate_key == [0; 32] { return Err(denied().into()); }
        let wire = command.to_wire();
        let identity = wire.identity.clone().ok_or_else(denied)?;
        let id = identity.command_id.clone();
        let (acknowledged, mut received) = tokio::sync::watch::channel(None);
        {
            let mut pending = self.provisions.lock().map_err(|_| denied())?;
            if pending.len() >= 128 || pending.contains_key(&id) { return Err(Status::resource_exhausted("provision observer unavailable").into()); }
            pending.insert(id.clone(), PendingProvision { identity: identity.clone(), acknowledged });
        }
        let _guard = ProvisionGuard { pending: self.provisions.clone(), id };
        let mut retry = tokio::time::interval(Duration::from_millis(500));
        loop {
            let remaining = identity.deadline_unix_ms - mllm_protocol::now_unix_ms();
            if remaining <= 0 { return Err(Status::deadline_exceeded("private ingress provision unresolved").into()); }
            tokio::select! {
                _ = retry.tick() => {
                    let peer = self.sessions.lock().map_err(|_| denied())?.get(&identity.host_id).map(|s| s.peer.clone());
                    if let Some(peer) = peer {
                        self.authority.authorize_certificate(&peer, &identity.host_id, now()).map_err(|_| denied())?;
                        let sessions = self.sessions.lock().map_err(|_| denied())?;
                        if let Some(session) = sessions.get(&identity.host_id).filter(|s| s.view.online && s.view.reconciled && s.peer == peer) {
                            if let Some(outgoing) = &session.outgoing {
                                let _ = send_command(outgoing, pb::ServerToAgent { msg: Some(server_to_agent::Msg::ProvisionIngress(pb::ProvisionIngress { command: Some(wire.clone()), gate_key: gate_key.to_vec() })) });
                            }
                        }
                    }
                },
                changed = received.changed() => {
                    changed.map_err(|_| Status::unavailable("private ingress provision unresolved"))?;
                    if let Some(outcome) = received.borrow().clone() { return Ok(outcome); }
                },
                _ = tokio::time::sleep(Duration::from_millis(remaining as u64)) => return Err(Status::deadline_exceeded("private ingress provision unresolved").into()),
            }
        }
    }
    fn receive_provision(&self, host: &str, result: pb::IngressProvisioned) -> Result<(), Box<Status>> {
        let id = result.identity.ok_or_else(denied)?;
        if id.host_id != host { return Err(denied().into()); }
        // SPEC §13: a refusal names one closed category, never free text.
        let outcome = match result.refused.as_str() {
            "" => ProvisionOutcome::Provisioned,
            reason if mllm_protocol::execution::is_policy_refusal(reason) => ProvisionOutcome::Refused(reason.to_owned()),
            _ => return Err(denied().into()),
        };
        let pending = self.provisions.lock().map_err(|_| denied())?;
        if let Some(expected) = pending.get(&id.command_id) {
            if expected.identity != id { return Err(denied().into()); }
            let _ = expected.acknowledged.send(Some(outcome));
        }
        Ok(())
    }

    /// Owner decision 2026-09-23: mark this session's host silent or heard
    /// again, and tell supervisors at once.
    fn set_unresponsive(&self, host: &str, id: &str, unresponsive: bool) -> Result<(), Box<Status>> {
        {
            let mut sessions = self.sessions.lock().map_err(|_| denied())?;
            let s = sessions.get_mut(host).ok_or_else(denied)?;
            if s.view.session_id != id {
                return Err(denied().into());
            }
            s.unresponsive = unresponsive;
            s.view.unresponsive = unresponsive;
        }
        self.changed();
        Ok(())
    }

    async fn serve(
        self,
        host: String,
        id: String,
        peer: Vec<u8>,
        mut incoming: Streaming<pb::AgentToServer>,
        outgoing: mpsc::Sender<Result<pb::ServerToAgent, Status>>,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut revoked = self.authority.revocations();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut sequence = 0;
        let reconciliation_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut reconciled = false;
        // Owner decision 2026-09-23: when this session last heard anything from
        // its host, and whether it asked for heartbeats.
        let policy = self.heartbeat;
        let heartbeats = self
            .sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(&host).map(|s| s.heartbeats))
            .unwrap_or(false);
        let mut last_heard = tokio::time::Instant::now();
        let mut silent = false;
        let mut beat = tokio::time::interval(policy.interval);
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let result: Result<(), Status> = async {
            loop {
                self.authority.authorize_certificate(&peer, &host, now()).map_err(|_| denied())?;
                tokio::select! {
                    _ = cancel.changed() => return Err(denied()),
                    _ = revoked.changed() => {},
                    _ = beat.tick(), if reconciled && heartbeats => {
                        // A full queue means the host is not reading; silence
                        // detection below is what acts on that, not this send.
                        let _ = send_command(&outgoing, pb::ServerToAgent { msg: Some(server_to_agent::Msg::Heartbeat(pb::Heartbeat { sent_at_unix_ms: mllm_protocol::now_unix_ms() })) });
                    },
                    _ = tick.tick() => {
                        if !reconciled && tokio::time::Instant::now() >= reconciliation_deadline {
                            return Err(Status::deadline_exceeded("host reconciliation deadline"));
                        }
                        if reconciled && heartbeats {
                            let silence = last_heard.elapsed();
                            // Owner decision 2026-09-23: past the lost bound the
                            // session is lost; the existing path below retains
                            // every claim.
                            if silence >= policy.lost_after {
                                return Err(Status::unavailable("host heartbeats silent past the lost bound"));
                            }
                            if silence >= policy.suspend_after && !silent {
                                silent = true;
                                self.set_unresponsive(&host, &id, true).map_err(|status| *status)?;
                                eprintln!(
                                    "host {host} control session {id}: no heartbeat for {} ms; dispatch suspended, accounting kept",
                                    silence.as_millis()
                                );
                                let hook = self.unresponsive_hook.lock().ok().and_then(|hook| hook.clone());
                                if let Some(hook) = hook {
                                    let named = host.clone();
                                    tokio::task::spawn_blocking(move || hook(&named))
                                        .await
                                        .map_err(|_| Status::internal("unresponsive host suspension failed"))?;
                                }
                            }
                        }
                    },
                    message = incoming.message() => {
                        let message = message?.ok_or_else(|| Status::unavailable("host disconnected"))?;
                        // SPEC §13: a result's freshness is judged at receipt.
                        let received_at = mllm_protocol::now_unix_ms();
                        self.authority.authorize_certificate(&peer, &host, now()).map_err(|_| denied())?;
                        last_heard = tokio::time::Instant::now();
                        if silent {
                            // Readiness is re-proven by a fresh probe on this
                            // session before dispatch reopens (same as reconnect).
                            silent = false;
                            self.set_unresponsive(&host, &id, false).map_err(|status| *status)?;
                            eprintln!("host {host} control session {id}: heartbeats resumed; readiness must be re-proven before dispatch reopens");
                        }
                        let mut draining = false;
                        let mut exited = None;
                        // SPEC §13: store work for a message runs after the session
                        // table is released, never under it; its effect on the
                        // session is applied only if the session is still this one.
                        let mut after = After::Nothing;
                        let ready = {
                            let mut sessions = self.sessions.lock().map_err(|_| denied())?;
                            let s = sessions.get_mut(&host).ok_or_else(denied)?;
                            if s.view.session_id != id { return Err(denied()); }
                            match message.msg {
                                Some(agent_to_server::Msg::ReportInventory(inventory)) if !s.view.reconciled && s.inventory.is_none() => {
                                    if inventory.domains.len() > 128 || inventory.profiles.len() > 128 || inventory.envelope.as_ref().is_none_or(|e| e.host_id != host || !mllm_protocol::compatible_peer(&e.protocol_version)) { return Err(denied()); }
                                    if inventory.domains.iter().any(|d| !bounded_name(&d.domain_id) || !matches!(d.kind.as_str(), "system"|"device_memory"|"filesystem"|"remote_storage") || d.observed_bytes < -1)
                                        || inventory.profiles.iter().any(|p| !bounded_name(&p.name) || !bounded_name(&p.build_fingerprint) || !matches!(p.eligibility.as_str(), "unknown"|"qualified"|"unsupported"|"disabled") || !installation_fields_valid(p)) { return Err(denied()); }
                                    after = After::Publish(Box::new(inventory));
                                    false
                                }
                                Some(agent_to_server::Msg::ReportInventory(inventory)) if s.view.reconciled => {
                                    let old = s.inventory.as_ref().ok_or_else(denied)?;
                                    if inventory.approved_host_config_json != old.approved_host_config_json
                                        || inventory.policy_fingerprint != old.policy_fingerprint
                                        || inventory.host_boot_id != old.host_boot_id
                                        || !same_registration(&inventory.profiles, &old.profiles)
                                        || !inventory.profiles.iter().all(installation_fields_valid)
                                        || inventory.domains.len() != old.domains.len()
                                        || inventory.envelope.as_ref().is_none_or(|e| e.host_id != host || !mllm_protocol::compatible_peer(&e.protocol_version))
                                    { return Err(denied()); }
                                    for domain in &inventory.domains {
                                        if !old.domains.iter().any(|d| d.domain_id == domain.domain_id && d.kind == domain.kind)
                                            || domain.capacity_bytes <= 0 || domain.available_bytes < 0
                                            || domain.available_bytes > domain.capacity_bytes
                                            || domain.observed_at_unix_ms > mllm_protocol::now_unix_ms() + HOST_CLOCK_LEAD_MS
                                        { return Err(denied()); }
                                    }
                                    // SPEC §7: a tolerated host clock lead is recorded on the
                                    // controller clock, so pre-send freshness never sees a future sample.
                                    // ADR 0008 (owner decision 2026-09-23): a drift the host newly
                                    // reports is journaled once; status follows what it reports.
                                    let drifts = newly_drifted(&inventory.profiles, &old.profiles);
                                    after = After::Refresh(Box::new(inventory), drifts);
                                    false
                                }
                                Some(agent_to_server::Msg::ReconcileHistory(page)) if !s.view.reconciled && s.inventory.is_some() => {
                                    if page.records.len() > 256 || s.history.len() + page.records.len() > 4096 { return Err(Status::resource_exhausted("journal reconciliation limit")); }
                                    for record in &page.records {
                                        if record.sequence <= sequence || record.command_id.is_empty() || record.command_id.len() > 128 || !matches!(record.state.as_str(), "accepted"|"attempted"|"launched"|"completed"|"tombstone") { return Err(denied()); }
                                        sequence = record.sequence;
                                    }
                                    s.history.extend(page.records);
                                    if page.complete {
                                        s.view.reconciled = true;
                                        // SPEC §4.2 (U5-G4): connected, reconciled and
                                        // prepared with a resolvable profile. Revocation
                                        // ends this session, which clears it below.
                                        s.view.eligible = s.prepared;
                                    }
                                    page.complete
                                }
                                Some(agent_to_server::Msg::MemberResult(result)) if s.view.reconciled => {
                                    after = After::Result(Box::new(result));
                                    false
                                }
                                Some(agent_to_server::Msg::IngressProvisioned(result)) if s.view.reconciled => {
                                    after = After::Provision(Box::new(result));
                                    false
                                }
                                // SPEC §10, ADR 0013 §10 (D9): engine load from a reconciled
                                // session. A malformed report or one naming another host ends
                                // the session; unusable samples inside it are only skipped.
                                Some(agent_to_server::Msg::ReportLoad(report)) if s.view.reconciled => {
                                    let report = mllm_protocol::reports::LoadReport::try_from(report)
                                        .map_err(|_| Status::invalid_argument("invalid host load report"))?;
                                    // SPEC §17 (M80): latency deltas ride on the same report.
                                    self.latency.accept(&host, &report, mllm_protocol::now_unix_ms());
                                    self.load.accept(&host, report, mllm_protocol::now_unix_ms()).map_err(|_| denied())?;
                                    false
                                }
                                // SPEC §4.3: the host is shutting down gracefully. Its
                                // session stops standing for readiness at once; dispatch
                                // is suspended below, then the host is acknowledged.
                                Some(agent_to_server::Msg::HostDraining(notice)) if s.view.reconciled => {
                                    if notice.host_id != host { return Err(denied()); }
                                    s.draining = true;
                                    draining = true;
                                    false
                                }
                                // Owner decision 2026-09-23: liveness only; hearing it
                                // (like any message) refreshed `last_heard` above.
                                Some(agent_to_server::Msg::Heartbeat(_)) if s.view.reconciled && s.heartbeats => false,
                                // SPEC §13.2 (W13): an owned engine process exited on this
                                // host. A malformed report, or one naming another host, ends
                                // the session; generation, handle and group are validated
                                // against the store below, where a stale one changes nothing.
                                Some(agent_to_server::Msg::MemberExit(report)) if s.view.reconciled => {
                                    let report = mllm_protocol::reports::MemberExit::try_from(report)
                                        .map_err(|_| Status::invalid_argument("invalid host exit report"))?;
                                    if report.host_id != host { return Err(denied()); }
                                    // SPEC §7: on the controller clock; a future observation
                                    // beyond the tolerated lead is not evidence. The host
                                    // reports it again, so skipping it loses nothing.
                                    if let Some(observed) = controller_time(report.observed_at_ms, mllm_protocol::now_unix_ms()) {
                                        exited = Some(mllm_protocol::reports::MemberExit { observed_at_ms: observed, ..report });
                                    }
                                    false
                                }
                                // Legacy untyped evidence never grants completion authority.
                                _ => return Err(denied()),
                            }
                        };
                        match after {
                            After::Nothing => {}
                            After::Publish(inventory) => {
                                // SPEC §§4.2, 13: a refused publication ends the session with a
                                // named reason instead of a generic denial.
                                self.authority.publish_inventory(&host, &inventory).map_err(|_| Status::failed_precondition("host inventory publication refused"))?;
                                let mut sessions = self.sessions.lock().map_err(|_| denied())?;
                                let s = sessions.get_mut(&host).ok_or_else(denied)?;
                                if s.view.session_id != id || s.view.reconciled || s.inventory.is_some() { return Err(denied()); }
                                s.view.domains = inventory.domains.iter().map(DomainView::of).collect();
                                s.view.profiles = inventory.profiles.iter().map(ProfileView::of).collect();
                                // Only an inventory `publish` accepted reaches here.
                                s.prepared = crate::host_publication::eligible(&inventory);
                                s.inventory = Some(*inventory);
                            }
                            After::Refresh(inventory, drifts) => {
                                for (name, registered, observed) in drifts {
                                    // Evidence only: a journal that cannot record it never ends the session.
                                    let _ = self.authority.record_installation_drift(&host, &name, &registered, &observed);
                                }
                                let mut sessions = self.sessions.lock().map_err(|_| denied())?;
                                let s = sessions.get_mut(&host).ok_or_else(denied)?;
                                if s.view.session_id != id { return Err(denied()); }
                                s.view.domains = inventory.domains.iter().map(DomainView::of).collect();
                                s.view.profiles = inventory.profiles.iter().map(ProfileView::of).collect();
                                s.inventory = Some(*inventory);
                            }
                            After::Result(result) => {
                                self.receive_result(&host, &id, *result, received_at).map_err(|status| *status)?;
                            }
                            After::Provision(result) => {
                                self.receive_provision(&host, *result).map_err(|status| *status)?;
                            }
                        }
                        if let Some(exit) = exited {
                            // SPEC §13.2 (W13): the exited instance's dispatch closes
                            // before this session reads another message. Store work
                            // is blocking.
                            let hook = self.exit_hook.lock().ok().and_then(|hook| hook.clone());
                            if let Some(hook) = hook {
                                let named = host.clone();
                                tokio::task::spawn_blocking(move || hook(&named, &exit))
                                    .await
                                    .map_err(|_| Status::internal("engine exit handling failed"))?;
                            }
                            self.changed();
                        }
                        if draining {
                            self.changed();
                            let hook = self.drain_hook.lock().ok().and_then(|hook| hook.clone());
                            if let Some(hook) = hook {
                                let named = host.clone();
                                // Store writes are blocking work; the ack waits for them.
                                tokio::task::spawn_blocking(move || hook(&named))
                                    .await
                                    .map_err(|_| Status::internal("drain suspension failed"))?;
                            }
                            send_reply(&outgoing, pb::ServerToAgent { msg: Some(server_to_agent::Msg::HostDrainAcknowledged(pb::HostDrainAcknowledged { host_id: host.clone() })) }).await.map_err(|status| *status)?;
                        }
                        if ready {
                            reconciled = true;
                            self.changed();
                            // Owner decision 2026-09-23: only a host that declared
                            // heartbeats is asked for them; an older peer keeps
                            // working, detected only on session loss.
                            let (heartbeat_interval_ms, heartbeat_lost_after_ms) = if heartbeats {
                                (millis(policy.interval), millis(policy.lost_after))
                            } else {
                                eprintln!("host {host} control session {id}: the host sends no heartbeats; a frozen host is detected only when its session is lost");
                                (0, 0)
                            };
                            send_reply(&outgoing, pb::ServerToAgent { msg: Some(server_to_agent::Msg::SessionReady(pb::SessionReady { controller_id: self.authority.controller_id(), session_id: id.clone(), heartbeat_interval_ms, heartbeat_lost_after_ms })) }).await.map_err(|status| *status)?;
                        }
                    }
                }
            }
        }.await;
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(s) = sessions.get_mut(&host).filter(|s| s.view.session_id == id) {
                s.view.online = false;
                s.view.reconciled = false;
                s.view.eligible = false;
                s.view.unresponsive = false;
                s.unresponsive = false;
                s.outgoing.take();
                // SPEC §13.2: a lost session leaves no current load for its host.
                self.load.forget_host(&host);
            }
        }
        // SPEC §13.2: whoever supervises this host's engines learns of the loss now.
        self.changed();
        if let Err(status) = result {
            // SPEC §13: the end of a session retains every claim. The reason is
            // a fixed status phrase for the operator, never command payloads.
            eprintln!("host {host} control session {id} ended: {}", status.message());
            let _ = outgoing.try_send(Err(status));
        }
    }
}
/// Store or observer work one host message needs once the session table is
/// released (SPEC §13).
enum After {
    Nothing,
    Publish(Box<pb::ReportInventory>),
    Refresh(Box<pb::ReportInventory>, Vec<(String, String, String)>),
    Result(Box<pb::MemberExecutionResult>),
    Provision(Box<pb::IngressProvisioned>),
}
fn now() -> i64 {
    mllm_protocol::now_unix_ms() / 1000
}
fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
/// Owner decision 4: a host with a pending drain (or whose drain state cannot
/// be read) is reported not eligible.
fn with_drain(
    mut view: HostSessionView,
    pending: Option<&std::collections::BTreeSet<String>>,
) -> HostSessionView {
    view.drain_pending = pending.is_none_or(|pending| pending.contains(&view.host_id));
    view.eligible &= !view.drain_pending && !view.unresponsive;
    view
}
fn denied() -> Status {
    Status::permission_denied("host session authorization failed")
}
#[tonic::async_trait]
impl AgentControl for AgentSessions {
    type SessionStream = ReceiverStream<Result<pb::ServerToAgent, Status>>;
    async fn session(
        &self,
        request: Request<Streaming<pb::AgentToServer>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let peer = request
            .peer_certs()
            .and_then(|p| p.first().map(|p| p.as_ref().to_vec()))
            .ok_or_else(denied)?;
        let authorized = self
            .authority
            .authorize_peer(&request, "")
            .map_err(|_| denied())?;
        let mut incoming = request.into_inner();
        let connect = tokio::time::timeout(Duration::from_secs(10), incoming.message())
            .await
            .map_err(|_| denied())??
            .ok_or_else(denied)?;
        let Some(agent_to_server::Msg::Connect(connect)) = connect.msg else {
            return Err(denied());
        };
        if connect.host_id != authorized.host_id
            || !mllm_protocol::compatible_peer(&connect.protocol_version)
            || !connect.journal_resume_token.is_empty()
        {
            return Err(denied());
        }
        let host = connect.host_id;
        let id = ulid::Ulid::new().to_string();
        let (outgoing, receiver) = mpsc::channel(CAPACITY);
        let (cancel, cancellation) = tokio::sync::watch::channel(false);
        let mut replaced = false;
        {
            let mut sessions = self.sessions.lock().map_err(|_| denied())?;
            if let Some(old) = sessions.insert(
                host.clone(),
                Session {
                    view: HostSessionView {
                        host_id: host.clone(),
                        session_id: id.clone(),
                        online: true,
                        reconciled: false,
                        eligible: false,
                        drain_pending: false,
                        unresponsive: false,
                        domains: vec![],
                        profiles: vec![],
                    },
                    peer: peer.clone(),
                    outgoing: Some(outgoing.clone()),
                    cancel,
                    inventory: None,
                    history: vec![],
                    prepared: false,
                    draining: false,
                    heartbeats: connect.heartbeats,
                    unresponsive: false,
                },
            ) {
                let _ = old.cancel.send(true);
                replaced = true;
            }
        }
        // SPEC §§10, 13.2: the replaced session's load samples go with it; the
        // new session has proven nothing yet. (Its own teardown no longer
        // names the host's current session, so it would not drop them.)
        if replaced {
            self.load.forget_host(&host);
        }
        self.changed();
        self.tasks.track(tokio::spawn(
            self.clone()
                .serve(host, id, peer, incoming, outgoing, cancellation),
        ));
        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}

impl crate::coordinator::ServiceObservation for AgentSessions {
    /// ADR 0013 §4 step 1 (W12): a host is a placement candidate only while its
    /// session is live and reconciled, it is not draining, and its approved
    /// configuration carries a profile whose build it reported. A revoked host
    /// has no session.
    ///
    /// Owner decision 4 (2026-09-22): nor while a drain of it has a Stop that
    /// has not settled (`Store::host_drain_pending`). A store that cannot be
    /// read fails closed: no host is a candidate.
    fn eligible_hosts(&self) -> Option<std::collections::BTreeSet<String>> {
        // Read before the session lock: the store lock is never taken inside it.
        let Some(pending) = self.authority.hosts_with_pending_drain() else {
            return Some(Default::default());
        };
        Some(
            self.sessions
                .lock()
                .map(|sessions| {
                    sessions
                        .iter()
                        .filter(|(host, s)| {
                            s.view.online
                                && s.view.reconciled
                                && s.view.eligible
                                && !s.draining
                                && !s.unresponsive
                                && !pending.contains(*host)
                        })
                        .map(|(host, _)| host.clone())
                        .collect()
                })
                .unwrap_or_default(),
        )
    }
    /// Owner decision 4: hosts with a live, reconciled session, to which a
    /// cleanup Terminate can be delivered now.
    fn online_hosts(&self) -> Option<std::collections::BTreeSet<String>> {
        Some(
            self.sessions
                .lock()
                .map(|sessions| {
                    sessions
                        .iter()
                        .filter(|(_, s)| s.view.online && s.view.reconciled)
                        .map(|(host, _)| host.clone())
                        .collect()
                })
                .unwrap_or_default(),
        )
    }
    fn observe(&self, host_id: String) -> crate::coordinator::ObservationFuture {
        let observed = self.observe_with_residents(host_id);
        Box::pin(async move { observed.await.map(|(observed, _)| observed) })
    }
    /// ADR 0007: the processes the host sampled in the same report as its
    /// availability (one report, so the two are coherent).
    fn observe_with_residents(&self, host_id: String) -> crate::coordinator::ResidentObservationFuture {
        let current = self.view(&host_id);
        Box::pin(async move {
            let host = current.filter(|h| h.online && h.reconciled)
                .ok_or_else(|| crate::coordinator::CoordinatorError::Service("remote memory observation unavailable".into()))?;
            let residents = host.domains.iter().flat_map(|d| d.residents.iter().cloned()).collect();
            let observed = host.domains.into_iter().map(|d| mllm_domain::resources::MemoryObservation {
                domain: mllm_config::remote_resources::ledger_key(&host_id, "domain", &d.domain_id),
                capacity_bytes: d.capacity_bytes, available_bytes: d.available_bytes,
                sampled_at_ms: d.observed_at_unix_ms,
            }).collect();
            Ok((observed, residents))
        })
    }
}

#[cfg(test)]
mod installation_tests {
    use super::*;

    fn measured() -> pb::RuntimeProfileStatus {
        pb::RuntimeProfileStatus {
            name: "local".into(),
            build_fingerprint: "sglang-0.5.20".into(),
            eligibility: "unknown".into(),
            installation_version: "0.5.20+custom".into(),
            installation_digest: format!("sha256:{}", "a".repeat(64)),
            installation_state: "measured".into(),
            ..Default::default()
        }
    }

    // T21 T22: ADR 0008 (owner decision 2026-09-23). A reconciled host may
    // report drift and probe results, never a different registration; a new
    // drift is reported once for the event journal.
    #[test]
    fn drift_and_capabilities_may_change_but_the_registration_may_not() {
        let old = vec![measured()];
        let mut new = old.clone();
        new[0].installation_state = "drifted".into();
        new[0].installation_observed_digest = format!("sha256:{}", "b".repeat(64));
        new[0].capabilities_missing = vec!["deep_park".into()];
        assert!(installation_fields_valid(&new[0]));
        assert!(same_registration(&new, &old));
        assert_eq!(
            newly_drifted(&new, &old),
            vec![(
                "local".to_owned(),
                old[0].installation_digest.clone(),
                new[0].installation_observed_digest.clone()
            )]
        );
        // The same drift reported again is not new.
        assert!(newly_drifted(&new, &new).is_empty());
        let mut moved = old.clone();
        moved[0].installation_digest = format!("sha256:{}", "c".repeat(64));
        assert!(!same_registration(&moved, &old));
        for bad in [
            ("state", "patched"),
            ("digest", "md5:abc"),
            ("observed", "sha256:short"),
            ("capability", "everything"),
        ] {
            let mut profile = measured();
            match bad.0 {
                "state" => profile.installation_state = bad.1.into(),
                "digest" => profile.installation_digest = bad.1.into(),
                "observed" => profile.installation_observed_digest = bad.1.into(),
                _ => profile.capabilities_missing = vec![bad.1.into()],
            }
            assert!(!installation_fields_valid(&profile), "{bad:?}");
        }
        // An older host publishes none of it.
        assert!(installation_fields_valid(&pb::RuntimeProfileStatus::default()));
    }
}

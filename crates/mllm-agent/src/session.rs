//! SPEC §13: outbound authenticated sessions reconcile retained journal state
//! before connecting the local execution fence. Transport alone grants no effects.
use crate::{
    enrollment::PendingEnrollment,
    journal::{CommandState, HostJournal},
};
use mllm_protocol::pb::{
    self, agent_control_client::AgentControlClient, agent_to_server, server_to_agent,
};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
#[derive(Debug, thiserror::Error)]
#[error("host session unavailable")]
pub struct SessionError;
/// SPEC §4.1, ADR 0016: the controller answered, over the host's mutual-TLS
/// control session, that this host's certificate is revoked. Reconnecting
/// cannot heal that; only `invite host --recover` and `join host --recover`
/// can. The host's engines are left exactly as they are.
#[derive(Debug, thiserror::Error)]
#[error("the controller revoked this host's certificate")]
pub struct HostRevoked;
fn frame(msg: agent_to_server::Msg) -> pb::AgentToServer {
    pb::AgentToServer { msg: Some(msg) }
}
pub type ExecutionFuture =
    Pin<Box<dyn Future<Output = Result<pb::MemberExecutionResult, SessionError>> + Send>>;
pub type LoadFuture = Pin<Box<dyn Future<Output = Vec<pb::ReportLoad>> + Send>>;
pub type ExitFuture = Pin<Box<dyn Future<Output = Vec<pb::MemberExit>> + Send>>;
/// The host implementation owns local policy, durable journal acceptance and
/// native adapters. Transport cannot render commands or mint launch authority.
pub trait SessionExecution: Send + Sync {
    fn execute(
        &self,
        session: u64,
        command: mllm_protocol::execution::MemberCommand,
    ) -> ExecutionFuture;
    fn inventory(&self) -> Option<pb::ReportInventory> {
        None
    }
    /// The inventory a new session publishes first. Unlike [`Self::inventory`]
    /// it may block, bounded, for a fresh device sample: it runs off the
    /// session loop, before any heartbeat is due. Publication refuses an
    /// unobserved or stale domain, so a first inventory that raced the GPU
    /// collector ended every host's first session (found on a fresh
    /// discrete-GPU host, 2026-09-26).
    fn session_inventory(&self) -> Option<pb::ReportInventory> {
        self.inventory()
    }
    fn connected(&self, _session: u64) -> Result<(), SessionError> {
        Ok(())
    }
    fn disconnected(&self, _session: u64) {}
    /// SPEC §10, D9: one tick of engine load reports for this host's Ready
    /// scopes. `None` when the host reports no load. Sent only on a reconciled
    /// session; a full outbound queue drops the tick, never the session.
    fn load_reports(&self) -> Option<LoadFuture> {
        None
    }
    /// The load reporting period; clamped to the D9 bounds (250 ms to 5 s).
    fn load_interval(&self) -> Duration {
        crate::load::DEFAULT_LOAD_INTERVAL
    }
    /// SPEC §13.2 (W13): every Ready launch with an owned process exited now.
    /// `None` when the host watches no engines. Sent only on a reconciled
    /// session, each at once and then again every `EXIT_RESEND_INTERVAL` while
    /// it stands; a full outbound queue defers it to the next scan.
    fn member_exits(&self) -> Option<ExitFuture> {
        None
    }

    fn provision(
        &self,
        _command: mllm_protocol::execution::MemberCommand,
        _gate_key: [u8; 32],
    ) -> Pin<Box<dyn Future<Output = Result<Provisioned, SessionError>> + Send>> {
        Box::pin(async { Err(SessionError) })
    }
}
/// What a private ingress provision established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provisioned {
    /// The key is in protected host storage.
    Stored,
    /// SPEC §13: host policy refused the launch before any effect; no key was
    /// stored. The closed reason reaches the controller, and the session stays.
    Refused(&'static str),
}
/// SPEC §4.3 (Phase B follow-up): a host beginning a graceful shutdown tells the
/// controller first, so the controller suspends dispatch to this host's engines
/// before the host closes its ingress. Without it the router kept dispatching
/// into a closing ingress and its clients saw 500 instead of a retryable 503.
/// The notice carries no authority: it only closes dispatch on the controller.
pub struct DrainSignal {
    requested: watch::Sender<bool>,
    acknowledged: watch::Sender<bool>,
    /// Whether a reconciled control session is up right now.
    connected: watch::Sender<bool>,
}

/// What announcing a drain established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrainAnnouncement {
    /// The controller acknowledged: dispatch to this host is suspended.
    Acknowledged,
    /// No control session was up, so the controller already treats this host's
    /// engines as unproven and dispatches nothing to them.
    NotConnected,
    /// No acknowledgement inside the bound. The host drains anyway; a request
    /// its ingress refuses is refused with a retryable 503 before forwarding.
    Unacknowledged,
}

impl DrainAnnouncement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Acknowledged => "acknowledged",
            Self::NotConnected => "not_connected",
            Self::Unacknowledged => "unacknowledged",
        }
    }
}

impl DrainSignal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            requested: watch::channel(false).0,
            acknowledged: watch::channel(false).0,
            connected: watch::channel(false).0,
        })
    }

    /// Ask the controller to suspend this host's dispatch and wait, at most
    /// `bound`, for its acknowledgement.
    pub async fn announce(&self, bound: Duration) -> DrainAnnouncement {
        self.requested.send_replace(true);
        let mut acknowledged = self.acknowledged.subscribe();
        let mut connected = self.connected.subscribe();
        let wait = async {
            loop {
                if *acknowledged.borrow_and_update() {
                    return DrainAnnouncement::Acknowledged;
                }
                if !*connected.borrow_and_update() {
                    return DrainAnnouncement::NotConnected;
                }
                tokio::select! {
                    changed = acknowledged.changed() => if changed.is_err() { return DrainAnnouncement::Unacknowledged },
                    changed = connected.changed() => if changed.is_err() { return DrainAnnouncement::Unacknowledged },
                }
            }
        };
        tokio::time::timeout(bound, wait)
            .await
            .unwrap_or(DrainAnnouncement::Unacknowledged)
    }
}

/// Resolves once a drain is requested; never, when there is no signal.
async fn drain_requested(requested: &mut Option<watch::Receiver<bool>>) {
    match requested {
        Some(receiver) => {
            if receiver.wait_for(|requested| *requested).await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

/// Marks the session disconnected for the drain signal when it ends.
struct Connected(Option<Arc<DrainSignal>>);
impl Drop for Connected {
    fn drop(&mut self) {
        if let Some(drain) = &self.0 {
            drain.connected.send_replace(false);
        }
    }
}

/// ADR 0018 §3, §4: requests from the local control handler to the live
/// session, and what the session learned of its server.
pub struct ProfileUpdates {
    profiles: Arc<crate::profiles::HostProfiles>,
    sender: mpsc::Sender<ProfileRequest>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<ProfileRequest>>,
    connected: watch::Sender<bool>,
    server: watch::Sender<std::collections::BTreeSet<String>>,
}

enum ProfileRequest {
    Publish {
        request_id: String,
        reply: tokio::sync::oneshot::Sender<PublishOutcome>,
    },
    Retire {
        request_id: String,
        profile: String,
        drain: bool,
        reply: mpsc::Sender<pb::ProfileRetirement>,
    },
}

/// How a live publication ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The server accepted it; the set is now the accepted one.
    Accepted,
    /// The server refused it, saying why; the previous set stays.
    Rejected(String),
    /// ADR 0017: the server does not take live updates; a role restart
    /// publishes the document.
    RestartRequired,
    /// No reconciled session; the next session publishes the accepted set.
    NotConnected,
    /// The session ended, or no verdict came inside the bound.
    SessionEnded,
    /// Another publication is waiting for its verdict.
    Busy,
}

/// How a retirement request ended. `draining` is progress, never an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetireOutcome {
    Confirmed,
    InUse(Vec<String>),
    Holding(Vec<String>),
    Refused(String),
    RestartRequired,
    NotConnected,
    SessionEnded,
}

impl ProfileUpdates {
    pub fn new(profiles: Arc<crate::profiles::HostProfiles>) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(4);
        Arc::new(Self {
            profiles,
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
            connected: watch::channel(false).0,
            server: watch::channel(Default::default()).0,
        })
    }
    pub fn profiles(&self) -> &Arc<crate::profiles::HostProfiles> {
        &self.profiles
    }
    /// Whether a reconciled control session is up right now.
    pub fn connected(&self) -> bool {
        *self.connected.borrow()
    }
    /// ADR 0017: whether the connected server's SessionReady listed
    /// `live_profile_update`.
    pub fn server_supports(&self) -> bool {
        self.server
            .borrow()
            .contains(mllm_protocol::capabilities::LIVE_PROFILE_UPDATE)
    }
    /// What the session learned: `Some(capabilities)` once SessionReady
    /// arrives, `None` when the session ends.
    pub fn observe_session(&self, capabilities: Option<&[String]>) {
        match capabilities {
            Some(capabilities) => {
                self.server
                    .send_replace(capabilities.iter().cloned().collect());
                self.connected.send_replace(true);
            }
            None => {
                self.connected.send_replace(false);
                self.server.send_replace(Default::default());
            }
        }
    }

    /// ADR 0018 §3: stage `set`, send it, and settle on the server's verdict.
    pub async fn publish(
        &self,
        set: crate::profiles::ProfileSet,
        bound: Duration,
    ) -> PublishOutcome {
        if !self.connected() {
            return PublishOutcome::NotConnected;
        }
        if !self.server_supports() {
            return PublishOutcome::RestartRequired;
        }
        let request_id = ulid::Ulid::new().to_string();
        if self.profiles.stage(&request_id, set).is_err() {
            return PublishOutcome::Busy;
        }
        let (reply, answer) = tokio::sync::oneshot::channel();
        let sent = self
            .sender
            .send(ProfileRequest::Publish {
                request_id: request_id.clone(),
                reply,
            })
            .await;
        let outcome = match (sent, tokio::time::timeout(bound, answer).await) {
            (Ok(()), Ok(Ok(outcome))) => outcome,
            _ => PublishOutcome::SessionEnded,
        };
        if outcome != PublishOutcome::Accepted {
            // Verdicts were settled by the session loop; anything else leaves
            // nothing staged. A verdict arriving after this ends the session
            // (see the loop), so the host's accepted set stays authoritative.
            self.profiles.settle(&request_id, false);
        }
        outcome
    }

    /// ADR 0018 §4: ask the server to retire `profile`; wait for a terminal
    /// answer (`draining` is progress, not an answer).
    pub async fn retire(&self, profile: &str, drain: bool, bound: Duration) -> RetireOutcome {
        if !self.connected() {
            return RetireOutcome::NotConnected;
        }
        if !self.server_supports() {
            return RetireOutcome::RestartRequired;
        }
        let (reply, mut answers) = mpsc::channel(4);
        let request = ProfileRequest::Retire {
            request_id: ulid::Ulid::new().to_string(),
            profile: profile.into(),
            drain,
            reply,
        };
        if self.sender.send(request).await.is_err() {
            return RetireOutcome::SessionEnded;
        }
        let wait = async {
            while let Some(answer) = answers.recv().await {
                match answer.outcome.as_str() {
                    "draining" => continue,
                    "confirmed" => return RetireOutcome::Confirmed,
                    "in_use" => return RetireOutcome::InUse(answer.deployments),
                    "holding" => return RetireOutcome::Holding(answer.deployments),
                    _ => return RetireOutcome::Refused(answer.reason),
                }
            }
            RetireOutcome::SessionEnded
        };
        tokio::time::timeout(bound, wait)
            .await
            .unwrap_or(RetireOutcome::SessionEnded)
    }
}

/// Marks the session disconnected for the control handler when it ends.
struct Disconnected(Option<Arc<ProfileUpdates>>);
impl Drop for Disconnected {
    fn drop(&mut self) {
        if let Some(updates) = &self.0 {
            updates.observe_session(None);
        }
    }
}

/// Cancellation is local and never releases retained resource claims.
pub async fn run_session(
    identity: &PendingEnrollment,
    journal: Arc<HostJournal>,
    inventory: pb::ReportInventory,
    shutdown: watch::Receiver<bool>,
) -> Result<(), HostRevoked> {
    run_session_with_execution(identity, journal, inventory, shutdown, None).await
}

pub async fn run_session_with_execution(
    identity: &PendingEnrollment,
    journal: Arc<HostJournal>,
    inventory: pb::ReportInventory,
    shutdown: watch::Receiver<bool>,
    execution: Option<Arc<dyn SessionExecution>>,
) -> Result<(), HostRevoked> {
    run_session_with_drain(identity, journal, inventory, shutdown, execution, None).await
}

/// As `run_session_with_execution`, also carrying the host's drain notice to
/// the controller on every session (SPEC §4.3).
///
/// Every session end is retried with backoff (an unreachable or restarting
/// controller, a version refusal, any generic refusal), except the one
/// authoritative answer that this host's certificate is revoked
/// (SPEC §4.1, ADR 0016): then the loop returns [`HostRevoked`] at once,
/// logging nothing itself, so the role can say so once and exit.
pub async fn run_session_with_drain(
    identity: &PendingEnrollment,
    journal: Arc<HostJournal>,
    inventory: pb::ReportInventory,
    shutdown: watch::Receiver<bool>,
    execution: Option<Arc<dyn SessionExecution>>,
    drain: Option<Arc<DrainSignal>>,
) -> Result<(), HostRevoked> {
    run_session_with_updates(
        identity, journal, inventory, shutdown, execution, drain, None,
    )
    .await
}

/// As `run_session_with_drain`, also carrying the local control handler's
/// profile publications and retirements (ADR 0018 §3, §4).
pub async fn run_session_with_updates(
    identity: &PendingEnrollment,
    journal: Arc<HostJournal>,
    inventory: pb::ReportInventory,
    mut shutdown: watch::Receiver<bool>,
    execution: Option<Arc<dyn SessionExecution>>,
    drain: Option<Arc<DrainSignal>>,
    updates: Option<Arc<ProfileUpdates>>,
) -> Result<(), HostRevoked> {
    let mut delay = Duration::from_millis(250);
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let ended = tokio::select! {
            _ = shutdown.changed() => { return Ok(()); }
            ended = connect_once(identity, journal.clone(), inventory.clone(), execution.clone(), drain.clone(), updates.clone()) => ended,
        };
        // SPEC §13: a session end retains every claim and grants nothing. Its
        // reason is operator diagnostics only: a fixed phrase, never a command
        // payload, key or credential.
        match ended {
            Ok(()) => {}
            // SPEC §4.1, ADR 0016: nothing is stopped or signalled here; the
            // session's fence has journaled the disconnect like any other end.
            Err(SessionEnd::Revoked) => return Err(HostRevoked),
            Err(reason) => mllm_domain::role_log::notice(
                mllm_domain::role_log::Level::Notice,
                &format!("host control session ended: {reason}; reconnecting"),
            ),
        }
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(Duration::from_secs(10));
    }
}
/// Why one control session ended, as a fixed phrase safe for operator logs.
#[derive(Debug)]
enum SessionEnd {
    /// Retried with backoff.
    Ended(std::borrow::Cow<'static, str>),
    /// SPEC §4.1, ADR 0016: the controller's authoritative revocation answer.
    Revoked,
}
impl SessionEnd {
    fn fixed(reason: &'static str) -> Self {
        Self::Ended(std::borrow::Cow::Borrowed(reason))
    }
}
impl std::fmt::Display for SessionEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ended(reason) => f.write_str(reason),
            Self::Revoked => f.write_str("the controller revoked this host's certificate"),
        }
    }
}
/// ADR 0017: a controller older than this host refuses the session and says
/// so; the operator must upgrade the server first. The reconnect loop keeps
/// retrying with its backoff, so the host connects once the server is
/// upgraded. The logged reason is the controller's fixed policy sentence
/// (two version numbers), bounded and printable, never a payload.
///
/// SPEC §4.1, ADR 0016: the controller's exact revocation refusal ends the
/// reconnect loop. It can only arrive here as the answer on this host's own
/// control channel, which is mutual TLS against the controller CA pinned at
/// enrollment; a look-alike or garbled refusal stays a generic one.
fn refused_session(status: tonic::Status) -> SessionEnd {
    if mllm_protocol::is_host_revoked_refusal(&status) {
        return SessionEnd::Revoked;
    }
    let message = status.message();
    if status.code() == tonic::Code::FailedPrecondition
        && message.starts_with(mllm_protocol::version::NEWER_HOST_REFUSAL)
        && message.len() <= 512
        && message.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
    {
        return SessionEnd::Ended(std::borrow::Cow::Owned(format!(
            "the controller refused this host's version ({message}); upgrade the server first"
        )));
    }
    SessionEnd::fixed("controller refused the session")
}
fn end<E>(reason: &'static str) -> impl FnOnce(E) -> SessionEnd {
    move |_| SessionEnd::fixed(reason)
}
struct Fence {
    journal: Arc<HostJournal>,
    session: u64,
    execution: Option<Arc<dyn SessionExecution>>,
}
impl Drop for Fence {
    fn drop(&mut self) {
        if let Some(execution) = &self.execution {
            execution.disconnected(self.session);
        }
        let _ = self.journal.disconnect(self.session);
    }
}
/// The inventory a new session publishes: the executor's (with a fresh
/// device sample), else the accepted profile set's with its measured domains
/// read again, else the startup snapshot. SPEC §§4.2, 13: publication refuses
/// an observation older than the policy's TTL, and the startup snapshot is
/// taken before the installations are measured, which on a real engine
/// environment takes longer than that TTL.
fn first_inventory(
    execution: Option<&dyn SessionExecution>,
    updates: Option<&ProfileUpdates>,
    startup: pb::ReportInventory,
) -> pb::ReportInventory {
    if let Some(inventory) = execution.and_then(|e| e.session_inventory()) {
        return inventory;
    }
    let Some(updates) = updates else {
        return startup;
    };
    let set = updates.profiles.accepted();
    let mut inventory = set.inventory.clone();
    if let Ok(reading) = crate::memory::read_host_memory() {
        let document = &set.config.document;
        let gpu = (!crate::device_domains::device_domains(document).is_empty())
            .then(crate::gpu_memory::sample)
            .flatten();
        inventory.domains =
            crate::device_domains::startup_domains(document, &reading.memory, gpu.as_ref());
    }
    inventory
}

async fn connect_once(
    identity: &PendingEnrollment,
    journal: Arc<HostJournal>,
    startup_inventory: pb::ReportInventory,
    execution: Option<Arc<dyn SessionExecution>>,
    drain: Option<Arc<DrainSignal>>,
    updates: Option<Arc<ProfileUpdates>>,
) -> Result<(), SessionEnd> {
    let host = identity
        .host_id()
        .ok_or(SessionEnd::fixed("host enrollment is incomplete"))?
        .to_owned();
    let endpoint = identity
        .control_endpoint(mllm_protocol::now_unix_ms() / 1000)
        .map_err(end("host certificate is not currently valid"))?;
    let channel = endpoint
        .connect()
        .await
        .map_err(end("control endpoint unreachable"))?;
    let (send, receive) = mpsc::channel(16);
    send.send(frame(agent_to_server::Msg::Connect(pb::Connect {
        host_id: host.clone(),
        protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
        journal_resume_token: vec![],
        envelope: None,
        // Owner decision 2026-09-23: this agent sends and accepts heartbeats.
        heartbeats: true,
        // ADR 0008: this agent executes MaterializeSource.
        model_sources: true,
        // ADR 0017: the server judges this build's version against its own
        // and sends only the post-baseline features declared here.
        binary_version: mllm_protocol::version::BINARY_VERSION.into(),
        capabilities: mllm_protocol::capabilities::agent_capabilities(),
    })))
    .await
    .map_err(end("outbound stream closed"))?;
    let mut stream = AgentControlClient::new(channel)
        .max_decoding_message_size(65536)
        .max_encoding_message_size(65536)
        .session(ReceiverStream::new(receive))
        .await
        .map_err(refused_session)?
        .into_inner();
    // SPEC §§4.2, 13: publication refuses a domain observation older than the
    // host policy's observation TTL, so every connect reports a fresh
    // measurement. The startup snapshot is only a fallback for hosts that
    // publish no measured domains; resending it on a reconnect minutes later
    // made every reconnect fail publication (found live, U5 on host-a).
    // ADR 0018 §3: without a measured refresh, the accepted profile set's
    // inventory, so a reconnect publishes what the host now holds.
    let mut inventory = tokio::task::spawn_blocking({
        let execution = execution.clone();
        let updates = updates.clone();
        move || first_inventory(execution.as_deref(), updates.as_deref(), startup_inventory)
    })
    .await
    .map_err(end("inventory measurement failed"))?;
    inventory.envelope = Some(pb::Envelope {
        host_id: host.clone(),
        protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
        ..Default::default()
    });
    send.send(frame(agent_to_server::Msg::ReportInventory(inventory)))
        .await
        .map_err(end("outbound stream closed"))?;
    let producer_journal = journal.clone();
    let reports = send.clone();
    let producer = tokio::spawn(async move {
        let mut cursor = 0;
        loop {
            let records = producer_journal
                .history(cursor, 128)
                .map_err(|_| SessionError)?;
            let complete = records.is_empty();
            let records = records
                .into_iter()
                .map(|r| {
                    cursor = r.sequence;
                    pb::JournalSummary {
                        sequence: r.sequence,
                        command_id: r.command_id,
                        state: match r.state {
                            CommandState::Accepted => "accepted",
                            CommandState::Attempted => "attempted",
                            CommandState::Launched => "launched",
                            CommandState::Completed => "completed",
                            CommandState::Tombstone => "tombstone",
                        }
                        .into(),
                        claim_retained: r.claim_retained,
                    }
                })
                .collect();
            send.send(frame(agent_to_server::Msg::ReconcileHistory(
                pb::ReconcileHistory { records, complete },
            )))
            .await
            .map_err(|_| SessionError)?;
            if complete {
                break;
            }
        }
        // Keep outbound stream open independently of server reads.
        send.closed().await;
        Ok::<(), SessionError>(())
    });
    struct Producer(tokio::task::JoinHandle<Result<(), SessionError>>);
    impl Drop for Producer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _producer = Producer(producer);
    // SPEC §10, D9: load reports start once the session is reconciled and
    // stop with it. They are routing hints: a full queue skips a tick.
    let (load_ready, mut load_gate) = watch::channel(false);
    // SPEC §13.2 (W13): owned engine exits, reported promptly once the session
    // is reconciled and repeated while they stand; they stop with the session.
    let mut exit_gate = load_ready.subscribe();
    let _exits = execution.clone().map(|execution| {
        let reports = reports.clone();
        Producer(tokio::spawn(async move {
            if exit_gate.wait_for(|ready| *ready).await.is_err() {
                return Ok(());
            }
            let mut sent = std::collections::BTreeMap::<String, tokio::time::Instant>::new();
            let mut tick = tokio::time::interval(crate::exits::EXIT_SCAN_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(scan) = execution.member_exits() else {
                    return Ok(());
                };
                let exits = scan.await;
                sent.retain(|handle, _| exits.iter().any(|exit| &exit.owned_handle == handle));
                for exit in exits {
                    if sent
                        .get(&exit.owned_handle)
                        .is_some_and(|at| at.elapsed() < crate::exits::EXIT_RESEND_INTERVAL)
                    {
                        continue;
                    }
                    let handle = exit.owned_handle.clone();
                    match reports.try_send(frame(agent_to_server::Msg::MemberExit(exit))) {
                        Ok(()) => {
                            sent.insert(handle, tokio::time::Instant::now());
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => break,
                        Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                    }
                }
            }
        }))
    });
    let _load = execution.clone().map(|execution| {
        let reports = reports.clone();
        Producer(tokio::spawn(async move {
            if load_gate.wait_for(|ready| *ready).await.is_err() {
                return Ok(());
            }
            let period = execution.load_interval().clamp(
                crate::load::MIN_LOAD_INTERVAL,
                crate::load::MAX_LOAD_INTERVAL,
            );
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(sampled) = execution.load_reports() else {
                    return Ok(());
                };
                for report in sampled.await {
                    match reports.try_send(frame(agent_to_server::Msg::ReportLoad(report))) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => break,
                        Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                    }
                }
            }
        }))
    });
    let mut fence = None;
    let mut effects =
        tokio::task::JoinSet::<(String, Result<agent_to_server::Msg, SessionError>)>::new();
    // SPEC §13: the controller redelivers the same immutable command at least
    // once until it observes a terminal result. A redelivery that arrives while
    // that command's effect is still running is the same request, not new
    // work: the running effect reports for it. Spawning a parallel replay per
    // redelivery filled the effect bound during a slow native launch and tore
    // the session down, which aborted the launch mid-readiness (found live,
    // U5 on host-a).
    let mut in_flight = std::collections::BTreeSet::<String>::new();
    let mut observations = tokio::time::interval(Duration::from_millis(500));
    // SPEC §4.3: the drain notice is sent once per reconciled session.
    let _connected = Connected(drain.clone());
    let mut requested = drain.as_ref().map(|drain| drain.requested.subscribe());
    let mut announced = false;
    // Owner decision 2026-09-23: once the controller's SessionReady asks for
    // heartbeats, send one each period and end the session when nothing has
    // been heard from the controller for its lost bound. Ending the session
    // leaves engines and claims exactly as any other session end does.
    let mut lost_after: Option<Duration> = None;
    let mut last_heard = tokio::time::Instant::now();
    let mut beat = tokio::time::interval(Duration::from_secs(1));
    // ADR 0018: requests from the control handler, held for this session.
    let mut requests = match &updates {
        Some(u) => Some(u.receiver.lock().await),
        None => None,
    };
    let mut publishing =
        std::collections::BTreeMap::<String, tokio::sync::oneshot::Sender<PublishOutcome>>::new();
    let mut retiring =
        std::collections::BTreeMap::<String, mpsc::Sender<pb::ProfileRetirement>>::new();
    let _disconnected = Disconnected(updates.clone());
    loop {
        let message = tokio::select! {
            _ = drain_requested(&mut requested), if fence.is_some() && !announced => {
                announced = true;
                reports
                    .try_send(frame(agent_to_server::Msg::HostDraining(pb::HostDraining { host_id: host.clone() })))
                    .map_err(end("outbound report queue is full"))?;
                continue;
            },
            request = async {
                match requests.as_mut() {
                    Some(r) => r.recv().await,
                    None => std::future::pending().await,
                }
            }, if fence.is_some() => {
                match request {
                    // A publication whose caller already gave up is never sent.
                    Some(ProfileRequest::Publish { reply, .. }) if reply.is_closed() => {}
                    Some(ProfileRequest::Publish { request_id, reply }) => {
                        let staged = updates
                            .as_ref()
                            .and_then(|u| u.profiles.pending())
                            .filter(|(id, _)| *id == request_id);
                        let Some((_, set)) = staged else {
                            let _ = reply.send(PublishOutcome::SessionEnded);
                            continue;
                        };
                        let mut inventory = set.inventory.clone();
                        inventory.envelope = Some(pb::Envelope { host_id: host.clone(), protocol_version: mllm_protocol::PROTOCOL_VERSION.into(), ..Default::default() });
                        reports
                            .try_send(frame(agent_to_server::Msg::PublishProfiles(pb::PublishProfiles { request_id: request_id.clone(), inventory: Some(inventory) })))
                            .map_err(end("outbound report queue is full"))?;
                        publishing.insert(request_id, reply);
                    }
                    // Likewise a retirement (queued across a reconnect): nobody
                    // would finish the removal.
                    Some(ProfileRequest::Retire { reply, .. }) if reply.is_closed() => {}
                    Some(ProfileRequest::Retire { request_id, profile, drain, reply }) => {
                        reports
                            .try_send(frame(agent_to_server::Msg::RetireProfile(pb::RetireProfile { request_id: request_id.clone(), profile, drain })))
                            .map_err(end("outbound report queue is full"))?;
                        retiring.insert(request_id, reply);
                    }
                    None => {}
                }
                continue;
            },
            message = stream.message() => {
                // SPEC §4.1, ADR 0016: a live session the controller closes
                // because this host was revoked carries the same exact refusal.
                let message = message
                    .map_err(|status| {
                        if mllm_protocol::is_host_revoked_refusal(&status) {
                            SessionEnd::Revoked
                        } else {
                            SessionEnd::fixed("controller closed the session with an error")
                        }
                    })?
                    .ok_or(SessionEnd::fixed("controller closed the session"))?;
                last_heard = tokio::time::Instant::now();
                message
            },
            _ = beat.tick(), if lost_after.is_some() => {
                if lost_after.is_some_and(|lost| last_heard.elapsed() >= lost) {
                    return Err(SessionEnd::fixed("controller heartbeats stopped"));
                }
                // A full queue skips a beat; silence is judged by the controller.
                let _ = reports.try_send(frame(agent_to_server::Msg::Heartbeat(pb::Heartbeat {
                    sent_at_unix_ms: mllm_protocol::now_unix_ms(),
                })));
                continue;
            },
            // ADR 0018 §3: no refresh while a publication awaits its verdict;
            // the verdict settles which document the next one describes.
            _ = observations.tick(), if fence.is_some() && !updates.as_ref().is_some_and(|u| u.profiles.publishing()) => {
                if let Some(mut inventory) = execution.as_ref().and_then(|e| e.inventory()) {
                    inventory.envelope = Some(pb::Envelope { host_id: host.clone(), protocol_version: mllm_protocol::PROTOCOL_VERSION.into(), ..Default::default() });
                    reports.try_send(frame(agent_to_server::Msg::ReportInventory(inventory))).map_err(end("outbound report queue is full"))?;
                }
                continue;
            },
            result = effects.join_next(), if !effects.is_empty() => {
                let (key, result) = result
                    .ok_or(SessionEnd::fixed("effect set is empty"))?
                    .map_err(end("a local effect task failed"))?;
                in_flight.remove(&key);
                let result = result.map_err(end("a local effect failed"))?;
                tokio::time::timeout(Duration::from_secs(5), reports.send(frame(result)))
                    .await
                    .map_err(end("outbound report queue stalled"))?
                    .map_err(end("outbound stream closed"))?;
                continue;
            }
        };
        match message.msg {
            Some(server_to_agent::Msg::SessionReady(ready))
                if fence.is_none()
                    && ready.controller_id == identity.controller_id()
                    && !ready.session_id.is_empty() =>
            {
                let connected = Fence {
                    session: journal
                        .connect()
                        .map_err(end("host journal refused the session"))?,
                    journal: journal.clone(),
                    execution: execution.clone(),
                };
                if let Some(execution) = &execution {
                    execution
                        .connected(connected.session)
                        .map_err(end("host execution refused the session"))?;
                }
                fence = Some(connected);
                if ready.heartbeat_interval_ms > 0 && ready.heartbeat_lost_after_ms > 0 {
                    let period = Duration::from_millis(
                        ready.heartbeat_interval_ms.clamp(250, 10_000) as u64,
                    );
                    lost_after = Some(Duration::from_millis(
                        ready.heartbeat_lost_after_ms.clamp(3_000, 600_000) as u64,
                    ));
                    last_heard = tokio::time::Instant::now();
                    beat = tokio::time::interval(period);
                    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                }
                let _ = load_ready.send(true);
                if let Some(drain) = &drain {
                    drain.connected.send_replace(true);
                }
                // ADR 0017, 0018 §3: live updates only to a server that says
                // it takes them.
                if let Some(u) = &updates {
                    u.observe_session(Some(&ready.capabilities));
                }
            }
            Some(server_to_agent::Msg::HostDrainAcknowledged(ack))
                if fence.is_some() && announced && ack.host_id == host =>
            {
                if let Some(drain) = &drain {
                    drain.acknowledged.send_replace(true);
                }
            }
            // Liveness only; hearing it refreshed `last_heard` above.
            Some(server_to_agent::Msg::Heartbeat(_)) if fence.is_some() && lost_after.is_some() => {
            }
            Some(server_to_agent::Msg::ExecuteMember(wire)) if fence.is_some() => {
                let executor = execution
                    .as_ref()
                    .ok_or(SessionEnd::fixed("host has no native execution"))?
                    .clone();
                let command =
                    mllm_protocol::execution::MemberCommand::try_from(pb::ServerToAgent {
                        msg: Some(server_to_agent::Msg::ExecuteMember(wire)),
                    })
                    .map_err(end("controller sent an invalid command"))?;
                command
                    .verify_digest()
                    .map_err(end("controller sent an invalid command"))?;
                let key = format!("execute/{}", command.identity.command_id);
                if in_flight.contains(&key) {
                    continue;
                }
                if effects.len() >= 8 {
                    return Err(SessionEnd::fixed("local effect bound exceeded"));
                }
                let session = fence
                    .as_ref()
                    .ok_or(SessionEnd::fixed("session is not connected"))?
                    .session;
                in_flight.insert(key.clone());
                effects.spawn(async move {
                    (
                        key,
                        executor
                            .execute(session, command)
                            .await
                            .map(agent_to_server::Msg::MemberResult),
                    )
                });
            }
            Some(server_to_agent::Msg::ProvisionIngress(provision)) if fence.is_some() => {
                let executor = execution
                    .as_ref()
                    .ok_or(SessionEnd::fixed("host has no native execution"))?
                    .clone();
                let command =
                    mllm_protocol::execution::MemberCommand::try_from(pb::ServerToAgent {
                        msg: Some(server_to_agent::Msg::ExecuteMember(
                            provision
                                .command
                                .ok_or(SessionEnd::fixed("controller sent an invalid provision"))?,
                        )),
                    })
                    .map_err(end("controller sent an invalid provision"))?;
                command
                    .verify_digest()
                    .map_err(end("controller sent an invalid provision"))?;
                let gate: [u8; 32] = provision
                    .gate_key
                    .try_into()
                    .map_err(end("controller sent an invalid provision"))?;
                let key = format!("provision/{}", command.identity.command_id);
                if in_flight.contains(&key) {
                    continue;
                }
                if effects.len() >= 8 {
                    return Err(SessionEnd::fixed("local effect bound exceeded"));
                }
                let id = command.to_wire().identity;
                in_flight.insert(key.clone());
                effects.spawn(async move {
                    let provisioned = executor.provision(command, gate).await.map(|outcome| {
                        let refused = match outcome {
                            Provisioned::Stored => String::new(),
                            Provisioned::Refused(reason) => reason.into(),
                        };
                        agent_to_server::Msg::IngressProvisioned(pb::IngressProvisioned {
                            identity: id,
                            refused,
                        })
                    });
                    (key, provisioned)
                });
            }
            // ADR 0018 §3: settle before the next inventory tick.
            Some(server_to_agent::Msg::ProfilesPublished(verdict))
                if fence.is_some() && updates.is_some() =>
            {
                let settled = updates
                    .as_ref()
                    .is_some_and(|u| u.profiles.settle(&verdict.request_id, verdict.accepted));
                if let Some(reply) = publishing.remove(&verdict.request_id) {
                    let _ = reply.send(if verdict.accepted {
                        PublishOutcome::Accepted
                    } else {
                        PublishOutcome::Rejected(verdict.reason)
                    });
                }
                // The server accepted a document this host no longer holds
                // (its caller gave up at the bound): end the session, so the
                // reconnect publishes the host's accepted set authoritatively.
                if verdict.accepted && !settled {
                    return Err(SessionEnd::fixed(
                        "a profile publication was accepted after the host gave up on it",
                    ));
                }
            }
            // ADR 0018 §4: relayed to the waiting handler; `draining` is progress.
            Some(server_to_agent::Msg::ProfileRetirement(answer))
                if fence.is_some() && updates.is_some() =>
            {
                let terminal = answer.outcome != "draining";
                let id = answer.request_id.clone();
                if let Some(reply) = retiring.get(&id) {
                    let _ = reply.try_send(answer);
                }
                if terminal {
                    retiring.remove(&id);
                }
            }
            // Legacy raw argv and commands before reconciliation remain denied.
            _ => return Err(SessionEnd::fixed("controller sent an unexpected message")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T06 T34: ADR 0017. A controller older than this host refuses the
    // session; the host logs "upgrade the server first" with the versions
    // and keeps reconnecting. Any other refusal stays a fixed phrase.
    #[test]
    fn a_newer_host_refusal_says_upgrade_the_server_first() {
        let refusal = tonic::Status::failed_precondition(format!(
            "{}: host 0.3.0 is newer than server 0.2.1; upgrade the server first",
            mllm_protocol::version::NEWER_HOST_REFUSAL
        ));
        let logged = refused_session(refusal).to_string();
        assert!(logged.contains("upgrade the server first"), "{logged}");
        assert!(
            logged.contains("0.3.0") && logged.contains("0.2.1"),
            "{logged}"
        );
        for other in [
            tonic::Status::permission_denied("host session authorization failed"),
            tonic::Status::failed_precondition("something else"),
            tonic::Status::failed_precondition(format!(
                "{}: \u{1b}[31mnot printable",
                mllm_protocol::version::NEWER_HOST_REFUSAL
            )),
        ] {
            assert_eq!(
                refused_session(other).to_string(),
                "controller refused the session"
            );
        }
    }

    // T06 (SPEC §4.1, ADR 0016): only the controller's exact revocation
    // refusal ends the reconnect loop. A look-alike message, another status
    // code carrying the same text, or a generic refusal stays retryable.
    #[test]
    fn only_the_exact_revocation_refusal_stops_reconnecting() {
        assert!(matches!(
            refused_session(mllm_protocol::host_revoked_refusal()),
            SessionEnd::Revoked
        ));
        let revoked = mllm_protocol::HOST_REVOKED_REFUSAL;
        for other in [
            tonic::Status::permission_denied("host session authorization failed"),
            tonic::Status::permission_denied(format!("{revoked} ")),
            tonic::Status::permission_denied(format!("{revoked}:spoofed")),
            tonic::Status::permission_denied(revoked.to_uppercase()),
            tonic::Status::unauthenticated(revoked),
            tonic::Status::failed_precondition(revoked),
            tonic::Status::unavailable(revoked),
            tonic::Status::unknown(revoked),
        ] {
            assert!(
                matches!(refused_session(other.clone()), SessionEnd::Ended(_)),
                "{other:?} stopped the reconnect loop"
            );
        }
    }

    // T33 (SPEC §§4.2, 13): a host without an executor publishes its accepted
    // set's inventory. Its measured domains are read again for every session,
    // never sent as the startup snapshot, which is older than the policy's
    // observation TTL once installations have been measured.
    #[test]
    fn a_session_without_an_executor_publishes_a_fresh_measurement() {
        let golden: serde_json::Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let mut host = golden["input"]["host"].clone();
        host["state_dir"] = "/home/operator/.local/state/mllm".into();
        host["identity_dir"] = "/home/operator/.local/state/mllm/identity".into();
        let config = mllm_config::remote_roles::HostConfig::parse(&host.to_string()).unwrap();
        let reading = crate::memory::read_host_memory().unwrap().memory;
        let stale = mllm_domain::resources::MemoryObservation {
            sampled_at_ms: 1_000,
            ..reading
        };
        let startup = pb::ReportInventory {
            domains: crate::device_domains::startup_domains(&config.document, &stale, None),
            ..Default::default()
        };
        assert!(!startup.domains.is_empty());
        let updates = ProfileUpdates::new(crate::profiles::HostProfiles::new(
            crate::profiles::ProfileSet::new(config, startup.clone()),
        ));
        let before = mllm_protocol::now_unix_ms();
        let published = first_inventory(None, Some(&updates), startup);
        assert_eq!(published.domains.len(), 1);
        for domain in &published.domains {
            assert!(
                domain.observed_at_unix_ms >= before - 1_000,
                "{} was published from the startup snapshot",
                domain.domain_id
            );
            assert!(domain.capacity_bytes > 0);
        }
    }
}

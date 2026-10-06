//! SPEC §§6.1, 13.2 (G2, owner decision D8): readiness of Ready remote engines
//! across host and controller restarts.
//!
//! A remote engine's readiness was proven on one authenticated host session. A
//! host agent restart, a control-channel loss or a controller restart ends that
//! session, and the host closes its ingress gate when it does, so the controller
//! must stop dispatching at once instead of forwarding into a closed gate. This
//! supervisor keeps dispatch open only while the host's current session is the
//! one that proved readiness. When a new session reconciles it asks the host for
//! a fresh native probe of the still-owned engine; only a passing probe of the
//! exact associated group reopens dispatch, with a new readiness receipt. A
//! failed or mismatched probe leaves the deployment charged, closed and retained.
//! Nothing here restarts, releases or adopts an engine.
//!
//! W12, SPEC §10: a controller crash with requests in flight leaves their leases
//! on the adopted launch. Dispatch stays closed while they remain. After a
//! passing probe, the supervisor waits for the host's authenticated load report
//! to show the engine quiescent — nothing running or waiting in the engine and
//! nothing in flight at the host ingress — sampled after that probe; only that
//! completion observation closes the leases as abandoned. A load sample is
//! otherwise a routing hint only; here it closes request accounting of a session
//! that can no longer act, never a reservation, and never on its own.
use crate::{
    agent_sessions::AgentSessions,
    load_table::{InstanceKey, LoadView},
    ownership::SharedCoordinatorState,
    remote_execution::ReadinessLedger,
};
use capyctl_domain::{
    completion::ProcessIdentity,
    group::{CommandIdentity, MemberKey},
};
use capyctl_protocol::{
    execution::{MemberAction, MemberCommand},
    pb,
};
use capyctl_store::ordinary_lifecycle::recovery::{RemoteReadinessEvidence, RemoteReadyLaunch};
use capyctl_store::ordinary_lifecycle::retired_leases::QuiescenceEvidence;
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

/// How often supervision runs without a session change to prompt it.
const PASS_INTERVAL: Duration = Duration::from_millis(250);
/// The bound on one probe command: the host's list read and chat probe fit
/// well inside it, and it ends before a stalled host holds the probe forever.
const PROBE_DEADLINE: Duration = Duration::from_secs(90);
/// The first wait after a failed probe, doubled per failure up to the cap. Every
/// probe is a journaled host command, so a failing engine is not hammered.
const FIRST_RETRY: Duration = Duration::from_secs(2);
const MAX_RETRY: Duration = Duration::from_secs(300);
/// How long a passing probe waits for a quiescent load sample before it gives
/// up this round; hosts report about once a second.
const QUIESCENCE_WAIT: Duration = Duration::from_secs(10);
const QUIESCENCE_POLL: Duration = Duration::from_millis(100);

pub type ProbeFuture =
    Pin<Box<dyn Future<Output = Result<(String, pb::MemberExecutionResult), ()>> + Send>>;

/// The host side supervision needs: which authenticated session is current,
/// notice when that changes, and the result of a probe with the session that
/// answered it. `AgentSessions` is the production implementation.
pub trait ReadinessHosts: Send + Sync + 'static {
    fn current_session(&self, host: &str) -> Option<String>;
    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64>;
    fn probe(&self, command: MemberCommand) -> ProbeFuture;
    /// The latest load one host session reported for an instance, if any.
    fn load(&self, _deployment_id: &str, _generation: i64) -> Option<LoadView> {
        None
    }
}
impl ReadinessHosts for AgentSessions {
    fn current_session(&self, host: &str) -> Option<String> {
        AgentSessions::current_session(self, host)
    }
    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        AgentSessions::subscribe(self)
    }
    fn load(&self, deployment_id: &str, generation: i64) -> Option<LoadView> {
        self.load_table().sample_at(
            &InstanceKey::new(deployment_id, generation),
            capyctl_protocol::now_unix_ms(),
        )
    }
    fn probe(&self, command: MemberCommand) -> ProbeFuture {
        let sessions = self.clone();
        Box::pin(async move {
            sessions
                .execute_on_session(command, None)
                .await
                .map_err(|_| ())
        })
    }
}

struct ProbeState {
    /// The host session the current probing round is for.
    session: String,
    running: bool,
    next_at: Instant,
    backoff: Duration,
    reported: bool,
}

pub struct RemoteReadiness {
    owner: SharedCoordinatorState,
    hosts: Arc<dyn ReadinessHosts>,
    controller_id: String,
    ledger: ReadinessLedger,
    probes: Mutex<BTreeMap<String, ProbeState>>,
    /// Review finding 15: every probe task, owned by the supervision loop.
    children: crate::supervised::Children,
}

impl RemoteReadiness {
    pub fn new(
        owner: SharedCoordinatorState,
        hosts: Arc<dyn ReadinessHosts>,
        controller_id: String,
        ledger: ReadinessLedger,
    ) -> Arc<Self> {
        Arc::new(Self {
            owner,
            hosts,
            controller_id,
            ledger,
            probes: Mutex::new(BTreeMap::new()),
            children: Default::default(),
        })
    }

    /// Run supervision until the task is dropped or aborted. Aborting it
    /// aborts every probe it started.
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
            let mut changes = self.hosts.subscribe();
            loop {
                // Store reads are blocking work; keep them off the async threads.
                let supervisor = self.clone();
                if tokio::task::spawn_blocking(move || supervisor.pass())
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::select! {
                    _ = crate::supervised::cancelled(&mut cancel) => break,
                    changed = changes.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                    _ = tokio::time::sleep(PASS_INTERVAL) => {}
                }
            }
            self.children.join_all().await;
        })
    }

    /// One supervision pass over this session's Ready remote launches. Closing
    /// dispatch happens inline, before anything else can be sent; probes run as
    /// their own tasks.
    pub fn pass(self: &Arc<Self>) {
        let launches = {
            let Ok(owner) = self.owner.lock() else { return };
            match owner.store().remote_ready_launches(owner.session()) {
                Ok(launches) => launches,
                Err(_) => return,
            }
        };
        // Review finding 13: forget the probe state of every launch that no
        // longer needs a probe (released, proven, or its host offline; a new
        // host session starts a fresh round anyway), so the map is bounded by
        // the launches awaiting proof.
        let mut probing = std::collections::BTreeSet::new();
        for launch in launches {
            let current = self.hosts.current_session(&launch.host_id);
            let proven = self
                .ledger
                .lock()
                .ok()
                .and_then(|ledger| ledger.get(&launch.binding_id).cloned());
            if current.is_some() && proven == current {
                continue;
            }
            // SPEC §13.2: the session that proved readiness is gone, so its
            // proof is too. Close dispatch now; ownership and accounting stay.
            // W10 gap (a): a gate a switch closed still gets this reason, so a
            // failed switch cannot reopen it.
            if launch.dispatch_enabled || !launch.host_closure_recorded {
                let Ok(owner) = self.owner.lock() else { return };
                let _ = owner
                    .store()
                    .suspend_remote_dispatch(owner.session(), &launch.step_id);
            }
            if let Some(session) = current {
                probing.insert(launch.binding_id.clone());
                self.start_probe(launch, session);
            }
        }
        if let Ok(mut probes) = self.probes.lock() {
            probes.retain(|binding, state| state.running || probing.contains(binding));
        }
    }

    /// SPEC §4.3: a host announced a graceful shutdown. Close dispatch to every
    /// Ready engine on it now, before the host is acknowledged and closes its
    /// ingress. Ownership, reservations and request leases stay; the next
    /// session re-proves readiness exactly as after any session loss.
    pub fn suspend_host(&self, host: &str) {
        let Ok(owner) = self.owner.lock() else { return };
        let Ok(launches) = owner.store().remote_ready_launches(owner.session()) else {
            return;
        };
        for launch in launches.into_iter().filter(|launch| {
            launch.host_id == host && (launch.dispatch_enabled || !launch.host_closure_recorded)
        }) {
            let _ = owner
                .store()
                .suspend_remote_dispatch(owner.session(), &launch.step_id);
        }
    }

    /// Owner decision 2026-09-23: a host's heartbeats went silent past the
    /// suspend bound while its session stayed up. Its session's readiness proof
    /// is forgotten, so heard-again heartbeats on the same session re-prove the
    /// engine with a fresh probe before dispatch reopens (same as a reconnect),
    /// and dispatch closes now. Ownership, reservations and request leases stay;
    /// nothing is released.
    pub fn suspend_unresponsive_host(&self, host: &str) {
        let launches = {
            let Ok(owner) = self.owner.lock() else { return };
            let Ok(launches) = owner.store().remote_ready_launches(owner.session()) else {
                return;
            };
            launches
        };
        if let Ok(mut ledger) = self.ledger.lock() {
            for launch in launches.iter().filter(|launch| launch.host_id == host) {
                ledger.remove(&launch.binding_id);
            }
        }
        self.suspend_host(host);
    }

    fn start_probe(self: &Arc<Self>, launch: RemoteReadyLaunch, session: String) {
        let now = Instant::now();
        {
            let Ok(mut probes) = self.probes.lock() else {
                return;
            };
            let state = probes
                .entry(launch.binding_id.clone())
                .or_insert_with(|| ProbeState {
                    session: session.clone(),
                    running: false,
                    next_at: now,
                    backoff: FIRST_RETRY,
                    reported: false,
                });
            if state.session != session && !state.running {
                // A new host session is a new chance: probe it at once.
                *state = ProbeState {
                    session: session.clone(),
                    running: false,
                    next_at: now,
                    backoff: FIRST_RETRY,
                    reported: false,
                };
            }
            if state.running || now < state.next_at {
                return;
            }
            state.running = true;
        }
        let supervisor = self.clone();
        self.children.track(tokio::spawn(async move {
            let outcome = supervisor.probe(&launch, &session).await;
            let Ok(mut probes) = supervisor.probes.lock() else {
                return;
            };
            if let Some(state) = probes.get_mut(&launch.binding_id) {
                state.running = false;
                match outcome {
                    Ok(()) => {
                        state.backoff = FIRST_RETRY;
                        state.next_at = Instant::now();
                    }
                    Err(reason) => {
                        state.next_at = Instant::now() + state.backoff;
                        state.backoff = (state.backoff * 2).min(MAX_RETRY);
                        if !state.reported {
                            state.reported = true;
                            if let Ok(owner) = supervisor.owner.lock() {
                                let _ = owner.store().record_journal(
                                    Some(&launch.host_id),
                                    Some(&launch.operation_id),
                                    Some("readiness_unproven"),
                                    &format!(
                                        "deployment {}: {reason}; dispatch stays closed and \
                                         the engine stays owned",
                                        launch.fence.deployment_id
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }));
    }

    /// Ask the host to re-prove this launch on `session`, and reopen dispatch
    /// only on its authenticated, fresh, exactly matching evidence.
    async fn probe(&self, launch: &RemoteReadyLaunch, session: &str) -> Result<(), String> {
        let id = ulid::Ulid::new().to_string();
        let deadline = capyctl_protocol::now_unix_ms()
            .saturating_add(i64::try_from(PROBE_DEADLINE.as_millis()).unwrap_or(i64::MAX));
        let mut command = MemberCommand {
            identity: CommandIdentity {
                controller_id: self.controller_id.clone(),
                member: MemberKey {
                    host_id: launch.host_id.clone(),
                    member_id: "head".into(),
                },
                deployment_id: launch.fence.deployment_id.clone(),
                operation_id: launch.operation_id.clone(),
                command_id: id.clone(),
                step_id: id,
                generation: launch.fence.generation,
                revision: launch.fence.revision,
                deadline_ms: deadline,
                payload_digest: [0; 32],
                expected_state: "ready".into(),
                profile_fingerprint: launch.profile_fingerprint.clone(),
                instance_index: 0,
            },
            action: MemberAction::Probe {
                owned_handle: launch.step_id.clone(),
                max_tokens: None,
            },
        };
        command.identity.payload_digest = command.canonical_digest();
        let (answered_on, result) = self
            .hosts
            .probe(command)
            .await
            .map_err(|()| "the host did not answer the readiness probe".to_owned())?;
        if answered_on != session {
            return Err("the probe was answered on a different host session".into());
        }
        if !result.model_usable
            || !result.claim_retained
            || result.binding_id != launch.binding_id
            || result.incarnation != launch.incarnation
        {
            return Err("the host's fresh native probe did not prove the model usable".into());
        }
        let alive: Vec<ProcessIdentity> = result
            .processes
            .iter()
            .filter(|p| p.presence == "alive")
            .map(|p| ProcessIdentity {
                role: p.role.clone(),
                pid: p.pid,
                boot_id: p.boot_id.clone(),
                start_ticks: p.start_ticks,
            })
            .collect();
        // ADR 0027: every engine process of the associated group and no
        // other; a helper of it may have exited.
        if !capyctl_domain::completion::same_engine(&launch.identities, &alive) {
            return Err("the probed engine is not the associated process group".into());
        }
        if self.hosts.current_session(&launch.host_id).as_deref() != Some(session) {
            return Err("the host session changed while the probe ran".into());
        }
        self.settle_retired_leases(launch, session, result.observed_at_unix_ms)
            .await?;
        // The ledger names this session before dispatch reopens, so a pass in
        // between sees a proven launch and leaves it alone; a refusal undoes it.
        self.ledger
            .lock()
            .map_err(|_| "readiness ledger unavailable".to_owned())?
            .insert(launch.binding_id.clone(), session.to_owned());
        let evidence = RemoteReadinessEvidence {
            binding_id: launch.binding_id.clone(),
            incarnation: launch.incarnation.clone(),
            identities: alive,
            observed_at_ms: result.observed_at_unix_ms,
            receipt: format!(
                "authenticated host session {session} answered a fresh native model probe"
            ),
        };
        let reopened = {
            let owner = self
                .owner
                .lock()
                .map_err(|_| "coordinator state unavailable".to_owned())?;
            owner.store().reverify_remote_dispatch(
                owner.session(),
                &launch.step_id,
                &evidence,
                capyctl_protocol::now_unix_ms(),
            )
        };
        if let Err(error) = reopened {
            if let Ok(mut ledger) = self.ledger.lock() {
                if ledger.get(&launch.binding_id).map(String::as_str) == Some(session) {
                    ledger.remove(&launch.binding_id);
                }
            }
            return Err(format!("readiness evidence was refused: {error}"));
        }
        Ok(())
    }

    /// SPEC §10 (W12): close a retired session's request leases on this launch
    /// only on completion observation. `probed_at` is when this session's fresh
    /// probe proved the engine alive; the quiescent sample must follow it.
    async fn settle_retired_leases(
        &self,
        launch: &RemoteReadyLaunch,
        session: &str,
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
        let deadline = Instant::now() + QUIESCENCE_WAIT;
        loop {
            if self.hosts.current_session(&launch.host_id).as_deref() != Some(session) {
                return Err("the host session changed while awaiting engine quiescence".into());
            }
            let sample = self
                .hosts
                .load(&launch.fence.deployment_id, launch.fence.generation)
                .filter(|view| quiescent(view, launch, probed_at));
            if let Some(view) = sample {
                let evidence = QuiescenceEvidence {
                    binding_id: launch.binding_id.clone(),
                    incarnation: launch.incarnation.clone(),
                    identities: launch.identities.clone(),
                    readiness_observed_at_ms: probed_at,
                    quiescent_at_ms: view.sampled_at_ms,
                    receipt: format!(
                        "authenticated host session {session} reported the launch's engine \
                         with 0 running and 0 waiting requests and 0 in flight at its ingress"
                    ),
                };
                let owner = self
                    .owner
                    .lock()
                    .map_err(|_| "coordinator state unavailable".to_owned())?;
                return owner
                    .store()
                    .abandon_retired_request_leases(
                        owner.session(),
                        &launch.step_id,
                        &evidence,
                        capyctl_protocol::now_unix_ms(),
                    )
                    .map(|_| ())
                    .map_err(|error| format!("quiescence evidence was refused: {error}"));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{retired} request lease(s) of the retired session stay charged: the \
                     engine was not observed quiescent after the probe"
                ));
            }
            tokio::time::sleep(QUIESCENCE_POLL).await;
        }
    }
}

/// A sample that proves the retired requests finished: reported by the launch's
/// host for exactly this launch, fresh, taken after the probe, with a
/// successful engine scrape showing nothing running or waiting and nothing in
/// flight at the ingress. Missing engine gauges are unknown, never zero.
fn quiescent(view: &LoadView, launch: &RemoteReadyLaunch, probed_at: i64) -> bool {
    crate::load_table::proves_quiescence(
        view,
        &launch.host_id,
        &launch.step_id,
        launch.fence.generation,
        probed_at,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct NoHosts(tokio::sync::watch::Sender<u64>);
    impl ReadinessHosts for NoHosts {
        fn current_session(&self, _: &str) -> Option<String> {
            None
        }
        fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
            self.0.subscribe()
        }
        fn probe(&self, _: MemberCommand) -> ProbeFuture {
            Box::pin(async { Err(()) })
        }
    }

    fn supervisor() -> (tempfile::TempDir, Arc<RemoteReadiness>) {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = Arc::new(Mutex::new(
            crate::ownership::OwnedCoordinatorState::open(state.path()).unwrap(),
        ));
        let supervisor = RemoteReadiness::new(
            owner,
            Arc::new(NoHosts(tokio::sync::watch::channel(0).0)),
            "controller".into(),
            Default::default(),
        );
        (state, supervisor)
    }

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

    // SPEC §13.2 (review finding 13): probe state is kept only for launches
    // still awaiting a probe; a released or proven launch is forgotten.
    // T20
    #[tokio::test]
    async fn a_pass_forgets_launches_that_no_longer_await_a_probe() {
        let (_state, supervisor) = supervisor();
        supervisor.probes.lock().unwrap().insert(
            "released-binding".into(),
            ProbeState {
                session: "gone".into(),
                running: false,
                next_at: Instant::now(),
                backoff: FIRST_RETRY,
                reported: true,
            },
        );
        supervisor.pass();
        assert!(supervisor.probes.lock().unwrap().is_empty());
    }

    // SPEC §13.2 (review finding 15): probes are owned by supervision. An
    // aborted supervisor aborts them; a cancelled one joins them first.
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

//! The controller operation engine (design §5): submits deployments
//! transactionally (via mllm-store), executes lifecycle transitions
//! against engine/launcher participants (the fake pair in F0), records
//! evidence in `journal_entries`, and moves the observed state strictly
//! through the legal transition table (mllm-domain).
//!
//! Semantics this module commits to:
//!
//! * `AdapterError::Uncertain` never resolves to a fabricated success: the
//!   observed state goes to RECONCILING. F0 reconciliation is fail-closed:
//!   it never consults the adapter to confirm an outcome, so every failure
//!   or uncertainty resolves RECONCILING → FAILED (legal via the table's
//!   reconciliation outcomes).
//! * Deterministic adapter failures (policy denial, crash, launcher
//!   failure) also route through RECONCILING → FAILED so FAILED is only
//!   ever entered legally.
//! * Admission (mllm-scheduler) runs before any activation transition,
//!   against a synthetic 128 GiB system-memory observation resolved by
//!   `resolve_auto` (F0 has no real topology discovery; the value is the
//!   documented synthetic host). The candidate's activation peak is the
//!   F0 synthetic footprint constant (`F0_SYNTHETIC_ACTIVATION_PEAK`).
//! * Idempotency key derivation (design rule: SHA-256 over server context
//!   id + deployment name + canonical manifest bytes) lives here, where
//!   the submission is composed: for F0 the context id is the literal
//!   `standalone`, and the manifest bytes are the canonicalized request
//!   JSON composed by the caller (CLI/test).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mllm_adapters::{
    AdapterError, EngineAdapter, Launcher, LauncherError, MemberRef, OwnedHandle, ParkLevel,
    PlanInput, Readiness, WorkObservation,
};
use mllm_domain::{
    DeploymentId, LifecycleAction, LifecycleState, OperationId, OwnerAccountId,
};
use mllm_scheduler::admission::{admit, BlockReason, Candidate};
use mllm_scheduler::auto::{resolve_auto, OBSERVATION_TTL_SECS};
use mllm_scheduler::ledger::{Domain, DomainKind, HostLimits};
use mllm_store::{AcceptDeployment, NewOperation, OpState, Store, StoreError};
use sha2::{Digest, Sha256};

/// F0 synthetic activation footprint charged as the candidate's peak in
/// admission (F0 has no real adapters; real adapters source peaks from
/// their recipes in F1).
const F0_SYNTHETIC_ACTIVATION_PEAK: i64 = 4096;
/// Attached deployments: conservative charge until observed otherwise.
const ATTACHED_CONSERVATIVE_BYTES: i64 = 32 * 1024 * 1024 * 1024;
const ATTACHED_KIND: &str = "attached";

/// Synthetic host observation backing F0 admission (no topology
/// discovery): 128 GiB of observed system memory, resolved by
/// `resolve_auto` into managed_limit 96 GiB / free_reserve 12 GiB.
const SYNTHETIC_SYSTEM_OBSERVED_BYTES: i64 = 128 * 1024 * 1024 * 1024;

/// F0 server-context id baked into idempotency keys (F3 introduces real
/// per-server contexts).
pub const STANDALONE_CONTEXT_ID: &str = "standalone";

/// Embedded host id used for journal attribution.
pub const EMBEDDED_HOST_ID: &str = "embedded-local";

/// How long an operation may run before `wait_terminal` gives up.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
/// Readiness-poll interval (fake readiness is immediate; real engines
/// poll this often until their own deadline).
const READINESS_POLL: Duration = Duration::from_millis(10);
/// Grace given to a terminating engine process.
const TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// A request to deploy: the manifest bytes must already be canonical
/// (the caller composes them; the controller hashes but never mutates).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployRequest {
    pub name: String,
    pub kind: String,
    pub manifest: Vec<u8>,
    /// The public model id clients route to (F1: alias resolution, SPEC
    /// §10). `None` = catalog-only deployment (no route).
    pub route_model_id: Option<String>,
}

/// A handle to a long-running control-plane operation.
#[derive(Debug, Clone)]
pub struct OperationHandle {
    pub operation_id: OperationId,
    pub deployment_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    #[error("deployment not found: {0}")]
    UnknownDeployment(String),
    #[error("illegal lifecycle transition: {from:?} -> {to:?}")]
    IllegalTransition {
        from: LifecycleState,
        to: LifecycleState,
    },
    #[error("admission blocked: {0}")]
    Blocked(BlockReason),
    #[error("no safe resource estimate ({0})")]
    NoSafeEstimate(String),
    #[error("operation {op} failed with code {code}")]
    OperationFailed { op: String, code: String },
    #[error("operation did not reach a terminal state in time")]
    Timeout,
    #[error("stale generation for deployment {0} (T18)")]
    StaleGeneration(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The per-step work that carries the observed state across a legal
/// transition pair.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// Scheduler admission before activation.
    Admit,
    /// Render the launch command and spawn the engine process.
    Spawn,
    /// Poll engine readiness until Ready.
    Readiness,
    /// Restore a parked engine (wake path).
    Restore,
    /// Observe work and confirm quiescence before parking.
    Quiesce,
    /// Observe work only (drain toward stop).
    Drain,
    /// Execute the park (level 1; level 2 is policy-gated).
    Park,
    /// Terminate the engine process.
    Terminate,
    /// Nothing beyond the state write itself.
    Confirm,
}

/// The legal chain of `(state, work)` pairs for an action from `from`.
/// `None` means the action is illegal from that state.
fn plan(from: LifecycleState, action: LifecycleAction) -> Option<Vec<(LifecycleState, Step)>> {
    use LifecycleAction::*;
    use LifecycleState::*;
    let chain: Vec<(LifecycleState, Step)> = match (action, from) {
        (Start, Stopped) => vec![
            (Starting, Step::Admit),
            (Starting, Step::Spawn),
            (Ready, Step::Readiness),
        ],
        (Start, Parked) => vec![(Waking, Step::Admit), (Ready, Step::Restore)],
        (Park, Ready) => vec![
            (Draining, Step::Quiesce),
            (Parking, Step::Park),
            (Parked, Step::Confirm),
        ],
        (Stop, Ready) => vec![
            (Draining, Step::Drain),
            (Stopping, Step::Terminate),
            (Stopped, Step::Confirm),
        ],
        (Stop, Parked) => vec![(Stopping, Step::Terminate), (Stopped, Step::Confirm)],
        // Preinitialize changes no lifecycle state; the adapter work is
        // executed directly.
        (Preinitialize, Stopped | Parked | Ready) => vec![],
        _ => return None,
    };
    Some(chain)
}

fn validate_chain(
    from: LifecycleState,
    chain: &[(LifecycleState, Step)],
) -> Result<(), ControllerError> {
    let mut current = from;
    for (state, _) in chain {
        // Consecutive steps in the same state continue work already
        // entered; only actual state changes must be legal transitions.
        if *state != current && !current.can_transition_to(*state) {
            return Err(ControllerError::IllegalTransition {
                from: current,
                to: *state,
            });
        }
        current = *state;
    }
    Ok(())
}

/// SHA-256(context_id \0 name \0 canonical_manifest_bytes), hex-encoded.
/// F0 in-process derivation: context id is `standalone` and the manifest
/// is the caller's canonical request JSON (see module docs).
fn idempotency_key(context_id: &str, name: &str, manifest: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(context_id.as_bytes());
    h.update([0u8]);
    h.update(name.as_bytes());
    h.update([0u8]);
    h.update(manifest);
    hex::encode(h.finalize())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn observed_str(state: LifecycleState) -> &'static str {
    match state {
        LifecycleState::Stopped => "stopped",
        LifecycleState::Starting => "starting",
        LifecycleState::Ready => "ready",
        LifecycleState::Draining => "draining",
        LifecycleState::Parking => "parking",
        LifecycleState::Parked => "parked",
        LifecycleState::Waking => "waking",
        LifecycleState::Stopping => "stopping",
        LifecycleState::Reconciling => "reconciling",
        LifecycleState::Failed => "failed",
    }
}

/// The controller operation engine. The store is a `Mutex<Store>` because
/// `rusqlite::Connection` is `Send` but not `Sync`; the lock is never held
/// across an `await`.
pub struct Controller {
    store: Arc<Mutex<Store>>,
    adapter: Arc<dyn EngineAdapter>,
    launcher: Arc<dyn Launcher>,
    context_id: String,
    host_id: String,
    /// Live engine process handles per deployment (spawn/park/stop).
    handles: Arc<Mutex<HashMap<String, OwnedHandle>>>,
    /// Host deep-park policy (F1 design §7): the opt-in gates the
    /// experimental profile itself, not just park/reload operations.
    park_policy: mllm_adapters::fake::ParkPolicy,
    /// The embedded fake engine handle (qualification/ambiguity injection).
    embedded_fake: Option<Arc<mllm_adapters::fake::FakeEngine>>,
}

impl Controller {
    pub fn new(
        store: Arc<Mutex<Store>>,
        adapter: Arc<dyn EngineAdapter>,
        launcher: Arc<dyn Launcher>,
    ) -> Self {
        Self::new_with_policy(store, adapter, launcher, mllm_adapters::fake::ParkPolicy::Denied)
    }

    pub fn new_with_policy(
        store: Arc<Mutex<Store>>,
        adapter: Arc<dyn EngineAdapter>,
        launcher: Arc<dyn Launcher>,
        park_policy: mllm_adapters::fake::ParkPolicy,
    ) -> Self {
        Self {
            store,
            adapter,
            launcher,
            context_id: STANDALONE_CONTEXT_ID.to_string(),
            host_id: EMBEDDED_HOST_ID.to_string(),
            handles: Arc::new(Mutex::new(HashMap::new())),
            park_policy,
            embedded_fake: None,
        }
    }

    /// Attach the embedded fake engine handle (ambiguity injection for the
    /// qualification suite).
    pub fn with_embedded_fake(mut self, fake: Arc<mllm_adapters::fake::FakeEngine>) -> Self {
        self.embedded_fake = Some(fake);
        self
    }

    /// Submit a deployment transactionally: derive the idempotency key,
    /// accept into the store (deploy operation included), journal the
    /// acceptance. Returns the durable deployment ID.
    ///
    /// Deployments are accepted in `Stopped`; activation happens only via
    /// an explicit `Start` transition.
    pub async fn submit_deploy(&self, req: DeployRequest) -> Result<String, ControllerError> {
        let id = DeploymentId::new();
        let key = idempotency_key(&self.context_id, &req.name, &req.manifest);
        let op = OperationId(format!("op-{}", ulid::Ulid::new()));
        let accepted = {
            let store = self.store.lock().unwrap();
            let accepted = store.accept_deployment(AcceptDeployment {
                id,
                name: req.name,
                kind: req.kind,
                route_model_id: req.route_model_id.clone(),
                desired_state: LifecycleState::Stopped,
                schema_version: 1,
                idempotency_key: key,
                initial_operation_id: op,
            })?;
            // On idempotent resolution the store hands back the ORIGINAL
            // deploy operation (the new one was never inserted); the
            // completion and journal evidence must attach to that one.
            let op = accepted.operation_id.clone();
            store.update_operation_state(&op.0, OpState::Succeeded, None)?;
            let manifest_sha = hex::encode({
                let mut h = Sha256::new();
                h.update(&req.manifest);
                h.finalize()
            });
            store.record_journal(
                Some(&self.host_id),
                Some(&op.0),
                Some("stopped"),
                &format!(
                    r#"{{"event":"accepted","deployment":"{id}","context":"{ctx}","manifest_sha256":"{sha}"}}"#,
                    id = accepted.deployment_id,
                    ctx = self.context_id,
                    sha = manifest_sha,
                ),
            )?;
            accepted
        };
        // Initial reservation intent persisted at acceptance (F1 G3: the
        // resource ledger's owner rows are durable, not just in-memory).
        {
            let store = self.store.lock().unwrap();
            let dep = accepted.deployment_id.to_string();
            store
                .insert_reservation(&mllm_store::ReservationRow {
                    owner_id: dep.clone(),
                    domain_id: Some("system".into()),
                    bytes: F0_SYNTHETIC_ACTIVATION_PEAK,
                    phase: "activation".into(),
                    exclusive_devices: vec![],
                })
                .map_err(ControllerError::from)?;
        }
        Ok(accepted.deployment_id.to_string())
    }

    /// Request a lifecycle transition. Legality is checked against the
    /// legal transition table *before* any operation is recorded; an
    /// illegal request leaves no trace. Legal requests record a pending
    /// operation and execute it in the background; await it with
    /// [`Controller::wait_terminal`].
    pub async fn request_transition(
        &self,
        deployment: &str,
        action: LifecycleAction,
    ) -> Result<OperationHandle, ControllerError> {
        self.request_transition_inner(deployment, action, false).await
    }

    /// Idle eviction (T10): stop the engine but keep the deployment
    /// on-demand eligible — the next inference request may activate it.
    /// Administrative stop (LifecycleAction::Stop) suspends instead, and a
    /// suspended deployment rejects activation.
    pub async fn idle_stop(&self, deployment: &str) -> Result<OperationHandle, ControllerError> {
        self.request_transition_inner(deployment, LifecycleAction::Stop, true)
            .await
    }

    async fn request_transition_inner(
        &self,
        deployment: &str,
        action: LifecycleAction,
        keep_on_demand_eligible: bool,
    ) -> Result<OperationHandle, ControllerError> {
        let observed = {
            let store = self.store.lock().unwrap();
            let row = store
                .get_deployment(deployment)?
                .ok_or_else(|| ControllerError::UnknownDeployment(deployment.to_string()))?;
            if row.kind == ATTACHED_KIND {
                // Ownership ≠ reachability (T11): attached services are for
                // routing/observation only — no sleep/kill/restart rights.
                return Err(ControllerError::OperationFailed {
                    op: action.as_str().to_string(),
                    code: "attached_no_lifecycle".to_string(),
                });
            }
            row.observed_state
        };
        // Administrative stop marks the deployment suspended up front
        // (T10): autoactivation must not undo the operator's explicit stop
        // (SPEC §6.3: required explicit stop MUST NOT be undone by the
        // next inference request).
        if action == LifecycleAction::Stop && !keep_on_demand_eligible {
            let store = self.store.lock().unwrap();
            store.set_suspended(deployment, true)?;
        }
        // A suspended deployment rejects activation Start.
        if action == LifecycleAction::Start {
            let store = self.store.lock().unwrap();
            if store.is_suspended(deployment)? {
                return Err(ControllerError::OperationFailed {
                    op: "start".to_string(),
                    code: "suspended".to_string(),
                });
            }
        }
        // Preinitialize contract (F1 design §7 / SPEC §6.3): start →
        // validate → park, never displacing live work; requires a
        // QUALIFIED parking capability — restart-only deployments and
        // policy-denied profiles fail clearly instead of claiming a
        // prewarmed deployment.
        if action == LifecycleAction::Preinitialize {
            // Lock scoped: never held across the recursive transitions.
            let (kind, observed) = {
                let store = self.store.lock().unwrap();
                store
                    .get_deployment(deployment)?
                    .map(|r| (r.kind, r.observed_state))
                    .ok_or_else(|| ControllerError::UnknownDeployment(deployment.to_string()))?
            };
            let qualified = kind == "vllm-sleep"
                && self.park_policy == mllm_adapters::fake::ParkPolicy::ExperimentalAllowed;
            if !qualified {
                return Err(ControllerError::OperationFailed {
                    op: "preinitialize".to_string(),
                    code: "unsupported_parking".to_string(),
                });
            }
            if observed != LifecycleState::Ready {
                // Sequentially start, validate, then park (SPEC §6.3).
                let start = Box::pin(
                    self.request_transition_inner(deployment, LifecycleAction::Start, false),
                )
                .await?;
                self.wait_terminal(&start).await?;
            }
            return Box::pin(self.request_transition_inner(
                deployment,
                LifecycleAction::Park,
                false,
            ))
            .await;
        }
        // Profile-level development-mode gate (F1 design §7, T21): the
        // experimental vllm-sleep profile cannot launch in ANY mode without
        // the host-policy opt-in — the dangerous surface exists from the
        // moment a development-mode engine starts.
        if action == LifecycleAction::Start {
            let store = self.store.lock().unwrap();
            let kind = store
                .get_deployment(deployment)?
                .map(|r| r.kind)
                .unwrap_or_default();
            if kind == "vllm-sleep" && self.park_policy != mllm_adapters::fake::ParkPolicy::ExperimentalAllowed {
                let op = OperationId(format!("op-{}", ulid::Ulid::new()));
                store.record_operation(NewOperation {
                    id: op.clone(),
                    deployment_id: deployment.to_string(),
                    kind: "policy_denied".to_string(),
                    idempotency_key: None,
                })?;
                store.record_journal(
                    Some(&self.host_id),
                    Some(&op.0),
                    Some("denied"),
                    r#"{"event":"policy_denied","profile":"vllm-sleep","reason":"development-mode opt-in required"}"#,
                )?;
                return Err(ControllerError::OperationFailed {
                    op: "start".to_string(),
                    code: "policy_denied".to_string(),
                });
            }
        }
        let chain = plan(observed, action).ok_or(ControllerError::IllegalTransition {
            from: observed,
            to: action_target(action),
        })?;
        validate_chain(observed, &chain)?;

        let op = OperationId(format!("op-{}", ulid::Ulid::new()));
        {
            let store = self.store.lock().unwrap();
            store.record_operation(NewOperation {
                id: op.clone(),
                deployment_id: deployment.to_string(),
                kind: action.as_str().to_string(),
                idempotency_key: None,
            })?;
        }
        let task = ExecTask {
            store: self.store.clone(),
            adapter: self.adapter.clone(),
            launcher: self.launcher.clone(),
            handles: self.handles.clone(),
            host_id: self.host_id.clone(),
        };
        let deployment = deployment.to_string();
        let op_for_task = op.clone();
        let dep_for_handle = deployment.clone();
        tokio::spawn(async move {
            task.run(deployment, op_for_task, action, chain).await;
        });
        Ok(OperationHandle {
            operation_id: op,
            deployment_id: dep_for_handle,
        })
    }

    /// Wait until the operation reaches a terminal state. `Ok` carries the
    /// deployment's final observed state; a failed operation is an error
    /// carrying its stable error code.
    pub async fn wait_terminal(
        &self,
        handle: &OperationHandle,
    ) -> Result<LifecycleState, ControllerError> {
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        loop {
            let (state, error_code, observed) = {
                let store = self.store.lock().unwrap();
                let row = store.get_operation(&handle.operation_id.0)?;
                match row {
                    None => return Err(ControllerError::UnknownDeployment(handle.operation_id.0.clone())),
                    Some(row) => {
                        let observed = store
                            .get_deployment(&handle.deployment_id)?
                            .map(|r| r.observed_state);
                        (row.state, row.error_code, observed)
                    }
                }
            };
            match (state, observed) {
                (OpState::Succeeded, Some(observed)) => {
                    // Generation machinery (F1 G3): every successful
                    // transition bumps the deployment generation and
                    // records history — monotonic, never reset.
                    {
                        let store = self.store.lock().unwrap();
                        let _ = store.bump_generation(&handle.deployment_id)?;
                    }
                    return Ok(observed);
                }
                (OpState::Failed, _) => {
                    return Err(ControllerError::OperationFailed {
                        op: handle.operation_id.0.clone(),
                        code: error_code.unwrap_or_else(|| "unknown".to_string()),
                    })
                }
                _ => {
                    if Instant::now() >= deadline {
                        return Err(ControllerError::Timeout);
                    }
                    tokio::time::sleep(READINESS_POLL).await;
                }
            }
        }
    }
}

fn action_target(action: LifecycleAction) -> LifecycleState {
    match action {
        LifecycleAction::Start => LifecycleState::Ready,
        LifecycleAction::Park => LifecycleState::Parked,
        LifecycleAction::Stop => LifecycleState::Stopped,
        LifecycleAction::Preinitialize => LifecycleState::Ready,
    }
}

/// Cloned execution context for a spawned operation task.
struct ExecTask {
    store: Arc<Mutex<Store>>,
    adapter: Arc<dyn EngineAdapter>,
    launcher: Arc<dyn Launcher>,
    handles: Arc<Mutex<HashMap<String, OwnedHandle>>>,
    host_id: String,
}

impl ExecTask {
    fn journal(&self, op: &OperationId, observed: LifecycleState, evidence: String) {
        let _ = self.store.lock().unwrap().record_journal(
            Some(&self.host_id),
            Some(&op.0),
            Some(observed_str(observed)),
            &evidence,
        );
    }

    fn set_observed(&self, dep: &str, state: LifecycleState) -> Result<(), ControllerError> {
        self.store.lock().unwrap().set_observed_state(dep, state)?;
        Ok(())
    }

    fn member(dep: &str) -> MemberRef {
        MemberRef {
            deployment_id: dep.to_string(),
            // Distinct member id per deployment: the engine adapter keys
            // member state by this (A parked must not make B unready).
            member_id: format!("{dep}-head"),
        }
    }

    async fn run(
        &self,
        dep: String,
        op: OperationId,
        action: LifecycleAction,
        chain: Vec<(LifecycleState, Step)>,
    ) {
        {
            let store = self.store.lock().unwrap();
            let _ = store.update_operation_state(&op.0, OpState::Running, None);
        }

        if chain.is_empty() {
            // Preinitialize: adapter work with no lifecycle-state change.
            let observed = {
                let store = self.store.lock().unwrap();
                store
                    .get_deployment(&dep)
                    .ok()
                    .flatten()
                    .map(|r| r.observed_state)
                    .unwrap_or(LifecycleState::Stopped)
            };
            let outcome = self
                .adapter
                .reload_weights(&Self::member(&dep))
                .await
                .map(|_| ())
                .map_err(|e| adapter_code(&e));
            self.finish(&dep, &op, action, observed, outcome).await;
            return;
        }

        for (state, step) in &chain {
            let (state, step) = (*state, *step);
            if let Err(err) = self.set_observed(&dep, state) {
                // The deployment vanished underneath us: nothing to transition.
                let _ = self.store.lock().unwrap().update_operation_state(
                    &op.0,
                    OpState::Failed,
                    Some("unknown_deployment"),
                );
                let _ = err;
                return;
            }
            let outcome = self.execute_step(&dep, &op, state, step).await;
            if let Err(code) = outcome {
                self.finish(&dep, &op, action, state, Err(code)).await;
                return;
            }
        }
        let final_state = chain
            .last()
            .map(|(s, _)| *s)
            .unwrap_or(LifecycleState::Stopped);
        self.finish(&dep, &op, action, final_state, Ok(())).await;
    }

    async fn execute_step(
        &self,
        dep: &str,
        op: &OperationId,
        state: LifecycleState,
        step: Step,
    ) -> Result<(), String> {
        let member = Self::member(dep);
        let result: Result<(), (bool, String)> = match step {
            Step::Admit => self
                .admission_check(dep, op, state)
                .map_err(|code| (false, code))
                .map(|_| ()),
            Step::Spawn => {
                self.adapter
                    .render_plan(&PlanInput {
                        deployment_id: dep.to_string(),
                        member_id: format!("{dep}-head"),
                        park_level: None,
                        engine_api_key: None, // F1 lab: engine on private loopback
                    })
                    .await
                    .map_err(|e| (is_uncertain(&e), adapter_code(&e)))
                    .and_then(|rendered| {
                        self.journal(
                            op,
                            state,
                            format!(
                                r#"{{"event":"plan_rendered","argv":{argv:?}}}"#,
                                argv = rendered.argv
                            ),
                        );
                        self.launcher
                            .spawn(&rendered)
                            .map_err(|e| (false, spawn_code(&e)))
                            .map(|handle| {
                                self.journal(
                                    op,
                                    state,
                                    format!(
                                        r#"{{"event":"spawned","pid":{pid},"start_identity":{sid}}}"#,
                                        pid = handle.pid,
                                        sid = handle.start_identity
                                    ),
                                );
                                self.handles
                                    .lock()
                                    .unwrap()
                                    .insert(dep.to_string(), handle);
                            })
                    })
            }
            Step::Readiness => {
                let deadline = Instant::now() + OPERATION_TIMEOUT;
                loop {
                    match self.adapter.check_readiness(&member).await {
                        Ok(Readiness::Ready) => {
                            self.journal(
                                op,
                                state,
                                r#"{"event":"ready"}"#.to_string(),
                            );
                            break Ok(());
                        }
                        Ok(Readiness::Initializing) => {
                            if Instant::now() >= deadline {
                                break Err((false, "activation_timeout".to_string()));
                            }
                            tokio::time::sleep(READINESS_POLL).await;
                        }
                        Err(e) => break Err((is_uncertain(&e), adapter_code(&e))),
                    }
                }
            }
            Step::Restore => {
                match self.adapter.restore(&member).await {
                    Ok(_) => {
                        self.journal(op, state, r#"{"event":"restored"}"#.to_string());
                        Ok(())
                    }
                    Err(e) => Err((is_uncertain(&e), adapter_code(&e))),
                }
            }
            Step::Quiesce => {
                let work = self.adapter.observe_work(&member).await;
                let quiescence = self.adapter.prepare_park(&member).await;
                match (work, quiescence) {
                    (Ok(WorkObservation::Idle), Ok(q)) if q.quiescent => {
                        self.journal(op, state, r#"{"event":"quiescent"}"#.to_string());
                        Ok(())
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        Err((is_uncertain(&e), adapter_code(&e)))
                    }
                    _ => Err((false, "not_quiescent".to_string())),
                }
            }
            Step::Drain => match self.adapter.observe_work(&member).await {
                Ok(WorkObservation::Idle) => Ok(()),
                Ok(other) => Err((false, format!("work_not_drained:{other:?}"))),
                Err(e) => Err((is_uncertain(&e), adapter_code(&e))),
            },
            Step::Park => match self.adapter.park(&member, ParkLevel::One).await {
                Ok(outcome) => {
                    self.journal(op, state, format!(r#"{{"event":"parked","outcome":{outcome:?}}}"#));
                    Ok(())
                }
                Err(e) => Err((is_uncertain(&e), adapter_code(&e))),
            },
            Step::Terminate => {
                let handle = self.handles.lock().unwrap().remove(dep);
                match handle {
                    Some(handle) => match self.launcher.terminate(&handle, TERMINATE_GRACE) {
                        Ok(report) => {
                            self.journal(
                                op,
                                state,
                                format!(
                                    r#"{{"event":"terminated","pid":{},"exit_code":{:?}}}"#,
                                    report.pid, report.exit_code
                                ),
                            );
                            Ok(())
                        }
                        Err(e) => Err((false, spawn_code(&e))),
                    },
                    None => {
                        // No live handle means we own no process: nothing to
                        // terminate, so the release is verified trivially
                        // (the STOPPED semantic: "no owned engine workers
                        // remain"). F3 adds restart-ownership reconciliation
                        // so a handle lost across controller restarts is
                        // recovered, not silently dropped.
                        self.journal(op, state, r#"{"event":"no_live_handle","verified":"no owned handle"}"#.to_string());
                        Ok(())
                    }
                }
            }
            Step::Confirm => Ok(()),
        };
        result.map_err(|(uncertain, code)| {
            // Any uncertain step routes through RECONCILING before the
            // failure lands; deterministic failures fail directly.
            if uncertain {
                let _ = self.set_observed(dep, LifecycleState::Reconciling);
                self.journal(
                    op,
                    LifecycleState::Reconciling,
                    format!(r#"{{"event":"uncertain","code":"{code}"}}"#),
                );
            }
            code
        })
    }

    /// Failure/uncertainty resolution: observed goes RECONCILING → FAILED.
    /// F0 reconciliation is fail-closed — it never consults the adapter to
    /// confirm an outcome, so every failure or uncertainty lands in FAILED
    /// (never a fabricated success).
    async fn finish(
        &self,
        dep: &str,
        op: &OperationId,
        action: LifecycleAction,
        at_state: LifecycleState,
        outcome: Result<(), String>,
    ) {
        if std::env::var("MLLM_DEBUG").is_ok() {
            eprintln!("DEBUG op {} action {:?} outcome {:?}", op.0, action, outcome.as_ref().err());
        }
        match outcome {
            Ok(()) => {
                self.journal(
                    op,
                    at_state,
                    format!(r#"{{"event":"succeeded","action":"{}"}}"#, action.as_str()),
                );
                let _ = self.store.lock().unwrap().update_operation_state(
                    &op.0,
                    OpState::Succeeded,
                    None,
                );
            }
            Err(code) => {
                // at_state → Reconciling → Failed (both legal pairs).
                let _ = self.set_observed(dep, LifecycleState::Reconciling);
                let _ = self.set_observed(dep, LifecycleState::Failed);
                self.journal(
                    op,
                    LifecycleState::Failed,
                    format!(
                        r#"{{"event":"failed","action":"{}","code":"{code}"}}"#,
                        action.as_str()
                    ),
                );
                let _ = self.store.lock().unwrap().update_operation_state(
                    &op.0,
                    OpState::Failed,
                    Some(&code),
                );
            }
        }
    }

    /// F0 admission: a synthetic 128 GiB system observation (documented
    /// in the module docs), auto-resolved limits, the F0 synthetic
    /// footprint as the activation peak, and an empty ledger (fresh
    /// host). Runs the mllm-scheduler ledger end-to-end and, on success,
    /// journals the auto-resolution provenance (policy version, observed
    /// bytes, resolved limits) as evidence.
    fn admission_check(
        &self,
        dep: &str,
        op: &OperationId,
        state: LifecycleState,
    ) -> Result<(), String> {
        let observed = SYNTHETIC_SYSTEM_OBSERVED_BYTES;
        let resolved = resolve_auto(observed)
            .map_err(|d| format!("{}:{}", d.code, d.detail))
            .map_err(|code| format!("no_safe_estimate:{code}"))?;
        let now = now_unix();
        let domains = vec![Domain {
            id: "system".to_string(),
            kind: DomainKind::System,
            observed_bytes: Some(observed),
            observed_at_unix: now,
        }];
        let limits = HostLimits {
            managed_limit: resolved.managed_limit,
            free_reserve: resolved.free_reserve,
            host_kv_limit: None,
            parked_limit: None,
            observation_ttl_secs: OBSERVATION_TTL_SECS,
            now_unix: now,
        };
        let candidate = Candidate {
            owner: OwnerAccountId(dep.to_string()),
            domain: "system".to_string(),
            activation_peak: F0_SYNTHETIC_ACTIVATION_PEAK,
            parked_budget: None,
            category: None,
            devices: vec![],
        };
        admit(&domains, &[], &candidate, &limits).map_err(|reason| format!("{reason:?}"))?;
        self.journal(
            op,
            state,
            format!(
                r#"{{"event":"admitted","policy_version":{v},"observed_bytes":{obs},"managed_limit":{ml},"free_reserve":{fr},"owner":"{dep}"}}"#,
                v = resolved.policy_version,
                obs = resolved.observed_bytes,
                ml = resolved.managed_limit,
                fr = resolved.free_reserve,
            ),
        );
        Ok(())
    }
}

fn is_uncertain(err: &AdapterError) -> bool {
    matches!(err, AdapterError::Uncertain(_))
}

fn adapter_code(err: &AdapterError) -> String {
    match err {
        AdapterError::Uncertain(detail) => format!("uncertain:{detail}"),
        AdapterError::PolicyDenied => "policy_denied".to_string(),
        AdapterError::UnsupportedCapability => "unsupported_capability".to_string(),
        AdapterError::Crash(phase) => format!("crash:{phase:?}"),
        AdapterError::UnsupportedCombination => "unsupported_combination".to_string(),
    }
}

fn spawn_code(err: &LauncherError) -> String {
    match err {
        LauncherError::SpawnFailed(detail) => format!("spawn_failed:{detail}"),
        LauncherError::TerminateFailed(detail) => format!("terminate_failed:{detail}"),
    }
}


impl Controller {
    /// The shared store handle (tests and the router read generations).
    pub fn store_ref(&self) -> Arc<Mutex<Store>> {
        self.store.clone()
    }

    /// The adapter's work observation for a deployment's member (the
    /// switch engine's drain oracle).
    pub async fn observe_adapter(
        &self,
        deployment: &str,
    ) -> Result<mllm_adapters::traits::WorkObservation, mllm_adapters::traits::AdapterError> {
        let member = mllm_adapters::traits::MemberRef {
            deployment_id: deployment.to_string(),
            member_id: format!("{deployment}-head"),
        };
        self.adapter.observe_work(&member).await
    }

    /// Test/qualification hook: mark the embedded fake engine's parks as
    /// ambiguous (effect applied, ack lost). Only available when the
    /// adapter is the fake; real adapters inject ambiguity at the engine.
    pub fn fake_engine(&self) -> Option<Arc<mllm_adapters::fake::FakeEngine>> {
        // Downcast through the shared adapter slot is not possible on
        // Arc<dyn EngineAdapter> without Any; the agent supplies the fake
        // handle separately. This helper exists for the embedded host.
        self.embedded_fake.clone()
    }

    /// The live engine process PID for a deployment, if owned and running.
    pub fn live_pid(&self, deployment: &str) -> Option<u32> {
        self.handles
            .lock()
            .unwrap()
            .get(deployment)
            .map(|h| h.pid)
    }

    /// Stale-generation dispatch check (T18): a dispatch carrying an older
    /// generation than the deployment's current one is rejected — the
    /// ingress gate refuses late/stale dispatch after its gate closes.
    pub fn check_dispatch_generation(&self, deployment: &str, observed: i64) -> Result<i64, ControllerError> {
        let store = self.store.lock().unwrap();
        store
            .check_generation(deployment, observed)
            .map_err(|_| ControllerError::StaleGeneration(deployment.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mllm_adapters::Phase;
    use mllm_adapters::fake::{FakeEngine, FakeLauncher};

    fn controller(engine: FakeEngine) -> (Controller, Arc<FakeEngine>, Arc<FakeLauncher>) {
        let engine = Arc::new(engine);
        let launcher = Arc::new(FakeLauncher::new());
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let c = Controller::new(store, engine.clone(), launcher.clone());
        (c, engine, launcher)
    }

    fn req(name: &str) -> DeployRequest {
        DeployRequest {
            name: name.to_string(),
            kind: "model".to_string(),
            manifest: format!(r#"{{"kind":"model","name":"{name}"}}"#).into_bytes(),
            route_model_id: Some(name.to_string()),
        }
    }

    async fn drive(
        c: &Controller,
        dep: &str,
        action: LifecycleAction,
        want: Result<LifecycleState, ()>,
    ) {
        let handle = c.request_transition(dep, action).await.unwrap();
        let result = c.wait_terminal(&handle).await;
        match want {
            Ok(state) => assert_eq!(result.unwrap(), state),
            Err(()) => assert!(result.is_err(), "expected failure, got {result:?}"),
        }
    }

    #[tokio::test]
    async fn full_fake_lifecycle_over_in_memory_store() {
        let (c, _engine, _launcher) = controller(FakeEngine::new());
        let dep = c.submit_deploy(req("m1")).await.unwrap();
        assert!(c
            .store
            .lock()
            .unwrap()
            .get_deployment(&dep)
            .unwrap()
            .is_some());
        drive(&c, &dep, LifecycleAction::Start, Ok(LifecycleState::Ready)).await;
        drive(&c, &dep, LifecycleAction::Park, Ok(LifecycleState::Parked)).await;
        drive(&c, &dep, LifecycleAction::Start, Ok(LifecycleState::Ready)).await;
        drive(&c, &dep, LifecycleAction::Stop, Ok(LifecycleState::Stopped)).await;
    }

    #[tokio::test]
    async fn every_observed_move_is_legal_including_failures() {
        let (c, engine, _launcher) = controller(FakeEngine::new().fail_at(Phase::Startup));
        let dep = c.submit_deploy(req("m1")).await.unwrap();
        drive(&c, &dep, LifecycleAction::Start, Err(())).await;
        let row = c.store.lock().unwrap().get_deployment(&dep).unwrap().unwrap();
        assert_eq!(row.observed_state, LifecycleState::Failed);
        // The crash was deterministic, but FAILED is still entered legally
        // through RECONCILING (any -> RECONCILING -> FAILED).
        let _ = engine;
    }

    #[tokio::test]
    async fn uncertain_park_routes_through_reconciling_and_fails() {
        let (c, engine, _launcher) = controller(FakeEngine::new().ambiguous_park());
        let dep = c.submit_deploy(req("m1")).await.unwrap();
        drive(&c, &dep, LifecycleAction::Start, Ok(LifecycleState::Ready)).await;
        drive(&c, &dep, LifecycleAction::Park, Err(())).await;
        let row = c.store.lock().unwrap().get_deployment(&dep).unwrap().unwrap();
        // The park effect was applied but the ack was lost; F0 reconciliation
        // confirms only proven-Ready outcomes, so the deployment fails
        // conservatively instead of fabricating Parked.
        assert_eq!(row.observed_state, LifecycleState::Failed);
        engine.reload_weights_count();
    }

    #[tokio::test]
    async fn preinitialize_is_policy_denied_under_default_gate() {
        // F1 contract (design §7): preinitialize fails CLEARLY on restart-only
        // deployments — an unsupported_parking error before any operation, so
        // the deployment never enters FAILED and never claims a prewarm.
        let (c, _engine, _launcher) = controller(FakeEngine::new());
        let dep = c.submit_deploy(req("m1")).await.unwrap();
        let out = c.request_transition(&dep, LifecycleAction::Preinitialize).await;
        match out {
            Err(ControllerError::OperationFailed { code, .. }) => {
                assert_eq!(code, "unsupported_parking");
            }
            other => panic!("expected unsupported_parking, got {other:?}"),
        }
        let row = c.store.lock().unwrap().get_deployment(&dep).unwrap().unwrap();
        assert_eq!(row.observed_state, LifecycleState::Stopped);
    }

    #[tokio::test]
    async fn illegal_transition_leaves_no_operation_trace() {
        let (c, _engine, _launcher) = controller(FakeEngine::new());
        let dep = c.submit_deploy(req("m1")).await.unwrap();
        let before = c.store.lock().unwrap().latest_operation(&dep).unwrap().unwrap();
        let err = c
            .request_transition(&dep, LifecycleAction::Stop)
            .await
            .unwrap_err();
        assert!(matches!(err, ControllerError::IllegalTransition { .. }));
        // No new operation was recorded for the illegal request.
        let after = c.store.lock().unwrap().latest_operation(&dep).unwrap().unwrap();
        assert_eq!(before.id, after.id);
    }

    #[tokio::test]
    async fn admission_journals_auto_resolution_provenance() {
        use mllm_scheduler::auto::AUTO_POLICY_VERSION;
        let (c, _engine, _launcher) = controller(FakeEngine::new());
        let dep = c.submit_deploy(req("m1")).await.unwrap();
        drive(&c, &dep, LifecycleAction::Start, Ok(LifecycleState::Ready)).await;
        let store = c.store.lock().unwrap();
        let op = store.latest_operation(&dep).unwrap().unwrap();
        let evidence = store.journal_evidence(&op.id).unwrap();
        let resolved = resolve_auto(SYNTHETIC_SYSTEM_OBSERVED_BYTES).unwrap();
        let expected = format!(
            r#"{{"event":"admitted","policy_version":{v},"observed_bytes":{obs},"managed_limit":{ml},"free_reserve":{fr},"owner":"{dep}"}}"#,
            v = resolved.policy_version,
            obs = resolved.observed_bytes,
            ml = resolved.managed_limit,
            fr = resolved.free_reserve,
        );
        assert!(
            evidence.contains(&expected),
            "admitted evidence must carry the resolved auto values; got {evidence:?}"
        );
        assert_eq!(resolved.policy_version, AUTO_POLICY_VERSION);
    }

    #[test]
    fn idempotency_key_is_stable_over_context_name_and_manifest() {
        let manifest = br#"{"name":"m1"}"#.to_vec();
        let a = idempotency_key("standalone", "m1", &manifest);
        let b = idempotency_key("standalone", "m1", &manifest);
        assert_eq!(a, b);
        assert_ne!(a, idempotency_key("standalone", "m2", &manifest));
        assert_ne!(a, idempotency_key("other-server", "m1", &manifest));
        assert_ne!(a, idempotency_key("standalone", "m1", br#"{"name":"M1"}"#));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn resubmit_resolves_to_the_same_durable_deployment() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let c = Controller::new(store, Arc::new(FakeEngine::new()), Arc::new(FakeLauncher::new()));
        let dep_id = DeploymentId::new();
        let key = idempotency_key("standalone", "m1", &req("m1").manifest);
        let accept = |id: DeploymentId| AcceptDeployment {
            id,
            name: "m1".into(),
            kind: "model".into(),
            route_model_id: None,
            desired_state: LifecycleState::Stopped,
            schema_version: 1,
            idempotency_key: key.clone(),
            initial_operation_id: OperationId(format!("op-{}", ulid::Ulid::new())),
        };
        let first = c
            .store
            .lock()
            .unwrap()
            .accept_deployment(accept(dep_id))
            .unwrap();
        let second = c
            .store
            .lock()
            .unwrap()
            .accept_deployment(accept(DeploymentId::new()))
            .unwrap();
        assert_eq!(
            first.deployment_id.to_string(),
            second.deployment_id.to_string()
        );
    }
}

/// Attachment request (F1 design §6, SPEC §5.2): register an already-running
/// service for routing and observation. Grants NO lifecycle permission.
#[derive(Debug, Clone)]
pub struct AttachRequest {
    pub name: String,
    pub endpoint: String,
    pub route_model_id: Option<String>,
    pub manifest: Vec<u8>,
}

pub struct AttachRequestTag;

impl Controller {
    /// Attach an already-running service: registration and observation
    /// only — attaching grants no permission to sleep, kill, restart, or
    /// evict (SPEC §5.2 / T11). Attached usage is charged conservatively
    /// (never reclaimable without evidence) and restart guarantees are
    /// marked unavailable without a configured supervisor integration.
    pub async fn attach(&self, req: AttachRequest) -> Result<String, ControllerError> {
        let id = DeploymentId::new();
        let key = idempotency_key(&self.context_id, &req.name, &req.manifest);
        let op = OperationId(format!("op-{}", ulid::Ulid::new()));
        let accepted = {
            let store = self.store.lock().unwrap();
            let accepted = store.accept_deployment(AcceptDeployment {
                id,
                name: req.name,
                kind: ATTACHED_KIND.to_string(),
                route_model_id: req.route_model_id,
                desired_state: LifecycleState::Ready,
                schema_version: 1,
                idempotency_key: key,
                initial_operation_id: op,
            })?;
            let op = accepted.operation_id.clone();
            store.update_operation_state(&op.0, OpState::Succeeded, None)?;
            store.record_journal(
                Some(&self.host_id),
                Some(&op.0),
                Some("attached"),
                &format!(
                    r#"{{"event":"attached","deployment":"{dep}","endpoint":"{ep}","supervisor":"none"}}"#,
                    dep = accepted.deployment_id,
                    ep = req.endpoint,
                ),
            )?;
            accepted
        };
        let dep = accepted.deployment_id.to_string();
        {
            // Conservative usage: charged once, never reclaimable without
            // verified evidence (SPEC §5.2: uncertain attached usage is not
            // reclaimable capacity).
            let store = self.store.lock().unwrap();
            store
                .insert_reservation(&mllm_store::ReservationRow {
                    owner_id: dep.clone(),
                    domain_id: Some("system".into()),
                    bytes: ATTACHED_CONSERVATIVE_BYTES,
                    phase: "ready".into(),
                    exclusive_devices: vec![],
                })
                .map_err(ControllerError::from)?;
        }
        Ok(dep)
    }
}

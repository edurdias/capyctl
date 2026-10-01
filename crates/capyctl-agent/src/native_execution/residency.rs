//! SPEC §§6.1, 9.1, 10, 13: remote Park and Restore on the host agent (W4).
//!
//! One engine-generic contract, executed on the host that owns the launch:
//!
//! Park (expected state `ready`): the journal persists `parking` before any
//! effect; the host closes the launch's ingress gate; ingress in-flight must be
//! zero and the adapter must prove engine quiescence (SPEC §10 steps 3–5); then
//! the adapter's persisted `Park` step runs, the journaled process group must
//! still be the same live group (PID, boot and start identity), and `parked`
//! is persisted with its evidence before the result is sent. The gate stays
//! closed; a park never claims a usable model.
//!
//! Restore (expected state `parked`): `restoring` is persisted first; the
//! adapter's `Restore`, `ReloadWeights` and `InvalidateCache` steps run in
//! order (for vLLM: weights wake, `reload_weights`, KV wake and prefix reset,
//! SPEC §9.1); the group must be unchanged; then a fresh native model probe
//! (SPEC §6.1) must pass. Only then is the launch persisted resident with a
//! usable model, and only then does the gate reopen for this session.
//!
//! A refusal before any engine call leaves the launch as it was (`unchanged`).
//! Any failure after an engine call is dispatched is `uncertain`: the claim is
//! retained, the gate stays closed, nothing is retried, and only a Terminate
//! settles the launch (SPEC §13.2, T20).
//!
//! SGLang uses its existing persisted control path, observed through a host
//! observer that fuses the saver's mapped bytes (`saver_source`: the enrolled
//! scheduler's saver map, SPEC §9.2), the journaled group's liveness, ingress
//! in-flight counts and the engine's own running and waiting gauges. A release
//! counts only when the saver shows every allocation unmapped, not when the
//! release route answers; a partly mapped saver is unknown. A host with no
//! saver observation source, or a launch that enrolled none, cannot prove
//! release or restoration, so SGLang residency is refused there before any
//! engine call rather than assumed.
use super::saver_source::{content, read_saver, saver_facts};
use super::{fresh_probe, NativeHostExecution, PROBE_MARGIN_MS};
use crate::{
    ingress::IngressScope,
    journal::{ExecutionTicket, ResidencyOutcome, ResidencyStart},
    session::SessionError,
};
use capyctl_adapters::{
    sglang::{SglangAdapter, SglangRuntimeObservation, SglangRuntimeObserver},
    traits::{EngineAdapter, MemberRef, RuntimeAction, RuntimeCommand, RuntimeError},
    vllm::VllmAdapter,
};
use capyctl_config::engine_policy::Engine;
use capyctl_domain::{
    completion::{
        EffectObservation, ExecutionIdentities, ProcessIdentity, StepExecutionContext,
        TransitionToken,
    },
    launch::NativeLaunch,
};
use capyctl_protocol::{
    execution::{MemberAction, MemberCommand, SingleLaunchPlan},
    pb,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

/// Mapped and reserved saver bytes of one SGLang launch on its single device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaverMapped {
    /// The saver library is the approved one (by digest), not a stand-in.
    pub real_saver: bool,
    pub weight_bytes: u64,
    pub kv_bytes: u64,
    /// The saver allocations of each tag, mapped or paused.
    pub weight_virtual_bytes: u64,
    pub kv_virtual_bytes: u64,
}

impl SaverMapped {
    pub fn mapped_bytes(&self) -> u64 {
        self.weight_bytes.saturating_add(self.kv_bytes)
    }
}

/// The saver observation could not be read or did not match the launch.
#[derive(Debug, thiserror::Error)]
#[error("saver observation unavailable")]
pub struct SaverUnavailable;

/// The launch one saver read is about.
#[derive(Clone)]
pub struct SaverScope {
    pub binding_id: String,
    pub incarnation: String,
    /// The launch's recorded processes; the enrolled scheduler must be one of
    /// them. `None` only before a step names them (an embedded quiescence
    /// check), when it must be a live process of this boot instead.
    pub members: Option<Vec<ProcessIdentity>>,
    /// The launch's admin credential (hex), from which its observation key is
    /// derived.
    pub admin_key: String,
    /// The installation interpreter the launch runs.
    pub executable: String,
}

/// SPEC §13.3: the admin credential is never formatted.
impl std::fmt::Debug for SaverScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SaverScope")
            .field("binding_id", &self.binding_id)
            .field("incarnation", &self.incarnation)
            .field("members", &self.members)
            .field(
                "admin_key",
                &capyctl_adapters::traits::redacted(!self.admin_key.is_empty()),
            )
            .field("executable", &self.executable)
            .finish()
    }
}

/// Fresh saver-map facts for exactly one launch. Implementations must read the
/// enrolled runtime's observation surface for this binding, incarnation and
/// process group, and fail when any of those does not match.
pub trait SaverResidency: Send + Sync {
    fn mapped(&self, scope: &SaverScope) -> Result<SaverMapped, SaverUnavailable>;

    /// The private directory launches enroll their observation in, if any.
    fn observation_dir(&self) -> Option<&std::path::Path> {
        None
    }

    /// Forget a launch proven gone: its enrollment is no longer evidence.
    fn retire(&self, _binding_id: &str) {}
}

type Liveness = Arc<dyn Fn() -> Result<Vec<ProcessIdentity>, ()> + Send + Sync>;
type InFlight = Arc<dyn Fn() -> Result<usize, ()> + Send + Sync>;
/// SPEC §10 step 4: the engine's own running and waiting gauges are at zero.
pub(super) type EngineIdle =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<bool, ()>> + Send>> + Send + Sync>;

/// The host's SGLang residency observer for one step. The first observation is
/// the adapter's precondition, every later one its post-condition.
pub(super) struct HostSglangObserver {
    token: TransitionToken,
    scope: SaverScope,
    saver: Arc<dyn SaverResidency>,
    live: Liveness,
    in_flight: InFlight,
    idle: EngineIdle,
    content: [(bool, bool); 2],
    calls: AtomicUsize,
}

impl HostSglangObserver {
    async fn observation(&self) -> SglangRuntimeObservation {
        let (weights, cache) = self.content[self.calls.fetch_add(1, Ordering::SeqCst).min(1)];
        let mapped = read_saver(&self.saver, self.scope.clone()).await;
        let facts = saver_facts(&mapped);
        let identities = (self.live)();
        let in_flight = (self.in_flight)();
        let idle = (self.idle)().await;
        SglangRuntimeObservation {
            token: self.token.clone(),
            binding_id: self.scope.binding_id.clone(),
            incarnation: self.scope.incarnation.clone(),
            unknown_work: facts.is_none()
                || identities.is_err()
                || in_flight.is_err()
                || idle.is_err(),
            identities: identities.unwrap_or_default(),
            real_memory_saver: facts.is_some_and(|f| f.real_saver),
            // SPEC §10 step 4: the ingress is the only path to the engine's
            // inference surface, so zero forwarded requests with the gate
            // closed, and the engine's own gauges at zero, are the host's
            // quiescence evidence.
            quiesced: in_flight.is_ok_and(|n| n == 0) && idle == Ok(true),
            allocations: facts.is_some_and(|f| f.resident),
            weights: weights && facts.is_some_and(|f| f.resident),
            cache: cache && facts.is_some_and(|f| f.resident),
        }
    }
}

impl SglangRuntimeObserver for HostSglangObserver {
    fn observe<'a, 'b>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<SglangRuntimeObservation, RuntimeError>> + Send + 'b>>
    where
        'a: 'b,
        Self: 'b,
    {
        Box::pin(async move { Ok(self.observation().await) })
    }
}

/// The engine-specific driver of one Park or Restore.
enum Driver {
    Vllm(Box<VllmAdapter>),
    Sglang {
        frozen: Box<NativeLaunch>,
        inference: String,
        admin: String,
        saver: Option<Arc<dyn SaverResidency>>,
        live: Liveness,
        in_flight: InFlight,
        idle: EngineIdle,
    },
}

/// Why a step did not complete.
enum StepFailure {
    /// Refused before any engine call.
    Refused,
    /// An engine call was dispatched; its outcome is unknown.
    Uncertain,
}

struct Run<'a> {
    driver: Driver,
    command: &'a MemberCommand,
    plan: &'a SingleLaunchPlan,
    expected: &'a [ProcessIdentity],
    stop_at_ms: i64,
}

impl Run<'_> {
    fn step(&self, action: RuntimeAction) -> RuntimeCommand {
        let id = &self.command.identity;
        RuntimeCommand {
            action,
            context: StepExecutionContext {
                token: TransitionToken {
                    deployment_id: id.deployment_id.clone(),
                    revision: id.revision,
                    generation: id.generation,
                    operation_id: id.operation_id.clone(),
                    step_id: format!("{}.{}", id.step_id, step_name(action)),
                },
                binding_id: self.plan.binding_id.clone(),
                incarnation: self.plan.incarnation.clone(),
                issued_at_ms: capyctl_protocol::now_unix_ms(),
                deadline_ms: self.stop_at_ms,
                identities: ExecutionIdentities::Retained(self.expected.to_vec()),
                completion_target: None,
                grant_id: Some(self.plan.grant_id.clone()),
                launch_settings: None,
            },
        }
    }

    /// The saver scope of this launch: its recorded processes and credential.
    fn saver_scope(&self) -> Option<SaverScope> {
        let Driver::Sglang { frozen, admin, .. } = &self.driver else {
            return None;
        };
        self.expected.iter().find(|p| p.role == "api")?;
        Some(SaverScope {
            binding_id: self.plan.binding_id.clone(),
            incarnation: self.plan.incarnation.clone(),
            members: Some(self.expected.to_vec()),
            admin_key: admin.clone(),
            executable: frozen.executable().into(),
        })
    }

    fn sglang_observer(
        &self,
        command: &RuntimeCommand,
        saver: &Arc<dyn SaverResidency>,
        live: &Liveness,
        in_flight: &InFlight,
        idle: &EngineIdle,
    ) -> Option<HostSglangObserver> {
        Some(HostSglangObserver {
            token: command.context.token.clone(),
            scope: self.saver_scope()?,
            saver: saver.clone(),
            live: live.clone(),
            in_flight: in_flight.clone(),
            idle: idle.clone(),
            content: content(command.action),
            calls: AtomicUsize::new(0),
        })
    }

    /// SPEC §9.2 evidence: the saver's mapped bytes now, when readable.
    async fn saver_mapped_bytes(&self) -> Option<u64> {
        let Driver::Sglang {
            saver: Some(saver), ..
        } = &self.driver
        else {
            return None;
        };
        let scope = self.saver_scope()?;
        read_saver(saver, scope)
            .await
            .ok()
            .map(|m| m.mapped_bytes())
    }

    /// SPEC §10 step 4: engine quiescence, proven by the adapter's own
    /// evidence, with no engine effect.
    async fn quiescent(&self) -> bool {
        match &self.driver {
            Driver::Vllm(vllm) => {
                let member = MemberRef {
                    deployment_id: self.command.identity.deployment_id.clone(),
                    member_id: self.plan.binding_id.clone(),
                };
                vllm.prepare_park(&member).await.is_ok_and(|q| q.quiescent)
            }
            Driver::Sglang {
                saver: Some(saver),
                live,
                in_flight,
                idle,
                ..
            } => {
                let command = self.step(RuntimeAction::Park);
                let Some(observer) = self.sglang_observer(&command, saver, live, in_flight, idle)
                else {
                    return false;
                };
                let o = observer.observation().await;
                let ready = o.real_memory_saver && o.quiesced && !o.unknown_work && o.allocations;
                if !ready {
                    // Found live 2026-09-23 (M28): a refused SGLang park said
                    // only `unchanged`. Which fact failed, and the saver's
                    // bytes per tag when readable, on the host's own log.
                    let saver = match self.saver_scope() {
                        Some(scope) => read_saver(saver, scope).await.ok(),
                        None => None,
                    }
                    .map(|m| {
                        serde_json::json!({
                            "real_saver": m.real_saver,
                            "weight_mapped": m.weight_bytes,
                            "weight_virtual": m.weight_virtual_bytes,
                            "kv_mapped": m.kv_bytes,
                            "kv_virtual": m.kv_virtual_bytes,
                        })
                    });
                    capyctl_domain::role_log::event(serde_json::json!({
                        "event": "sglang_park_not_quiescent",
                        "binding": self.plan.binding_id,
                        "real_memory_saver": o.real_memory_saver,
                        "quiesced": o.quiesced,
                        "unknown_work": o.unknown_work,
                        "allocations_resident": o.allocations,
                        "saver": saver,
                    }));
                }
                ready
            }
            Driver::Sglang { saver: None, .. } => false,
        }
    }

    /// One persisted adapter step. `first` marks the step before which the
    /// launch is untouched, where a refusal leaves it unchanged.
    async fn run(
        &self,
        action: RuntimeAction,
        first: bool,
    ) -> Result<EffectObservation, StepFailure> {
        let command = self.step(action);
        let result = match &self.driver {
            Driver::Vllm(vllm) => vllm.execute_persisted(&command).await,
            Driver::Sglang {
                frozen,
                inference,
                admin,
                saver,
                live,
                in_flight,
                idle,
            } => {
                let Some(saver) = saver else {
                    return Err(StepFailure::Refused);
                };
                let Some(observer) = self.sglang_observer(&command, saver, live, in_flight, idle)
                else {
                    return Err(StepFailure::Refused);
                };
                // The SGLang adapter reports every refusal as uncertain. Check
                // its precondition here first, so a launch this host cannot
                // prove ready for the step is refused without an engine call.
                let before = observer.observation().await;
                observer.calls.store(0, Ordering::SeqCst);
                if first
                    && (!before.real_memory_saver
                        || before.unknown_work
                        || !before.quiesced
                        || before.allocations != (action == RuntimeAction::Park))
                {
                    return Err(StepFailure::Refused);
                }
                match SglangAdapter::from_frozen(frozen, Some(Arc::new(observer))) {
                    Ok(adapter) => {
                        adapter
                            .with_credentials(inference.clone(), admin.clone())
                            .execute_persisted(&command)
                            .await
                    }
                    Err(_) => return Err(StepFailure::Refused),
                }
            }
        };
        match result {
            Ok(observation) => Ok(observation),
            // The vLLM adapter's `Unsupported` is a refusal before any call.
            Err(RuntimeError::Unsupported) if first => Err(StepFailure::Refused),
            Err(_) => Err(StepFailure::Uncertain),
        }
    }
}

fn step_name(action: RuntimeAction) -> &'static str {
    match action {
        RuntimeAction::Park => "park",
        RuntimeAction::Restore => "restore",
        RuntimeAction::ReloadWeights => "reload",
        RuntimeAction::InvalidateCache => "cache",
        _ => "step",
    }
}

fn mem_available() -> i64 {
    crate::memory::read_host_memory()
        .map(|sample| sample.memory.available_bytes)
        .unwrap_or(-1)
}

impl NativeHostExecution {
    /// SPEC §§9.1, 10, 13: execute one accepted Park or Restore. Ordinary
    /// refusals and failures are evidence in the persisted result; only a
    /// journal or session failure is an error here.
    pub(super) async fn residency(
        &self,
        session: u64,
        ticket: ExecutionTicket,
        command: &MemberCommand,
    ) -> Result<(), SessionError> {
        let journal = self.journal.clone();
        let policy = Arc::new(self.clone());
        let start = tokio::task::spawn_blocking(move || {
            journal.begin_residency(ticket, capyctl_protocol::now_unix_ms(), policy.as_ref())
        })
        .await
        .map_err(|_| SessionError)?
        .map_err(|_| SessionError)?;
        let ResidencyStart::Begun { owner, expected } = start else {
            return Ok(());
        };
        // SPEC §§9.1, 13.2: the readiness this session proved for the launch,
        // held aside so a Park refused after the gate closed can give it back.
        let prior = self.ready_claim(session, &owner.identity.command_id);
        let (outcome, evidence) = match self.drive(command, &owner, &expected).await {
            Ok(done) => done,
            // The intent is durable and nothing proved the launch unchanged.
            Err(_) => (ResidencyOutcome::Uncertain, evidence(-1, -1, Vec::new())),
        };
        // A refused Park left the engine as it was, so its readiness stands,
        // but only for the very group that readiness proved: a changed or
        // unverifiable group keeps the gate closed and the readiness revoked,
        // and only a fresh probe (or the exit report) settles it.
        let reopen = match (&command.action, outcome, &prior) {
            (MemberAction::Park { .. }, ResidencyOutcome::Unchanged, Some(_)) => {
                self.unchanged_group(&owner.identity.command_id, &expected)
                    .await
            }
            _ => false,
        };
        let journal = self.journal.clone();
        let id = command.identity.command_id.clone();
        let retained = expected.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            journal.finish_residency(
                &id,
                outcome,
                evidence,
                &retained,
                capyctl_protocol::now_unix_ms(),
            )
        })
        .await
        .map_err(|_| SessionError)?
        .map_err(|_| SessionError)?;
        if outcome == ResidencyOutcome::Restored {
            // SPEC §6.1: the gate reopens only on the fresh probe's persisted
            // readiness, and only for this session.
            let scope = self.register_retained(&owner)?;
            self.publish_ready(
                session,
                &owner.identity.command_id,
                &command.identity.command_id,
                &scope,
            )?;
        }
        if let (true, ResidencyOutcome::Unchanged, Some(ready)) = (reopen, outcome, prior) {
            // SPEC §§9.1, 13.2: `unchanged` means unchanged. The readiness
            // proved earlier in this session reopens the gate the attempt
            // closed; a session that ended meanwhile refuses it here.
            let scope = self.register_retained(&owner)?;
            let _ = self.publish_ready(session, &owner.identity.command_id, &ready, &scope);
        }
        Ok(())
    }

    /// The command whose persisted readiness holds `owned_handle`'s gate in
    /// `session`, if any.
    fn ready_claim(&self, session: u64, owned_handle: &str) -> Option<String> {
        let authority = self.authority.lock().ok()?;
        if authority.session != Some(session) {
            return None;
        }
        authority.ready.get(owned_handle).cloned()
    }

    /// ADR 0014 §7: the retained launch's checkpoint still measures to the
    /// digest its plan recorded (stat identity plus small-file rehash; a full
    /// rehash on any change).
    ///
    /// Owner decision 5 (2026-09-22): a plan journaled before WE3 recorded no
    /// digest. Its park journals the digest this host measured then
    /// (`journal_park_digest`), and its wake verifies against that journaled
    /// measurement: a Restore naming another digest is refused, so a
    /// checkpoint swapped while parked is never woken on the controller's
    /// word. A pre-WE3 launch parked before the park digest was journaled has
    /// none; it alone is still woken against the digest the server sends with
    /// the Restore (`supplied`), which the server records from this host's own
    /// measurement first. Without either it is refused. A Restore naming a
    /// digest other than the plan's is refused.
    pub(super) async fn checkpoint_unchanged(
        &self,
        owner: &MemberCommand,
        plan: &SingleLaunchPlan,
        supplied: &str,
    ) -> bool {
        let journaled = if plan.checkpoint_digest.is_empty() {
            let journal = self.journal.clone();
            let handle = owner.identity.command_id.clone();
            match tokio::task::spawn_blocking(move || journal.park_digest(&handle)).await {
                Ok(Ok(journaled)) => journaled,
                _ => return false,
            }
        } else {
            None
        };
        let recorded = journaled
            .as_deref()
            .unwrap_or(plan.checkpoint_digest.as_str());
        let digest = match (recorded, supplied) {
            ("", "") => return false,
            ("", supplied) => supplied,
            (recorded, "") => recorded,
            (recorded, supplied) if recorded == supplied => recorded,
            _ => return false,
        };
        let Ok(effective) = self.resolve_retained(owner) else {
            return false;
        };
        let host = self.clone();
        let plan = SingleLaunchPlan {
            checkpoint_digest: digest.to_owned(),
            ..plan.clone()
        };
        tokio::task::spawn_blocking(move || host.verify_checkpoint(&effective, &plan).is_ok())
            .await
            .unwrap_or(false)
    }

    /// ADR 0014 §7 (WE3), owner decision 5: journal this host's own
    /// measurement of a parked pre-WE3 launch's checkpoint, which its wake
    /// then verifies against. A plan that records a digest needs none; a
    /// measurement that fails journals nothing (the wake falls back as
    /// documented on `checkpoint_unchanged`).
    pub(super) async fn journal_park_digest(&self, owner: &MemberCommand) {
        let MemberAction::LaunchSingle(plan) = &owner.action else {
            return;
        };
        if !plan.checkpoint_digest.is_empty() {
            return;
        }
        let Ok(effective) = self.resolve_retained(owner) else {
            return;
        };
        let Ok(checkpoint) = effective.model.require_resolved_path().map(str::to_owned) else {
            return;
        };
        let checkpoints = self.checkpoints.clone();
        let journal = self.journal.clone();
        let handle = owner.identity.command_id.clone();
        let store = effective.checkpoint_store().to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            let measured = checkpoints
                .measure(&store, std::path::Path::new(&checkpoint))
                .ok()?;
            journal
                .record_park_digest(&handle, &measured.manifest.digest)
                .ok()
        })
        .await;
    }

    async fn drive(
        &self,
        command: &MemberCommand,
        owner: &MemberCommand,
        expected: &[ProcessIdentity],
    ) -> Result<(ResidencyOutcome, pb::ResidencyEvidence), SessionError> {
        let MemberAction::LaunchSingle(plan) = &owner.action else {
            return Err(SessionError);
        };
        let unchanged = |milestones: Vec<String>| {
            Ok((ResidencyOutcome::Unchanged, evidence(-1, -1, milestones)))
        };
        // Gate first: whatever happens next, nothing new is forwarded.
        let Ok(scope) = self.register_retained(owner) else {
            return unchanged(Vec::new());
        };
        self.ingress.close(&scope).map_err(|_| SessionError)?;
        self.revoke_ready(&owner.identity.command_id)?;
        let mut milestones = vec!["gate_closed".to_string()];
        let driver = match self.driver(owner, plan, &scope) {
            Ok(driver) => driver,
            Err(_) => return unchanged(milestones),
        };
        let run = Run {
            driver,
            command,
            plan,
            expected,
            stop_at_ms: command.identity.deadline_ms - PROBE_MARGIN_MS,
        };
        match &command.action {
            MemberAction::Park { .. } => {
                // SPEC §10 steps 4–5: drained at ingress, quiescent at the engine.
                if self.ingress.current_requests(&scope).ok() != Some(0) {
                    return unchanged(milestones);
                }
                milestones.push("ingress_idle".into());
                if !run.quiescent().await {
                    return unchanged(milestones);
                }
                milestones.push("engine_quiescent".into());
                let before = mem_available();
                // SPEC §9.2: the saver's mapped bytes on both sides of the
                // release are part of the evidence (SGLang only).
                if let Some(bytes) = run.saver_mapped_bytes().await {
                    milestones.push(format!("saver_mapped_before.{bytes}"));
                }
                match run.run(RuntimeAction::Park, true).await {
                    Ok(_) => {
                        milestones.push("memory_released".into());
                        if let Some(bytes) = run.saver_mapped_bytes().await {
                            milestones.push(format!("saver_mapped_after.{bytes}"));
                        }
                    }
                    Err(StepFailure::Refused) => return unchanged(milestones),
                    Err(StepFailure::Uncertain) => {
                        return Ok((
                            ResidencyOutcome::Uncertain,
                            evidence(before, mem_available(), milestones),
                        ))
                    }
                }
                if !self
                    .unchanged_group(&owner.identity.command_id, expected)
                    .await
                {
                    return Ok((
                        ResidencyOutcome::Uncertain,
                        evidence(before, mem_available(), milestones),
                    ));
                }
                milestones.push("identity_unchanged".into());
                // Owner decision 5: a pre-WE3 launch's wake verifies against
                // what this host measured now.
                self.journal_park_digest(owner).await;
                Ok((
                    ResidencyOutcome::Parked,
                    evidence(before, mem_available(), milestones),
                ))
            }
            MemberAction::Restore {
                checkpoint_digest, ..
            } => {
                let before = mem_available();
                let uncertain = |milestones: Vec<String>| {
                    Ok((
                        ResidencyOutcome::Uncertain,
                        evidence(before, mem_available(), milestones),
                    ))
                };
                // ADR 0014 §7 (WE3): waking reloads weights from disk (SPEC
                // §9.1), so a checkpoint changed under a parked engine would be
                // silent model substitution (SPEC §10). It is measured against
                // the recorded digest first and refused before any engine call.
                if !self
                    .checkpoint_unchanged(owner, plan, checkpoint_digest)
                    .await
                {
                    return unchanged(milestones);
                }
                milestones.push("checkpoint_verified".into());
                for (action, name, first) in [
                    (RuntimeAction::Restore, "allocations_restored", true),
                    (RuntimeAction::ReloadWeights, "weights_usable", false),
                    (RuntimeAction::InvalidateCache, "cache_valid", false),
                ] {
                    match run.run(action, first).await {
                        Ok(_) => milestones.push(name.into()),
                        Err(StepFailure::Refused) if first => return unchanged(milestones),
                        Err(_) => return uncertain(milestones),
                    }
                }
                if let Some(bytes) = run.saver_mapped_bytes().await {
                    milestones.push(format!("saver_mapped_after.{bytes}"));
                }
                if !self
                    .unchanged_group(&owner.identity.command_id, expected)
                    .await
                {
                    return uncertain(milestones);
                }
                // SPEC §6.1: waking and reloading are not readiness. The same
                // fresh probe a reconnect uses must pass before `restored`.
                // Owner decision 2026-09-22: a retained launch (pre-E1
                // included) is resolved as retained, never re-rendered.
                let effective = self.resolve_retained(owner).map_err(|_| SessionError)?;
                let keys = self
                    .identities
                    .load(&scope, owner.identity.payload_digest)
                    .map_err(|_| SessionError)?;
                let served = effective.routes.first().cloned().ok_or(SessionError)?;
                let probe = self.probe_adapter(&effective, plan, &keys, &served)?;
                let member = MemberRef {
                    deployment_id: owner.identity.deployment_id.clone(),
                    member_id: plan.binding_id.clone(),
                };
                if fresh_probe(probe.as_ref(), &member, &served, run.stop_at_ms)
                    .await
                    .is_err()
                {
                    return uncertain(milestones);
                }
                milestones.push("model_usable".into());
                if !self
                    .unchanged_group(&owner.identity.command_id, expected)
                    .await
                {
                    return uncertain(milestones);
                }
                milestones.push("identity_unchanged".into());
                Ok((
                    ResidencyOutcome::Restored,
                    evidence(before, mem_available(), milestones),
                ))
            }
            _ => Err(SessionError),
        }
    }

    /// The engine driver for the retained launch, from local policy only.
    fn driver(
        &self,
        owner: &MemberCommand,
        plan: &SingleLaunchPlan,
        scope: &IngressScope,
    ) -> Result<Driver, SessionError> {
        // Owner decision 2026-09-22: the retained launch's approved
        // configuration, which for a pre-E1 launch is today's document (the
        // same resolution `authorize_residency` admitted it under).
        let effective = self.resolve_retained(owner).map_err(|_| SessionError)?;
        let keys = self
            .identities
            .load(scope, owner.identity.payload_digest)
            .map_err(|_| SessionError)?;
        let served = effective.routes.first().cloned().ok_or(SessionError)?;
        Ok(match effective.profile.engine {
            Engine::Vllm => Driver::Vllm(Box::new(
                self.vllm_adapter(&effective, plan, &keys, &served)?,
            )),
            Engine::Sglang => {
                let journal = self.journal.clone();
                let handle = owner.identity.command_id.clone();
                let ingress = self.ingress.clone();
                let gate = scope.clone();
                let frozen = self.sglang_frozen(&effective, plan, &served)?;
                let endpoint = frozen.metadata().endpoint.clone();
                let inference = hex::encode(keys.inference);
                let metrics_key = inference.clone();
                Driver::Sglang {
                    frozen: Box::new(frozen),
                    inference,
                    admin: hex::encode(keys.admin),
                    saver: self.saver.clone(),
                    // SPEC §10 step 4: the engine's own gauges, on loopback
                    // with the launch's inference key.
                    idle: Arc::new(move || {
                        let endpoint = endpoint.clone();
                        let key = metrics_key.clone();
                        Box::pin(async move {
                            capyctl_adapters::sglang::observation::engine_idle(&endpoint, &key)
                                .await
                                .map_err(|_| ())
                        })
                    }),
                    live: Arc::new(move || {
                        journal
                            .inspect_owned(&handle)
                            .map(|observed| {
                                observed
                                    .into_iter()
                                    .filter(|(_, presence)| {
                                        *presence == capyctl_domain::completion::Presence::Alive
                                    })
                                    .map(|(identity, _)| identity)
                                    .collect()
                            })
                            .map_err(|_| ())
                    }),
                    in_flight: Arc::new(move || ingress.current_requests(&gate).map_err(|_| ())),
                }
            }
            // ADR 0023 §6: TensorFold never parks.
            Engine::Tensorfold => return Err(SessionError),
        })
    }

    async fn unchanged_group(&self, owned_handle: &str, expected: &[ProcessIdentity]) -> bool {
        let journal = self.journal.clone();
        let handle = owned_handle.to_string();
        let expected = expected.to_vec();
        tokio::task::spawn_blocking(move || journal.group_unchanged(&handle, &expected))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false)
    }
}

fn evidence(before: i64, after: i64, milestones: Vec<String>) -> pb::ResidencyEvidence {
    pb::ResidencyEvidence {
        // The journal sets the state from the persisted outcome.
        state: "unknown".into(),
        mem_available_before_bytes: before,
        mem_available_after_bytes: after,
        milestones,
    }
}

#[cfg(test)]
mod tests;

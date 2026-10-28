//! SPEC §§3, 6, 13: remote effects use the ordinary coordinator's armed step.
//! Authenticated host observations retain ownership; local PID lookup is forbidden.
use crate::{
    agent_sessions::{AgentSessions, ProvisionOutcome, RemoteEvidenceObserver},
    coordinator::{CoordinatorError, ExecutionBinding, SettlementContext},
    ownership::SharedCoordinatorState,
};
use capyctl_adapters::traits::*;
use capyctl_domain::{
    completion::{CleanupEvidence, EffectObservation, Milestone, ProcessIdentity},
    group::{CommandIdentity, MemberKey},
};
use capyctl_protocol::{
    capabilities,
    execution::{MemberAction, MemberCommand, SingleLaunchPlan},
    pb,
};
use capyctl_store::ordinary_lifecycle::cleanup::CleanupExecutionContext;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// SPEC §§6.1, 13.2 (G2): which authenticated host session last proved each
/// remote binding's model readiness. Readiness is that session's alone: once the
/// host's current session differs, dispatch closes until a fresh probe passes.
pub type ReadinessLedger = Arc<Mutex<BTreeMap<String, String>>>;

pub struct RemoteLaunchBinding {
    pub controller_id: String,
    pub host_id: String,
    pub member_id: String,
    pub profile_fingerprint: String,
    pub launch_command_id: String,
    pub plan: SingleLaunchPlan,
    pub ingress_gate_key: [u8; 32],
    /// ADR 0019 (discrete GPU design §8): the launch's footprint names a
    /// device memory domain. Every command of it then needs the host's
    /// `device_memory_domains`; a host without it is refused typed before
    /// anything is sent.
    pub device_memory: bool,
    /// ADR 0030 §1: the launch serves decision models (`api: decisions`).
    /// Every launch, park and wake of it then needs the host's `decisions`;
    /// a host without it is refused typed before anything is sent.
    pub decisions: bool,
    /// ADR 0031 §9: the launch runs a custom engine. Every command of it then
    /// needs the host's `custom_engines`; a host without it is refused typed
    /// before anything is sent.
    pub custom_engine: bool,
    /// ADR 0033 §13: the deployment states `lmcache: required`. Every
    /// command of it then needs the host's `lmcache`; a host without it is
    /// refused typed before anything is sent.
    pub lmcache_required: bool,
    /// ADR 0033 §§10, 13: the deployment states `retention: zero`. Every
    /// command of it then needs the host's `retention`; a host without it
    /// would not honour it, and is refused typed before anything is sent.
    pub retention_zero: bool,
    /// ADR 0039 §15: the launch's effective profile is TensorFold's native
    /// line. Every command of it then needs the host's `tensorfold_native`; a
    /// host without it is refused typed before anything is sent.
    pub tensorfold_native: bool,
    /// ADR 0013 §5: the instance every command of this launch names, so the
    /// host fences it by that instance's own generation. Zero on a host that
    /// does not advertise per-instance fencing: its journal fences per
    /// deployment, and a nonzero index would not survive its digest check.
    pub instance_index: u32,
}
impl RemoteLaunchBinding {
    /// ADR 0033 §13: the capabilities the deployment's declared `lmcache`
    /// and `retention` need of the host.
    fn lmcache_needs(&self, needs: &mut Vec<&'static str>) {
        if self.lmcache_required {
            needs.push(capabilities::LMCACHE);
        }
        if self.retention_zero {
            needs.push(capabilities::RETENTION);
        }
    }
}
struct RemoteEngine {
    sessions: Arc<AgentSessions>,
    owner: SharedCoordinatorState,
    binding: Arc<RemoteLaunchBinding>,
    readiness: ReadinessLedger,
    /// ADR 0033 (lifecycle design §10): what the host reported about the
    /// launch's LMCache, shared with the coordinator's driver of the launch.
    lmcache: Arc<capyctl_adapters::lmcache::LmcacheState>,
}
/// The caller resolves this frozen plan from the selected enrolled host's
/// approved configuration. This is construction only, never permission to send.
pub fn binding(
    sessions: Arc<AgentSessions>,
    owner: SharedCoordinatorState,
    binding: RemoteLaunchBinding,
    readiness: ReadinessLedger,
) -> ExecutionBinding {
    let binding = Arc::new(binding);
    let lmcache = Arc::new(capyctl_adapters::lmcache::LmcacheState::default());
    let engine = Arc::new(RemoteEngine {
        sessions: sessions.clone(),
        owner,
        binding: binding.clone(),
        readiness,
        lmcache: lmcache.clone(),
    });
    remote_binding(engine, sessions, binding).with_lmcache(lmcache)
}

/// The engine half of [`binding`] alone: the adapter the coordinator drives
/// for this launch's Initialize, Park and Restore steps. Tests drive it
/// directly; production reaches it only through [`binding`].
pub fn engine(
    sessions: Arc<AgentSessions>,
    owner: SharedCoordinatorState,
    binding: RemoteLaunchBinding,
    readiness: ReadinessLedger,
) -> Arc<dyn EngineAdapter> {
    Arc::new(RemoteEngine {
        sessions,
        owner,
        binding: Arc::new(binding),
        readiness,
        lmcache: Arc::default(),
    })
}

fn remote_binding(
    engine: Arc<RemoteEngine>,
    sessions: Arc<AgentSessions>,
    binding: Arc<RemoteLaunchBinding>,
) -> ExecutionBinding {
    let (settle_sessions, settle_binding) = (sessions.clone(), binding.clone());
    ExecutionBinding::remote(
        engine,
        Arc::new(move |context| {
            let sessions = sessions.clone();
            let binding = binding.clone();
            Box::pin(async move { cleanup(&sessions, &binding, context).await })
        }),
    )
    .with_settlement(Arc::new(move |context| {
        let sessions = settle_sessions.clone();
        let binding = settle_binding.clone();
        Box::pin(async move { settle(&sessions, &binding, context).await })
    }))
}
fn process(p: &pb::OwnedProcessObservation) -> ProcessIdentity {
    ProcessIdentity {
        role: p.role.clone(),
        pid: p.pid,
        boot_id: p.boot_id.clone(),
        start_ticks: p.start_ticks,
    }
}
struct OwnershipObserver {
    owner: SharedCoordinatorState,
    binding: String,
}
impl RemoteEvidenceObserver for OwnershipObserver {
    /// Spec §3: the host's evidence names a leader the store does not hold
    /// yet; it is recorded before anything else is believed of the launch.
    /// ADR 0033 §3: the LMCache server's leader is recorded the same way,
    /// so a failed or uncertain launch's settlement proves it gone too.
    fn retain(
        &self,
        command: &MemberCommand,
        result: &pb::MemberExecutionResult,
    ) -> Result<(), Box<tonic::Status>> {
        let leaders: Vec<_> = ["api", capyctl_domain::completion::LMCACHE_ROLE]
            .into_iter()
            .filter_map(|role| result.processes.iter().find(|p| p.role == role))
            .collect();
        if leaders.is_empty() {
            return Ok(());
        }
        let owner = self
            .owner
            .lock()
            .map_err(|_| tonic::Status::internal("ownership unavailable"))?;
        let fence = capyctl_store::lifecycle::DeploymentFence {
            deployment_id: command.identity.deployment_id.clone(),
            revision: command.identity.revision,
            generation: command.identity.generation,
        };
        for leader in leaders {
            owner
                .store()
                .record_launch_identity(owner.session(), &fence, &self.binding, &process(leader))
                .map_err(|_| {
                    Box::new(tonic::Status::internal(
                        "remote ownership could not be retained",
                    ))
                })?;
        }
        Ok(())
    }
}
impl RemoteEngine {
    /// ADR 0014 §7 (WE3): measure the checkpoint on the launch's host, record
    /// the digest, and return it for the launch plan. A mismatch with a
    /// declared or recorded digest refuses the launch with
    /// `checkpoint_mismatch`; a refusal or no answer leaves it unrecorded and
    /// uncertain. Either way the launch is never sent
    /// (`checkpoint_digests::first_placement_digest`).
    async fn first_placement(
        &self,
        c: &capyctl_domain::completion::StepExecutionContext,
        plan: &SingleLaunchPlan,
    ) -> Result<String, RuntimeError> {
        let b = &self.binding;
        let command = crate::checkpoint_digests::digest_command(
            &b.controller_id,
            &b.host_id,
            "head",
            &c.token.deployment_id,
            c.token.revision,
            c.token.generation,
            &b.profile_fingerprint,
            plan.deployment_config.clone(),
            plan.host_policy_fingerprint.clone(),
            c.deadline_ms,
        );
        let sessions = self.sessions.clone();
        crate::checkpoint_digests::first_placement_digest(
            &self.owner,
            &c.token.deployment_id,
            c.token.revision,
            &b.host_id,
            || async move {
                let result = sessions
                    .execute(command)
                    .await
                    .map_err(|_| crate::checkpoint_digests::MeasureError::Unavailable)?;
                crate::checkpoint_digests::measured_from(&result)
            },
        )
        .await
    }

    /// ADR 0014 §7, owner decision 5 (2026-09-22): the digest a remote wake
    /// carries. When none is recorded (a launch parked before checkpoint
    /// digests existed), the host measures the checkpoint through the WE3
    /// `DigestCheckpoint` path and the server records it, validated as on
    /// first placement, before the Restore is sent. A mismatch refuses the
    /// wake (`checkpoint_mismatch`); the launch stays parked.
    async fn wake_digest(
        &self,
        c: &capyctl_domain::completion::StepExecutionContext,
    ) -> Result<String, RuntimeError> {
        let b = &self.binding;
        let command = crate::checkpoint_digests::digest_command(
            &b.controller_id,
            &b.host_id,
            "head",
            &c.token.deployment_id,
            c.token.revision,
            c.token.generation,
            &b.profile_fingerprint,
            b.plan.deployment_config.clone(),
            b.plan.host_policy_fingerprint.clone(),
            c.deadline_ms,
        );
        let sessions = self.sessions.clone();
        crate::checkpoint_digests::wake_digest(
            &self.owner,
            &c.token.deployment_id,
            c.token.revision,
            &b.host_id,
            || async move {
                let result = sessions
                    .execute(command)
                    .await
                    .map_err(|_| crate::checkpoint_digests::MeasureError::Unavailable)?;
                crate::checkpoint_digests::measured_from(&result)
            },
        )
        .await
    }

    /// SPEC §§9.1, 10, 13 (W4 hook for W5): one remote Park or Restore of the
    /// launch this binding owns, as one host command whose id is the step id.
    ///
    /// The host runs the whole contract (gate, drain check, quiescence, engine
    /// steps, identity check, and for Restore the fresh model probe), so a
    /// remote Restore is one compound step here: it yields the four restore
    /// milestones at once, and only with a usable model. Evidence counts only
    /// when the host's live group is exactly the identities the controller
    /// retained. A host refusal without effect (`unchanged`) is `Unsupported`;
    /// anything else unproven is `Uncertain`, with the claim retained.
    async fn residency(&self, runtime: &RuntimeCommand) -> Result<EffectObservation, RuntimeError> {
        let outcome = self.residency_effect(runtime).await;
        if runtime.action == RuntimeAction::Park {
            if let Err(
                RuntimeError::Unsupported
                | RuntimeError::Refused(_)
                | RuntimeError::StaleRevision
                | RuntimeError::Missing,
            ) = &outcome
            {
                // W4 hand-off (W5): the coordinator settles a refused park at
                // once and leaves a remote launch's dispatch closed until a
                // fresh probe reopens it. Forget this binding's readiness proof
                // so the supervisor sends that probe, whether the host refused
                // (`unchanged`) or the park was refused before anything was sent
                // (ADR 0017: a drain-only host, a missing capability). Keeping
                // the proof left a Ready engine closed to dispatch for good
                // (rc.2 live validation, 2026-09-24).
                if let Ok(mut readiness) = self.readiness.lock() {
                    readiness.remove(&runtime.context.binding_id);
                }
            }
        }
        outcome
    }

    async fn residency_effect(
        &self,
        runtime: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        let c = &runtime.context;
        let b = &self.binding;
        let capyctl_domain::completion::ExecutionIdentities::Retained(recorded) = &c.identities
        else {
            return Err(RuntimeError::StaleRevision);
        };
        if c.binding_id != b.plan.binding_id
            || c.incarnation != b.plan.incarnation
            || recorded.is_empty()
        {
            return Err(RuntimeError::StaleRevision);
        }
        let park = runtime.action == RuntimeAction::Park;
        // ADR 0017: a drain-only host is never parked or woken, and a wake
        // needs the digest fields; refused typed before anything is sent.
        let mut needs = Vec::new();
        if !park {
            needs.extend([
                capabilities::CHECKPOINT_DIGEST,
                capabilities::RESTORE_CHECKPOINT_DIGEST,
            ]);
        }
        if b.instance_index != 0 {
            needs.push(capabilities::INSTANCE_INDEX);
        }
        if b.device_memory {
            needs.push(capabilities::DEVICE_MEMORY_DOMAINS);
        }
        if b.decisions {
            needs.push(capabilities::DECISIONS);
        }
        if b.custom_engine {
            needs.push(capabilities::CUSTOM_ENGINES);
        }
        if b.tensorfold_native {
            needs.push(capabilities::TENSORFOLD_NATIVE);
        }
        b.lmcache_needs(&mut needs);
        self.sessions
            .preflight(&b.host_id, &needs, true)
            .map_err(RuntimeError::Refused)?;
        // Owner decision 5: a wake carries the recorded digest, measured and
        // recorded first when there is none; nothing is sent otherwise.
        let checkpoint_digest = if park {
            String::new()
        } else {
            self.wake_digest(c).await?
        };
        let owned_handle = b.launch_command_id.clone();
        let mut command = MemberCommand {
            identity: CommandIdentity {
                controller_id: b.controller_id.clone(),
                member: MemberKey {
                    host_id: b.host_id.clone(),
                    member_id: b.member_id.clone(),
                },
                deployment_id: c.token.deployment_id.clone(),
                operation_id: c.token.operation_id.clone(),
                command_id: c.token.step_id.clone(),
                step_id: c.token.step_id.clone(),
                generation: c.token.generation,
                revision: c.token.revision,
                deadline_ms: c.deadline_ms,
                payload_digest: [0; 32],
                expected_state: if park { "ready" } else { "parked" }.into(),
                profile_fingerprint: b.profile_fingerprint.clone(),
                instance_index: b.instance_index,
            },
            action: if park {
                MemberAction::Park { owned_handle }
            } else {
                MemberAction::Restore {
                    owned_handle,
                    checkpoint_digest,
                }
            },
        };
        command.identity.payload_digest = command.canonical_digest();
        let (session, result) = self
            .sessions
            .execute_on_session(command, None)
            .await
            .map_err(|status| unresolved(&status, "remote residency change remains unresolved"))?;
        // A refused park leaves the host's gate closed; `residency` forgets
        // the readiness proof so a fresh probe reopens dispatch.
        let (alive, facts) = residency_evidence(park, recorded, &result)?;
        if !park {
            // SPEC §13.2 (G2): this readiness belongs to the host session whose
            // fresh probe proved it.
            self.readiness
                .lock()
                .map_err(|_| RuntimeError::Uncertain("readiness ledger unavailable".into()))?
                .insert(c.binding_id.clone(), session);
        }
        Ok(EffectObservation {
            token: c.token.clone(),
            binding_id: c.binding_id.clone(),
            incarnation: c.incarnation.clone(),
            identities: alive,
            observed_at_ms: result.observed_at_unix_ms,
            receipt: format!(
                "authenticated host {}",
                if park {
                    "park"
                } else {
                    "restore with fresh model probe"
                }
            ),
            facts,
            kernel_builds: Vec::new(),
        })
    }
}

/// SPEC §§6.1, 9.1, 13.2: what an authenticated Park or Restore result proves.
/// `parked` counts only without a usable-model claim, `restored` only with one,
/// and both only on completion with the claim retained and the live group
/// the engine the controller recorded (every engine process and no other; a
/// helper may be gone, ADR 0027). `unchanged` is a refusal
/// without effect; every other result is uncertain.
///
/// ADR 0033 §7 (lifecycle design §4.1): a `restart_only` park stops the
/// engine and keeps the launch's LMCache server, so its evidence names the
/// recorded server's group alone (`engine_stopped`), and the restore of such
/// a launch (which records the server alone) names a new engine beside that
/// same server. The store judges both again before it records them.
pub(crate) fn residency_evidence(
    park: bool,
    recorded: &[ProcessIdentity],
    result: &pb::MemberExecutionResult,
) -> Result<(Vec<ProcessIdentity>, Vec<Milestone>), RuntimeError> {
    use capyctl_domain::completion::{same_engine, same_lmcache};
    let alive: Vec<_> = result
        .processes
        .iter()
        .filter(|p| p.presence == "alive")
        .map(process)
        .collect();
    let restart_park = if park {
        result.residency.as_ref().is_some_and(|r| r.engine_stopped)
    } else {
        !recorded.iter().any(ProcessIdentity::is_engine)
    };
    // ADR 0027: every engine process the controller recorded and no other;
    // a helper of the group may have exited.
    let held = result.state == "completed"
        && result.claim_retained
        && if restart_park {
            // A park leaves the server alone; a restore names a new engine
            // (its `api`) beside it.
            same_lmcache(recorded, &alive)
                && if park {
                    alive.iter().all(ProcessIdentity::is_lmcache)
                } else {
                    alive.iter().any(|p| p.role == "api")
                }
        } else {
            same_engine(recorded, &alive)
        };
    let facts = match result.residency.as_ref().map(|r| r.state.as_str()) {
        Some("parked") if park && held && !result.model_usable => vec![Milestone::MemoryReleased],
        Some("restored") if !park && held && result.model_usable => vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ],
        Some("unchanged") if result.state == "completed" => return Err(RuntimeError::Unsupported),
        _ => {
            return Err(RuntimeError::Uncertain(
                "remote residency change is unproven".into(),
            ))
        }
    };
    Ok((alive, facts))
}

#[async_trait::async_trait]
impl EngineAdapter for RemoteEngine {
    async fn execute_persisted(
        &self,
        runtime: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        if matches!(runtime.action, RuntimeAction::Park | RuntimeAction::Restore) {
            return self.residency(runtime).await;
        }
        if runtime.action != RuntimeAction::Initialize {
            return Err(RuntimeError::Unsupported);
        }
        let c = &runtime.context;
        let b = &self.binding;
        if c.binding_id != b.plan.binding_id
            || c.incarnation != b.plan.incarnation
            || c.token.step_id != b.launch_command_id
            || !matches!(
                c.identities,
                capyctl_domain::completion::ExecutionIdentities::OwnedLaunch
            )
        {
            return Err(RuntimeError::StaleRevision);
        }
        let mut plan = b.plan.clone();
        plan.issued_at_ms = c.issued_at_ms;
        plan.grant_id = c.grant_id.clone().ok_or(RuntimeError::Missing)?;
        // ADR 0017: a drain-only host takes no new launch, and a launch needs
        // every field it will carry; refused typed before anything is sent
        // (no source download, no digest request, no key).
        let mut needs = vec![capabilities::CHECKPOINT_DIGEST];
        if plan.startup_bytes.is_some() {
            needs.push(capabilities::STARTUP_BYTES);
        }
        if plan.startup_host_bytes.is_some() {
            needs.push(capabilities::STARTUP_HOST_BYTES);
        }
        if plan.checkpoint_state_slot_bytes.is_some() {
            needs.push(capabilities::CHECKPOINT_STATE_SLOT);
        }
        if plan.checkpoint_tables.is_some() {
            needs.push(capabilities::CHECKPOINT_TABLES);
        }
        if plan.checkpoint_gguf.is_some() {
            needs.push(capabilities::CHECKPOINT_GGUF);
        }
        if b.instance_index != 0 {
            needs.push(capabilities::INSTANCE_INDEX);
        }
        // ADR 0008 (amendment 2026-10-08: the drafter's source too).
        if !capyctl_protocol::execution::MaterializeSourcePlan::all(
            &plan.deployment_config,
            &plan.host_policy_fingerprint,
        )
        .is_empty()
        {
            needs.push(capabilities::MODEL_SOURCES);
        }
        if b.device_memory {
            needs.push(capabilities::DEVICE_MEMORY_DOMAINS);
        }
        if b.decisions {
            needs.push(capabilities::DECISIONS);
        }
        // ADR 0031 §9: a custom engine's launch goes only to a host that
        // declared `custom_engines`.
        if b.custom_engine {
            needs.push(capabilities::CUSTOM_ENGINES);
        }
        // ADR 0039 §15: a native TensorFold launch goes only to a host that
        // declared `tensorfold_native`.
        if b.tensorfold_native {
            needs.push(capabilities::TENSORFOLD_NATIVE);
        }
        // ADR 0033 §13: `lmcache: required` and `retention: zero` go only to
        // a host that declared what honours them.
        b.lmcache_needs(&mut needs);
        self.sessions
            .preflight(&b.host_id, &needs, true)
            .map_err(RuntimeError::Refused)?;
        // ADR 0008: a declared remote source must be on this host's disk,
        // verified, before its digest is measured or anything is launched.
        // A failure or a download still running refuses the launch here,
        // before anything was sent.
        crate::model_sources::ensure_materialized(
            &self.owner,
            &self.sessions,
            &b.controller_id,
            &b.host_id,
            "head",
            &c.token.deployment_id,
            c.token.revision,
            c.token.generation,
            &b.profile_fingerprint,
            &plan.deployment_config,
            &plan.host_policy_fingerprint,
            c.deadline_ms,
        )
        .await?;
        if plan.checkpoint_digest.is_empty() {
            // ADR 0014 §7 (WE3): first placement of a revision whose digest is
            // not recorded yet. The host measures it and the server records it
            // before the launch is sent; nothing has been sent to the host yet.
            plan.checkpoint_digest = self.first_placement(c, &plan).await?;
        }
        // ADR 0035 §6: the rank-ordered GPUs, for the ranks the host names.
        let devices = plan.devices.clone();
        let mut command = MemberCommand {
            identity: CommandIdentity {
                controller_id: b.controller_id.clone(),
                member: MemberKey {
                    host_id: b.host_id.clone(),
                    member_id: b.member_id.clone(),
                },
                deployment_id: c.token.deployment_id.clone(),
                operation_id: c.token.operation_id.clone(),
                command_id: b.launch_command_id.clone(),
                step_id: c.token.step_id.clone(),
                generation: c.token.generation,
                revision: c.token.revision,
                deadline_ms: c.deadline_ms,
                payload_digest: [0; 32],
                expected_state: "reserved".into(),
                profile_fingerprint: b.profile_fingerprint.clone(),
                instance_index: b.instance_index,
            },
            action: MemberAction::LaunchSingle(plan),
        };
        command.identity.payload_digest = command.canonical_digest();
        let provisioned = self
            .sessions
            .provision_ingress(&command, b.ingress_gate_key)
            .await
            .map_err(|status| unresolved(&status, "remote ingress provision remains unresolved"))?;
        // SPEC §13 (WE3 limit 1): the host refused on its own policy, for
        // example a checkpoint that no longer matches the recorded digest.
        // Nothing was stored or started and the launch is never sent; the
        // failed launch settles on the host's evidence at once instead of
        // being redelivered until the Initialize deadline.
        if let ProvisionOutcome::Refused(reason) = provisioned {
            return Err(RuntimeError::Refused(reason));
        }
        let observer = Arc::new(OwnershipObserver {
            owner: self.owner.clone(),
            binding: c.binding_id.clone(),
        });
        let (session, result) = self
            .sessions
            .execute_on_session(command, Some(observer))
            .await
            .map_err(|status| unresolved(&status, "remote initialize remains unresolved"))?;
        // ADR 0033 (lifecycle design §10): what became of the launch's
        // LMCache, Ready or failed, for the coordinator's record before Ready
        // and its fallback, exactly as an embedded launch reports it.
        if let Some(report) = lmcache_report(&result) {
            self.lmcache.record(report);
        }
        launch_evidence(&result)?;
        let receipt = remote_receipt(&result, &devices)?;
        // SPEC §13.2 (G2): this readiness belongs to the host session that
        // proved it; a later session must re-prove it before dispatch.
        self.readiness
            .lock()
            .map_err(|_| RuntimeError::Uncertain("readiness ledger unavailable".into()))?
            .insert(c.binding_id.clone(), session);
        Ok(EffectObservation {
            token: c.token.clone(),
            binding_id: c.binding_id.clone(),
            incarnation: c.incarnation.clone(),
            identities: result
                .processes
                .iter()
                .filter(|p| p.presence == "alive")
                .map(process)
                .collect(),
            observed_at_ms: result.observed_at_unix_ms,
            receipt,
            facts: vec![
                Milestone::AllocationsRestored,
                Milestone::WeightsUsable,
                Milestone::CacheValid,
                Milestone::ModelUsable,
            ],
            kernel_builds: kernel_builds(&result),
        })
    }
    async fn inspect(&self, _: &MemberRef) -> Result<EngineState, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn render_plan(&self, _: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn prepare_park(&self, _: &MemberRef) -> Result<Quiescence, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<ParkOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn restore(&self, _: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn reload_weights(&self, _: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn observe_work(&self, _: &MemberRef) -> Result<WorkObservation, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    /// ADR 0032 §3: abort one request on the launch this binding owns, by
    /// CapyCTL's request ID (`req.id`, SGLang's `rid`). One AbortRequest,
    /// which the host's agent runs on loopback against its retained launch
    /// with the launch's own key, never through ingress or the router. It is
    /// sent only to a host whose session declares `request_abort` (ADR
    /// 0017), for the launch this coordinator session still runs there (its
    /// deployment, generation and revision from the store), with at most
    /// [`crate::agent_sessions::MAX_ABORTS_PER_HOST`] in flight to the host,
    /// and bounded by [`ABORT_BOUND`]. `Uncertain` once the host reports the
    /// engine answered the abort: that answer is never evidence the request
    /// ended. Anything else is an error (not sent, refused by the host, or
    /// unanswered), and the request keeps today's rule.
    async fn cancel_work(
        &self,
        member: &MemberRef,
        req: &RequestRef,
        _: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        let b = &self.binding;
        if member.member_id != b.plan.binding_id
            || !capyctl_protocol::execution::is_request_id(&req.id)
        {
            return Err(AdapterError::UnsupportedCombination);
        }
        if !self.sessions.supports_request_abort(&b.host_id) {
            return Err(AdapterError::UnsupportedCapability);
        }
        let launch = {
            let owner = self
                .owner
                .lock()
                .map_err(|_| AdapterError::Uncertain("ownership unavailable".into()))?;
            owner
                .store()
                .remote_live_launch(owner.session(), &b.plan.binding_id)
                .ok()
                .flatten()
        };
        let Some(launch) = launch
            .filter(|l| l.host_id == b.host_id && l.fence.deployment_id == member.deployment_id)
        else {
            return Err(AdapterError::PolicyDenied);
        };
        // Past the host's share of aborts: not sent.
        let Some(_slot) = self.sessions.abort_slot(&b.host_id) else {
            return Err(AdapterError::PolicyDenied);
        };
        let id = ulid::Ulid::new().to_string();
        let mut command = MemberCommand {
            identity: CommandIdentity {
                controller_id: b.controller_id.clone(),
                member: MemberKey {
                    host_id: b.host_id.clone(),
                    member_id: b.member_id.clone(),
                },
                deployment_id: launch.fence.deployment_id.clone(),
                operation_id: id.clone(),
                command_id: id.clone(),
                step_id: id,
                generation: launch.fence.generation,
                revision: launch.fence.revision,
                deadline_ms: capyctl_protocol::now_unix_ms()
                    .saturating_add(ABORT_BOUND.as_millis() as i64),
                payload_digest: [0; 32],
                expected_state: capyctl_protocol::execution::ABORT_REQUEST_STATE.into(),
                profile_fingerprint: b.profile_fingerprint.clone(),
                instance_index: b.instance_index,
            },
            action: MemberAction::AbortRequest {
                owned_handle: b.launch_command_id.clone(),
                request_id: req.id.clone(),
            },
        };
        command.identity.payload_digest = command.canonical_digest();
        let result = tokio::time::timeout(ABORT_BOUND, self.sessions.execute(command))
            .await
            .map_err(|_| AdapterError::Uncertain("the host did not answer the abort".into()))?
            .map_err(
                |status| match crate::agent_sessions::gate_refusal(&status) {
                    Some(_) => AdapterError::UnsupportedCapability,
                    None => AdapterError::Uncertain("the host did not answer the abort".into()),
                },
            )?;
        abort_outcome(&result)
    }
    async fn engine_quiescent(&self, member: &MemberRef, after_ms: i64) -> bool {
        // SPEC §10 (W12; amended 2026-10-01): the host's load report for this
        // launch, which folds TensorFold's /health in, is the only evidence.
        let b = &self.binding;
        if member.member_id != b.plan.binding_id {
            return false;
        }
        let launch = {
            let Ok(owner) = self.owner.lock() else {
                return false;
            };
            // Not the Ready list: a park or stop accepted after the hang-up
            // drains on this lease while the instance is no longer Ready.
            let Ok(launch) = owner
                .store()
                .remote_live_launch(owner.session(), &b.plan.binding_id)
            else {
                return false;
            };
            launch
        };
        let Some(launch) = launch.filter(|l| l.host_id == b.host_id) else {
            return false;
        };
        let key = crate::load_table::InstanceKey::new(
            launch.fence.deployment_id.clone(),
            launch.fence.generation,
        );
        self.sessions
            .load_table()
            .sample_at(&key, capyctl_protocol::now_unix_ms())
            .is_some_and(|view| {
                crate::load_table::proves_quiescence(
                    &view,
                    &launch.host_id,
                    &launch.step_id,
                    launch.fence.generation,
                    after_ms,
                )
            })
    }
    /// Owner decision 2026-10-09: the wake canary of a remote single launch.
    /// One completion probe (ADR 0028 §9) of the launch this binding owns,
    /// which the host's agent runs on loopback against its retained launch
    /// with the launch's own key, never through ingress or the router. A host
    /// whose session does not declare completion probes (`engine_groups`) is
    /// sent nothing (`Unsupported`): it keeps the wake checks it had. ADR 0028
    /// note of 2026-10-16: a wake commits its evidence as observed with the
    /// canary, so for a retained launch the answer counts only with the host
    /// reporting, at the time it observed it, the claim retained and the
    /// launch's engine alive (every recorded engine process and no other; a
    /// helper may be gone, ADR 0027), as a restore's result must.
    ///
    /// ADR 0030 §4: a decisions launch's canary is the fixed decision probe,
    /// sent only to a host whose session declares `decisions` (every host
    /// that took the launch did); its answer is judged well-formed again
    /// here. It never answers `Unsupported`, which would pass a wake
    /// unchecked: a host without the capability is a canary not answered.
    async fn wake_canary(
        &self,
        c: &capyctl_domain::completion::StepExecutionContext,
        max_tokens: u32,
        bound: std::time::Duration,
    ) -> Result<capyctl_adapters::completion_probe::CanaryAnswer, RuntimeError> {
        use capyctl_adapters::completion_probe::CanaryAnswer;
        let b = &self.binding;
        if c.binding_id != b.plan.binding_id || c.incarnation != b.plan.incarnation {
            return Err(RuntimeError::StaleRevision);
        }
        if b.decisions {
            if !self.sessions.supports(&b.host_id, capabilities::DECISIONS) {
                return Err(RuntimeError::Uncertain(
                    "the host no longer declares decisions; the decision probe was not sent".into(),
                ));
            }
        } else if !self
            .sessions
            .supports(&b.host_id, capabilities::ENGINE_GROUPS)
        {
            return Err(RuntimeError::Unsupported);
        }
        let id = ulid::Ulid::new().to_string();
        let mut command = MemberCommand {
            identity: CommandIdentity {
                controller_id: b.controller_id.clone(),
                member: MemberKey {
                    host_id: b.host_id.clone(),
                    member_id: b.member_id.clone(),
                },
                deployment_id: c.token.deployment_id.clone(),
                operation_id: c.token.operation_id.clone(),
                command_id: id.clone(),
                step_id: id,
                generation: c.token.generation,
                revision: c.token.revision,
                deadline_ms: capyctl_protocol::now_unix_ms()
                    .saturating_add(i64::try_from(bound.as_millis()).unwrap_or(i64::MAX)),
                payload_digest: [0; 32],
                expected_state: "ready".into(),
                profile_fingerprint: b.profile_fingerprint.clone(),
                instance_index: b.instance_index,
            },
            action: MemberAction::Probe {
                owned_handle: b.launch_command_id.clone(),
                max_tokens: (!b.decisions).then_some(max_tokens),
                decision: b.decisions,
            },
        };
        command.identity.payload_digest = command.canonical_digest();
        let result = tokio::time::timeout(bound, self.sessions.execute(command))
            .await
            .map_err(|_| {
                RuntimeError::Uncertain("the wake canary was not answered in time".into())
            })?
            .map_err(|_| {
                RuntimeError::Uncertain("the host did not answer the wake canary".into())
            })?;
        let answer = canary_evidence(b.decisions, &result)?;
        canary_engine_alive(&c.identities, &result)?;
        Ok(CanaryAnswer {
            answer,
            observed_at_ms: result.observed_at_unix_ms,
        })
    }
}

/// ADR 0028 note of 2026-10-16: a host's answer to the wake canary of a
/// retained launch counts only with the claim retained and the engine
/// `identities` names alive (every recorded engine process and no other; a
/// helper may be gone, ADR 0027). The coordinator names the processes the
/// restore's evidence named, so a restart-park's wake (ADR 0033 §7) is
/// judged on its new engine.
pub(crate) fn canary_engine_alive(
    identities: &capyctl_domain::completion::ExecutionIdentities,
    result: &pb::MemberExecutionResult,
) -> Result<(), RuntimeError> {
    let capyctl_domain::completion::ExecutionIdentities::Retained(recorded) = identities else {
        return Ok(());
    };
    let alive: Vec<_> = result
        .processes
        .iter()
        .filter(|p| p.presence == "alive")
        .map(process)
        .collect();
    if !result.claim_retained || !capyctl_domain::completion::same_engine(recorded, &alive) {
        return Err(RuntimeError::Uncertain(
            "the host did not report the launch's engine alive with the wake canary".into(),
        ));
    }
    Ok(())
}

/// ADR 0032 §3: how long the server waits for a host's answer to an abort,
/// and the AbortRequest's deadline. The host's post is bounded at 3 s.
pub const ABORT_BOUND: std::time::Duration = std::time::Duration::from_secs(4);

/// ADR 0032 §3: what a host's answer to an AbortRequest (shape already
/// checked by `validate_result`) means: posted and answered by the engine
/// (`Uncertain`, never evidence the request ended), refused with nothing
/// posted, or posted without an answer.
fn abort_outcome(result: &pb::MemberExecutionResult) -> Result<CancellationOutcome, AdapterError> {
    match (result.state.as_str(), result.refused.as_str()) {
        ("completed", "") => Ok(CancellationOutcome::Uncertain),
        ("completed", "request_abort_unsupported") => Err(AdapterError::UnsupportedCapability),
        ("completed", _) => Err(AdapterError::PolicyDenied),
        _ => Err(AdapterError::Uncertain(
            "the engine did not answer the abort".into(),
        )),
    }
}

/// What a host's answer to a wake canary probe proves: generated tokens or
/// text for a chat launch (owner decision 2026-10-09), and for a decisions
/// launch (ADR 0030 §4) only its decision answer, of this build's probe
/// version and judged well-formed again here, never generated text.
fn canary_evidence(
    decisions: bool,
    result: &pb::MemberExecutionResult,
) -> Result<capyctl_adapters::completion_probe::CanaryEvidence, RuntimeError> {
    use capyctl_adapters::completion_probe::{CanaryEvidence, ProbeAnswer};
    use capyctl_adapters::decision_probe;
    if result.state != "completed" {
        return Err(RuntimeError::Uncertain(
            "the host did not complete the wake canary".into(),
        ));
    }
    if !decisions {
        if result.probe_tokens.is_empty() && result.probe_text.is_empty() {
            return Err(RuntimeError::Uncertain(
                "the engine generated nothing for the wake canary".into(),
            ));
        }
        return Ok(CanaryEvidence::Generated(ProbeAnswer {
            tokens: result.probe_tokens.clone(),
            text: result.probe_text.clone(),
        }));
    }
    let evidence = result.probe_decision.as_ref().ok_or_else(|| {
        RuntimeError::Uncertain(
            "the engine did not answer the decision probe for the wake canary".into(),
        )
    })?;
    let probabilities: Vec<(String, f64)> = evidence
        .probabilities
        .iter()
        .map(|p| (p.option.clone(), p.probability))
        .collect();
    (evidence.probe_version == decision_probe::VERSION)
        .then(|| decision_probe::well_formed(&evidence.choice, &probabilities).ok())
        .flatten()
        .map(CanaryEvidence::Decision)
        .ok_or_else(|| {
            RuntimeError::Uncertain("the host relayed a malformed decision probe answer".into())
        })
}
async fn cleanup(
    sessions: &AgentSessions,
    binding: &RemoteLaunchBinding,
    context: CleanupExecutionContext,
) -> Result<CleanupEvidence, CoordinatorError> {
    let mut command = MemberCommand {
        identity: CommandIdentity {
            controller_id: binding.controller_id.clone(),
            member: MemberKey {
                host_id: binding.host_id.clone(),
                member_id: binding.member_id.clone(),
            },
            deployment_id: context.fence.deployment_id.clone(),
            operation_id: context.operation_id.clone(),
            command_id: context.step_id.clone(),
            step_id: context.step_id.clone(),
            generation: context.fence.generation,
            revision: context.fence.revision,
            deadline_ms: context.deadline_ms,
            payload_digest: [0; 32],
            expected_state: "retained".into(),
            profile_fingerprint: binding.profile_fingerprint.clone(),
            instance_index: binding.instance_index,
        },
        action: MemberAction::Terminate {
            owned_handle: binding.launch_command_id.clone(),
            // ADR 0016: what this server recorded, so a host that lost its
            // journal can still report the launch's processes by identity.
            // ADR 0017: only to a host that declared the field.
            recorded: recorded_for(sessions, binding, &context.identities),
        },
    };
    command.identity.payload_digest = command.canonical_digest();
    let result = sessions
        .execute(command)
        .await
        .map_err(|_| CoordinatorError::Service("remote cleanup remains unresolved".into()))?;
    let receipt = proven_gone(binding, &context.identities, &result)?;
    Ok(CleanupEvidence {
        binding_id: context.binding_id,
        incarnation: context.incarnation,
        identities: context.identities,
        observed_at_ms: result.observed_at_unix_ms,
        receipt: receipt.into(),
    })
}

/// ADR 0016, ADR 0017: the recorded identities a Terminate carries. A host
/// that did not declare `terminate_recorded_processes` would refuse a command
/// carrying them (by digest), so it is sent the baseline Terminate and acts on
/// its own journal record, as before the field existed. The proof required of
/// its answer is unchanged: every recorded identity must be reported gone.
fn recorded_for(
    sessions: &AgentSessions,
    binding: &RemoteLaunchBinding,
    identities: &[ProcessIdentity],
) -> Vec<ProcessIdentity> {
    if sessions.supports(&binding.host_id, capabilities::TERMINATE_RECORDED_PROCESSES) {
        identities.to_vec()
    } else {
        Vec::new()
    }
}

/// ADR 0017: a command refused before it was sent (drain-only host, missing
/// capability) is a typed refusal without effect; anything else unresolved
/// stays uncertain.
fn unresolved(status: &tonic::Status, uncertain: &str) -> RuntimeError {
    match crate::agent_sessions::gate_refusal(status) {
        Some(reason) => RuntimeError::Refused(reason.to_owned()),
        None => RuntimeError::Uncertain(uncertain.into()),
    }
}

/// What a LaunchSingle result proves (SPEC §§6.1, 6.4, 13, 13.2).
///
/// - Refused at acceptance (the host changed after provisioning): nothing was
///   journaled, started or claimed.
/// - Launched, not usable, every recorded process gone: the engine exited
///   before readiness. That is a launch failure with the host's bounded reason
///   (the exit status and refused option names, never values), not ownership
///   uncertainty. Nothing is released here: the settlement still releases the
///   launch only on the host's verified gone evidence.
/// - Anything short of a usable model with the claim retained is unproven.
fn launch_evidence(result: &pb::MemberExecutionResult) -> Result<(), RuntimeError> {
    if !result.refused.is_empty() {
        return Err(RuntimeError::Refused(result.refused.clone()));
    }
    if result.model_usable && result.claim_retained {
        return Ok(());
    }
    if crate::agent_sessions::launch_ended_before_readiness(result) {
        let reason = if capyctl_protocol::execution::is_launch_failure_text(&result.launch_failure)
        {
            result.launch_failure.clone()
        } else {
            "the engine exited before readiness".into()
        };
        return Err(RuntimeError::LaunchFailed(reason));
    }
    Err(RuntimeError::Uncertain(
        "remote model readiness is unproven".into(),
    ))
}

/// ADR 0033 (lifecycle design §10): a LaunchSingle result's LMCache report
/// as the coordinator reads it for an embedded launch. `On` names the
/// server's group among the result's alive processes. The result was
/// validated, so a report that does not convert was never there.
pub(crate) fn lmcache_report(
    result: &pb::MemberExecutionResult,
) -> Option<capyctl_adapters::lmcache::LmcacheReport> {
    use capyctl_adapters::lmcache::LmcacheReport;
    use capyctl_protocol::execution::{LaunchLmcache, LaunchLmcacheState};
    let report = LaunchLmcache::try_from(result.lmcache.as_ref()?).ok()?;
    Some(match report.state {
        LaunchLmcacheState::On => LmcacheReport::On {
            server: capyctl_domain::completion::lmcache_members(
                &result
                    .processes
                    .iter()
                    .filter(|p| p.presence == "alive")
                    .map(process)
                    .collect::<Vec<_>>(),
            ),
            rpc_port: report.rpc_port,
            http_port: report.http_port,
            pool_bytes: report.pool_bytes,
        },
        LaunchLmcacheState::Off => LmcacheReport::Off {
            reason: report.reason?,
            detail: report.detail,
            connector_failed: report.connector_failed,
        },
    })
}

/// ADR 0014 amendment A12: the kernel builds a usable launch's host saw, in
/// the host's clock (the clock its memory samples carry). The result was
/// validated, so every span is ordered and there are at most 64.
fn kernel_builds(
    result: &pb::MemberExecutionResult,
) -> Vec<capyctl_domain::completion::KernelBuild> {
    result
        .kernel_builds
        .iter()
        .map(|span| capyctl_domain::completion::KernelBuild {
            from_ms: span.from_unix_ms,
            until_ms: span.until_unix_ms,
        })
        .collect()
}

/// SPEC §§6.1, 13.2: the host's authenticated Terminate result proves the owned
/// launch gone only when its claim is released, every process it reports is
/// gone, and every identity the controller recorded is among them. A host that
/// reports no process at all proves the launch never released one, which can
/// only settle a launch for which the controller recorded nothing either.
fn proven_gone(
    binding: &RemoteLaunchBinding,
    recorded: &[ProcessIdentity],
    result: &pb::MemberExecutionResult,
) -> Result<&'static str, CoordinatorError> {
    if result.state != "completed"
        || result.claim_retained
        || result.owned_handle != binding.launch_command_id
        || result.processes.iter().any(|p| p.presence != "gone")
        || recorded.iter().any(|expected| {
            !result
                .processes
                .iter()
                .any(|actual| process(actual) == *expected)
        })
    {
        return Err(CoordinatorError::Service(
            "authenticated remote process absence is incomplete".into(),
        ));
    }
    Ok(if result.processes.is_empty() {
        "authenticated host reports the launch never released a process"
    } else {
        "authenticated host owned process group observed gone"
    })
}

/// SPEC §§6, 13.2 (G1): settle a launch that failed or went uncertain after arm.
/// The host terminates what it owns for exactly this launch and reports it gone;
/// an unreachable or unproven host leaves the launch uncertain and retained.
async fn settle(
    sessions: &AgentSessions,
    binding: &RemoteLaunchBinding,
    context: SettlementContext,
) -> Result<CleanupEvidence, CoordinatorError> {
    if context.step_id != binding.launch_command_id
        || context.binding_id != binding.plan.binding_id
        || context.incarnation != binding.plan.incarnation
    {
        return Err(CoordinatorError::Service(
            "settlement does not name this remote launch".into(),
        ));
    }
    // A fresh command per attempt: a retry after reconnect replays the same
    // command until its deadline, and a later attempt is a new request.
    let id = ulid::Ulid::new().to_string();
    let mut command = MemberCommand {
        identity: CommandIdentity {
            controller_id: binding.controller_id.clone(),
            member: MemberKey {
                host_id: binding.host_id.clone(),
                member_id: binding.member_id.clone(),
            },
            deployment_id: context.fence.deployment_id.clone(),
            operation_id: context.operation_id.clone(),
            command_id: id.clone(),
            step_id: id,
            generation: context.fence.generation,
            revision: context.fence.revision,
            deadline_ms: context.deadline_ms,
            payload_digest: [0; 32],
            expected_state: "retained".into(),
            profile_fingerprint: binding.profile_fingerprint.clone(),
            instance_index: binding.instance_index,
        },
        action: MemberAction::Terminate {
            owned_handle: binding.launch_command_id.clone(),
            // ADR 0016: what this server recorded, so a host that lost its
            // journal can still report the launch's processes by identity.
            // ADR 0017: only to a host that declared the field.
            recorded: recorded_for(sessions, binding, &context.identities),
        },
    };
    command.identity.payload_digest = command.canonical_digest();
    let result = sessions
        .execute(command)
        .await
        .map_err(|_| CoordinatorError::Service("remote settlement remains unresolved".into()))?;
    // Processes the host journaled but the controller never learned of are
    // covered too: every one the host reports must be gone.
    let receipt = proven_gone(binding, &context.identities, &result)?;
    Ok(CleanupEvidence {
        binding_id: context.binding_id,
        incarnation: context.incarnation,
        identities: context.identities,
        observed_at_ms: result.observed_at_unix_ms,
        receipt: receipt.into(),
    })
}

/// Production resolution uses the same Store, current enrolled publication and
/// immutable deployment revision as ordinary admission; no second coordinator.
pub struct RemoteProfileBindings {
    owner: SharedCoordinatorState,
    sessions: Arc<AgentSessions>,
    controller_id: String,
    readiness: ReadinessLedger,
}
impl RemoteProfileBindings {
    pub fn new(
        owner: SharedCoordinatorState,
        sessions: Arc<AgentSessions>,
        controller_id: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            owner,
            sessions,
            controller_id,
            readiness: ReadinessLedger::default(),
        })
    }
    /// The ledger the readiness supervisor shares with every launch.
    pub fn readiness(&self) -> ReadinessLedger {
        self.readiness.clone()
    }
}
impl crate::coordinator::ExecutionBindings for RemoteProfileBindings {
    fn resolve(
        &self,
        work: &capyctl_store::ordinary_lifecycle::worker::InitializeWork,
    ) -> Result<ExecutionBinding, CoordinatorError> {
        let launch = remote_launch(&self.owner, &self.controller_id, work)?;
        Ok(binding(
            self.sessions.clone(),
            self.owner.clone(),
            launch,
            self.readiness.clone(),
        ))
    }
    fn groups(&self) -> Option<Arc<dyn crate::group_activation::GroupHosts>> {
        Some(Arc::new(AgentGroupHosts::new(
            self.owner.clone(),
            self.sessions.clone(),
            self.controller_id.clone(),
            self.readiness.clone(),
        )))
    }
}

/// The launch a remote instance's initialize work sends its host: the plan
/// built from the store's frozen revision and the host's current approved
/// publication, and what the command needs of the host's session.
fn remote_launch(
    owner: &SharedCoordinatorState,
    controller_id: &str,
    work: &capyctl_store::ordinary_lifecycle::worker::InitializeWork,
) -> Result<RemoteLaunchBinding, CoordinatorError> {
    let denied = || CoordinatorError::Service("remote frozen binding unavailable".into());
    let owner = owner.lock().map_err(|_| denied())?;
    let host = &work.effective().host.name;
    let publication = owner
        .store()
        .host_publication(host)
        .map_err(|_| denied())?
        .ok_or_else(denied)?;
    if publication.host_id != *host {
        return Err(denied());
    }
    let host_config = capyctl_config::remote_roles::HostConfig::parse(&publication.config_json)
        .map_err(|_| denied())?;
    let ingress = host_config.ingress.as_ref().ok_or_else(denied)?;
    // ADR 0013 §3: the document as scoped to the host this instance was
    // placed on and (ADR 0019) to the GPU placement chose there, which the
    // frozen launch names as its one selected device. ADR 0035 §2: on
    // several GPUs, to the set it chose, keyed by its GPUs in rank order,
    // so the host resolves the launch on all N of them.
    let set: Vec<String> = work
        .effective()
        .selected_devices
        .iter()
        .map(|claim| claim.id.clone())
        .collect();
    let key = (!set.is_empty()).then(|| capyctl_config::instances::set_key(&set));
    let device = key.as_deref();
    let source = owner
        .store()
        .launch_configuration_source(
            &work.fence().deployment_id,
            work.fence().revision,
            host,
            device,
        )
        .map_err(|_| denied())?
        .ok_or_else(denied)?;
    let scoped = source;
    let local = capyctl_config::remote_resources::local_deployment_document(host, &scoped)
        .map_err(|_| denied())?;
    let profile_name = local["runtime_profile"]
        .as_str()
        .ok_or_else(denied)?
        .to_owned();
    let key = match owner
        .store()
        .engine_key(
            work.binding_id(),
            work.incarnation(),
            capyctl_store::secrets::SecretRole::Inference,
        )
        .map_err(|_| denied())?
    {
        Some(key) => key,
        None => {
            let key = capyctl_store::secrets::new_engine_key();
            owner
                .store()
                .store_engine_key(
                    work.binding_id(),
                    work.incarnation(),
                    &key,
                    capyctl_store::secrets::SecretRole::Inference,
                )
                .map_err(|_| denied())?;
            key
        }
    };
    owner
        .store()
        .bind_remote_ingress(work.binding_id(), host, &ingress.address)
        .map_err(|_| denied())?;
    // ADR 0014 §7 (WE3): the recorded digest, or empty when this is the
    // revision's first placement (measured before the launch is sent).
    let checkpoint_digest = owner
        .store()
        .recorded_checkpoint(&work.fence().deployment_id, work.fence().revision)
        .map_err(|_| denied())?
        .unwrap_or_default();
    let reservation = work.startup_reservation();
    let measured = capyctl_config::effective::StartupProvenance::Measured;
    let (startup_bytes, startup_host_bytes) = match &reservation.split {
        Some(split) if split.device_provenance == measured || split.host_provenance == measured => {
            (split.device_bytes, Some(split.host_bytes))
        }
        _ => (reservation.bytes, None),
    };
    let plan = SingleLaunchPlan {
        // ADR 0014 §5: the host resolves with the facts this revision was
        // frozen with, so both sides derive the same memory request: the
        // whole checkpoint's weights, which the host verifies.
        checkpoint_weights_bytes: work
            .effective()
            .engine_config
            .memory()
            .checkpoint_weights_bytes(),
        // ADR 0014 amendment A16: and the hybrid state slot beside them.
        checkpoint_state_slot_bytes: work.effective().engine_config.memory().state_slot_bytes,
        // ADR 0028 §5: a single-host launch holds the whole checkpoint.
        // ADR 0035 §3: on N GPUs of one host each rank's share is taken
        // with the checkpoint's layout, as a group member's is, so the
        // host derives the same per-GPU charge.
        checkpoint_layout: (work.effective().local_tensor_parallel() > 1)
            .then(|| {
                work.effective()
                    .engine_config
                    .memory()
                    .member
                    .and_then(|member| member.layout)
            })
            .flatten(),
        // ADR 0014 amendment A20: the tables an engine that keeps them on
        // disk was sized without.
        checkpoint_tables: work
            .effective()
            .engine_config
            .memory()
            .disk_tables
            .map(|recorded| recorded.tables),
        // ADR 0029 §9: the GGUF facts a llama.cpp revision was sized with.
        checkpoint_gguf: work
            .effective()
            .engine_config
            .memory()
            .gguf
            .map(|recorded| recorded.facts),
        // Owner decision 2026-09-23: the peak this launch reserved. ADR
        // 0019 §3: on a discrete GPU measured per domain, the card's and
        // host RAM's apart, so the host charges each where it belongs.
        startup_bytes: Some(startup_bytes),
        startup_host_bytes,
        // ADR 0033 §§3, 6 (lifecycle design §10): the store's LMCache
        // grant, its ports and the charge it reserved, which the host lays
        // over the same footprint. Absent when the store planned it off.
        lmcache: work.lmcache().and_then(|plan| plan.grant()).map(|grant| {
            capyctl_protocol::execution::LmcacheGrant {
                rpc_port: grant.rpc_port,
                http_port: grant.http_port,
                pool_bytes: grant.pool_bytes,
                overhead_bytes: grant.overhead_bytes,
                device_bytes: grant.device_bytes,
                held_kv_bytes: grant.held_kv_bytes,
            }
        }),
        checkpoint_digest,
        deployment_config: local.to_string(),
        profile_name,
        checkpoint_fingerprint: work.effective().model.content_fingerprint.clone(),
        host_policy_fingerprint: publication.fingerprint,
        binding_id: work.binding_id().into(),
        incarnation: work.incarnation().into(),
        // The real armed grant and issue time replace these before digesting.
        grant_id: work.step_id().into(),
        issued_at_ms: 0,
        coordinator_session_id: owner.session().id().into(),
        // ADR 0035 §§2, 11: a launch on several GPUs of this host names
        // them in rank order, by the host's own ids; on one GPU none, so
        // the command encodes exactly as before.
        devices: local_devices(host, work.effective()),
        service_port: work
            .endpoint()
            .rsplit(':')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(denied)?,
    };
    // ADR 0013 §5: a host that fences per instance is told which instance
    // this launch is; any other host fences per deployment and is sent
    // instance 0, exactly the command it accepted before instances.
    let instance_index = if owner
        .store()
        .host_has_per_instance_fencing(host)
        .map_err(|_| denied())?
    {
        work.instance_index()
    } else {
        0
    };
    drop(owner);
    Ok(RemoteLaunchBinding {
        controller_id: controller_id.into(),
        host_id: host.clone(),
        member_id: "head".into(),
        profile_fingerprint: work.effective().profile.build_fingerprint.clone(),
        launch_command_id: work.step_id().into(),
        plan,
        ingress_gate_key: key,
        instance_index,
        // ADR 0019: the Ready footprint is charged to a device domain.
        device_memory: work.effective().ready_device_allocation().is_some(),
        // ADR 0030 §1: a decisions launch goes only to a host that
        // declared it.
        decisions: !work.effective().api.is_chat(),
        // ADR 0031 §9: a custom engine's launch goes only to a host
        // that declared it.
        custom_engine: work.effective().profile.engine
            == capyctl_config::engine_policy::Engine::Custom,
        // ADR 0033 §13: `lmcache: required` and `retention: zero`
        // go only to a host that declared what honours them.
        lmcache_required: work.effective().lmcache
            == capyctl_config::lmcache::LmcacheMode::Required,
        retention_zero: work.effective().retention == capyctl_config::lmcache::Retention::Zero,
        // ADR 0039 §15: a native TensorFold launch goes only to a host that
        // declared it.
        tensorfold_native: work.effective().profile.engine
            == capyctl_config::engine_policy::Engine::Tensorfold
            && work.effective().profile.line.is_native(),
    })
}

/// ADR 0028 §8: the member hosts of a group through their authenticated
/// sessions, with the head's readiness in the same ledger every remote launch
/// shares (SPEC §13.2, G2).
pub struct AgentGroupHosts {
    owner: SharedCoordinatorState,
    sessions: Arc<AgentSessions>,
    controller_id: String,
    readiness: ReadinessLedger,
}

impl AgentGroupHosts {
    pub fn new(
        owner: SharedCoordinatorState,
        sessions: Arc<AgentSessions>,
        controller_id: String,
        readiness: ReadinessLedger,
    ) -> Self {
        Self {
            owner,
            sessions,
            controller_id,
            readiness,
        }
    }
}

impl crate::group_activation::GroupHosts for AgentGroupHosts {
    fn controller_id(&self) -> String {
        self.controller_id.clone()
    }

    fn host(&self, host_id: &str) -> Result<crate::group_activation::MemberHost, String> {
        let owner = self
            .owner
            .lock()
            .map_err(|_| "ownership unavailable".to_owned())?;
        let publication = owner
            .store()
            .host_publication(host_id)
            .map_err(|e| e.to_string())?
            .filter(|p| p.host_id == host_id)
            .ok_or_else(|| format!("host {host_id} has no current publication"))?;
        let config = capyctl_config::remote_roles::HostConfig::parse(&publication.config_json)
            .map_err(|e| e.to_string())?;
        // ADR 0028 §3: read from the publication, never from the normalized
        // policy (its digest leaves the groups block out).
        let groups = capyctl_config::groups_policy::host_groups_policy(&config.document)
            .map_err(|e| e.to_string())?;
        Ok(crate::group_activation::MemberHost {
            policy_fingerprint: publication.fingerprint,
            groups,
        })
    }

    fn preflight(&self, host_id: &str, needs: &[&str]) -> Result<(), String> {
        self.sessions.preflight(host_id, needs, true)
    }

    fn supports(&self, host_id: &str, capability: &str) -> bool {
        self.sessions.supports(host_id, capability)
    }

    fn execute(
        &self,
        command: MemberCommand,
    ) -> crate::group_activation::HostFuture<'_, Result<pb::MemberExecutionResult, String>> {
        Box::pin(async move {
            self.sessions
                .execute(command)
                .await
                .map_err(|status| status.message().to_owned())
        })
    }

    fn provision_head<'a>(
        &'a self,
        command: &'a MemberCommand,
    ) -> crate::group_activation::HostFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let MemberAction::Launch { member, .. } = &command.action else {
                return Err("not a group launch".into());
            };
            let host = &command.identity.member.host_id;
            let key = {
                let owner = self
                    .owner
                    .lock()
                    .map_err(|_| "ownership unavailable".to_owned())?;
                let publication = owner
                    .store()
                    .host_publication(host)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("host {host} has no current publication"))?;
                let config =
                    capyctl_config::remote_roles::HostConfig::parse(&publication.config_json)
                        .map_err(|e| e.to_string())?;
                let ingress = config
                    .ingress
                    .as_ref()
                    .ok_or_else(|| format!("host {host} publishes no ingress"))?;
                // ADR 0012: the head's launch has its own inference key,
                // sealed beside the binding before the host is provisioned.
                let role = capyctl_store::secrets::SecretRole::Inference;
                let key = match owner
                    .store()
                    .engine_key(&member.binding_id, &member.incarnation, role)
                    .map_err(|e| e.to_string())?
                {
                    Some(key) => key,
                    None => {
                        let key = capyctl_store::secrets::new_engine_key();
                        owner
                            .store()
                            .store_engine_key(&member.binding_id, &member.incarnation, &key, role)
                            .map_err(|e| e.to_string())?;
                        key
                    }
                };
                // SPEC §§6, 15: the group's only replica is the head's ingress.
                owner
                    .store()
                    .bind_remote_ingress(&member.binding_id, host, &ingress.address)
                    .map_err(|e| e.to_string())?;
                key
            };
            match self.sessions.provision_ingress(command, key).await {
                Ok(ProvisionOutcome::Provisioned) => Ok(()),
                Ok(ProvisionOutcome::Refused(reason)) => Err(reason),
                Err(status) => Err(status.message().to_owned()),
            }
        })
    }

    fn head_ready(&self, binding_id: &str, host_id: &str) {
        if let (Ok(mut readiness), Some(session)) = (
            self.readiness.lock(),
            self.sessions.current_session(host_id),
        ) {
            readiness.insert(binding_id.to_owned(), session);
        }
    }

    /// ADR 0028 §12 (R12): the saver map `member`'s own host last reported on
    /// its session for the member's launch, read there from its own
    /// observation directory with the member's own credential. A host with no
    /// live session, or whose report names no whole map for the member, has
    /// reported nothing: the member stays silent. Nothing falls back to
    /// process sampling.
    fn saver_mapped<'a>(
        &'a self,
        _deployment_id: &'a str,
        member: &'a capyctl_store::ordinary_lifecycle::park::ArmedMember,
    ) -> crate::group_activation::HostFuture<'a, Result<(i64, i64), String>> {
        Box::pin(async move {
            let handle = member
                .launch_handle
                .as_deref()
                .ok_or_else(|| format!("rank {} recorded no launch", member.rank))?;
            self.sessions
                .member_saver(&member.host_id, handle)
                .ok_or_else(|| {
                    format!(
                        "host {} reports no saver map for rank {}",
                        member.host_id, member.rank
                    )
                })
        })
    }
}

/// ADR 0035 §§2, 11: the host-local ids (`gpuN`) of the GPUs a launch on
/// several GPUs of `host` runs on, in rank order (rank r on the r-th claim of
/// the frozen revision); empty for a launch on one GPU. Its size is the
/// launch's local tensor-parallel size N.
pub(crate) fn local_devices(
    host: &str,
    effective: &capyctl_config::effective::EffectiveDeployment,
) -> Vec<String> {
    if effective.local_tensor_parallel() < 2 {
        return Vec::new();
    }
    let scoped = capyctl_config::remote_resources::ledger_key(host, "device", "");
    effective
        .selected_devices
        .iter()
        .map(|claim| {
            claim
                .id
                .strip_prefix(&scoped)
                .unwrap_or(&claim.id)
                .to_owned()
        })
        .collect()
}

/// The readiness receipt of a remote launch. ADR 0035 §6: the host's own
/// observation names the rank processes of a launch on several GPUs (each
/// alive process's `rank`, its GPU the plan's `devices[rank]`); the receipt
/// records them, as an embedded launch's does, so a rank's exit is named by
/// its rank and GPU. A host that names ranks names one per GPU, or the
/// readiness proves nothing. llama.cpp runs one process on every GPU and
/// names none.
fn remote_receipt(
    result: &pb::MemberExecutionResult,
    devices: &[String],
) -> Result<String, RuntimeError> {
    use capyctl_domain::local_tp::{ranks_receipt, RankRecord};
    const PROBED: &str = "authenticated host native model probe";
    let mut ranks: Vec<RankRecord> = result
        .processes
        .iter()
        .filter(|p| p.presence == "alive")
        .filter_map(|p| {
            let rank = p.rank?;
            Some(RankRecord {
                rank,
                device: devices.get(usize::try_from(rank).ok()?)?.clone(),
                process: process(p),
            })
        })
        .collect();
    if ranks.is_empty() && result.processes.iter().all(|p| p.rank.is_none()) {
        return Ok(PROBED.into());
    }
    ranks.sort_by_key(|r| r.rank);
    let covers = devices.len() >= 2
        && ranks.len() == devices.len()
        && ranks
            .iter()
            .enumerate()
            .all(|(index, r)| usize::try_from(r.rank).ok() == Some(index));
    if !covers {
        return Err(RuntimeError::Uncertain(
            "the host's readiness names rank processes that are not one per GPU of the launch"
                .into(),
        ));
    }
    Ok(format!("{PROBED}; {}", ranks_receipt(&ranks)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> RemoteLaunchBinding {
        RemoteLaunchBinding {
            controller_id: "controller".into(),
            host_id: "host".into(),
            member_id: "head".into(),
            profile_fingerprint: "fingerprint".into(),
            launch_command_id: "launch".into(),
            plan: SingleLaunchPlan {
                deployment_config: "{}".into(),
                profile_name: "local".into(),
                checkpoint_fingerprint: "checkpoint".into(),
                host_policy_fingerprint: "a".repeat(64),
                binding_id: "binding".into(),
                incarnation: "incarnation".into(),
                grant_id: "grant".into(),
                service_port: 30000,
                issued_at_ms: 1,
                coordinator_session_id: "session".into(),
                checkpoint_digest: String::new(),
                checkpoint_weights_bytes: None,
                checkpoint_state_slot_bytes: None,
                checkpoint_layout: None,
                checkpoint_tables: None,
                checkpoint_gguf: None,
                startup_bytes: None,
                startup_host_bytes: None,
                lmcache: None,
                devices: Vec::new(),
            },
            ingress_gate_key: [7; 32],
            instance_index: 0,
            device_memory: false,
            decisions: false,
            custom_engine: false,
            lmcache_required: false,
            retention_zero: false,
            tensorfold_native: false,
        }
    }

    /// ADR 0035 §§2, 3, 11: an enrolled host with two identical GPUs that
    /// declared every capability, and the initialize work of a deployment
    /// placed there with `topology` (none on one GPU), its 20 GiB of weights
    /// and the checkpoint's `layout` measured. Returns the owned state, the
    /// host's id and the work.
    fn remote_work(
        topology: Option<serde_json::Value>,
        layout: capyctl_domain::member_weights::CheckpointLayout,
    ) -> (
        tempfile::TempDir,
        SharedCoordinatorState,
        String,
        capyctl_store::ordinary_lifecycle::worker::InitializeWork,
    ) {
        use serde_json::json;
        let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(
            dir.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let owned = Arc::new(Mutex::new(
            crate::OwnedCoordinatorState::open(dir.path()).unwrap(),
        ));
        let golden: serde_json::Value = serde_json::from_str(include_str!(
            "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let mut host = golden["input"]["host"].clone();
        host["resource_policy"]["domains"] = json!({
            "system": {"memory": "distinct", "managed_limit": "200GiB", "free_reserve": "8GiB",
                       "parked_limit": "64GiB"},
            "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "40GiB",
                     "free_reserve": "1GiB", "parked_limit": "4GiB"},
            "gpu1": {"memory": "device", "device": "gpu1", "managed_limit": "40GiB",
                     "free_reserve": "1GiB", "parked_limit": "4GiB"}
        });
        host["resource_policy"]["devices"] = json!({
            "gpu0": {"domain": "gpu0", "sharing": "shared", "product_name": "NVIDIA RTX A6000"},
            "gpu1": {"domain": "gpu1", "sharing": "shared", "product_name": "NVIDIA RTX A6000"}
        });
        // The host's published role document: its policy, its state and its
        // ingress.
        let mut published = host.clone();
        published["state_dir"] = json!("/home/operator/.local/state/capyctl");
        published["identity_dir"] = json!("/home/operator/.local/state/capyctl/identity");
        published["ingress"] = json!({
            "transport": "trusted_private_link",
            "address": "http://100.64.0.2:9443",
            "bind": "100.64.0.2:9443"
        });
        let work = {
            let owner = owned.lock().unwrap();
            let (store, session) = (owner.store(), owner.session());
            store
                .create_host_invitation(&"b".repeat(64), "spark", 100, 0)
                .unwrap();
            let host_id = store
                .redeem_host_invitation(
                    &capyctl_store::enrollment::Redemption {
                        invitation_digest: "b".repeat(64),
                        transaction_id: "tx".into(),
                        host_name: "spark".into(),
                        key_digest: "c".repeat(64),
                        csr_digest: "d".repeat(64),
                    },
                    1,
                    |id| {
                        Ok(capyctl_store::enrollment::CertificateRecord {
                            host_id: id.into(),
                            fingerprint: "bf".repeat(32),
                            certificate_pem: "certificate".into(),
                            expires_unix: 500,
                        })
                    },
                )
                .unwrap()
                .host_id;
            store
                .record_host_version(
                    &host_id,
                    &capyctl_store::host_versions::HostVersion {
                        binary_version: capyctl_protocol::version::BINARY_VERSION.into(),
                        compatibility: "supported".into(),
                        reason: String::new(),
                        capabilities: capabilities::agent_capabilities(),
                        recorded_at_ms: 1,
                    },
                )
                .unwrap();
            store
                .publish_host_configuration(&capyctl_store::host_publication::HostPublication {
                    host_id: host_id.clone(),
                    config_json: published.to_string(),
                    boot_id: "boot".into(),
                    fingerprint: capyctl_config::remote_resources::policy_fingerprint(&published),
                    received_at_ms: 1,
                })
                .unwrap();
            let policy = capyctl_config::effective::normalize_host_policy(&host).unwrap();
            let observed: Vec<_> = ["system", "gpu0", "gpu1"]
                .map(|domain| capyctl_domain::resources::MemoryObservation {
                    domain: domain.into(),
                    capacity_bytes: 1 << 40,
                    available_bytes: 1 << 40,
                    sampled_at_ms: 1,
                })
                .to_vec();
            store
                .import_remote_resource_policy(session, &host_id, &policy, &observed, 1)
                .unwrap();
            let trusted =
                capyctl_config::remote_resources::scope_host_document(&host_id, &host).unwrap();
            let current = store.resource_policy(&host_id).unwrap().unwrap();
            let target = capyctl_store::managed_configuration::HostTarget {
                host_id: host_id.clone(),
                host_name: "spark".into(),
                trusted_host: capyctl_config::effective::compose_current_resource_controls(
                    &trusted,
                    &current.context,
                    &current.controls,
                )
                .unwrap(),
                scoped: true,
            };
            let mut config = golden["input"]["deployment"].clone();
            let object = config.as_object_mut().unwrap();
            object.remove("resources");
            object.remove("host");
            config["devices"] = json!([]);
            config["engine_config"] = json!({"memory": {"kv_cache": "4GiB"}});
            config["residency"] = json!("restart_only");
            config["instances"] = json!(1);
            config["placement"] = json!({"hosts": ["spark"]});
            if let Some(topology) = topology {
                config["topology"] = topology;
            }
            let receipt = store
                .create_managed_configuration_on_hosts(
                    session,
                    "owner",
                    "deploy",
                    &json!({ "config": config }).to_string(),
                    std::slice::from_ref(&target),
                    &[],
                    1000,
                )
                .unwrap();
            let id = receipt.deployment_id;
            let revision = store.current_revision(&id).unwrap().unwrap();
            store
                .record_checkpoint_measurement(
                    session,
                    &id,
                    revision,
                    &host_id,
                    &format!("sha256:{}", "a".repeat(64)),
                    20 << 30,
                    None,
                    Some(layout),
                    capyctl_config::effective::DigestProvenance::Measured,
                    None,
                    None,
                    None,
                    1000,
                )
                .unwrap();
            store
                .accept_scoped_start_command(
                    session,
                    "owner",
                    &id,
                    capyctl_store::ordinary_lifecycle::placement::StartScope::All,
                    revision,
                    "start",
                    1000,
                    100_000,
                    None,
                )
                .unwrap();
            store.next_initialize(session).unwrap().unwrap()
        };
        let host_id = work.effective().host.name.clone();
        (dir, owned, host_id, work)
    }

    fn layout() -> capyctl_domain::member_weights::CheckpointLayout {
        capyctl_domain::member_weights::CheckpointLayout {
            sharded_bytes: 18 << 30,
            layer_count: 4,
            largest_layer_bytes: 5 << 30,
        }
    }

    /// The document the server keeps for the revision on `host` and `key`
    /// (a GPU, or a set in rank order), as the host receives it.
    fn local_source(
        owned: &SharedCoordinatorState,
        work_host: &str,
        key: &str,
        id: &str,
    ) -> String {
        let owner = owned.lock().unwrap();
        let revision = owner.store().current_revision(id).unwrap().unwrap();
        let source = owner
            .store()
            .launch_configuration_source(id, revision, work_host, Some(key))
            .unwrap()
            .unwrap();
        capyctl_config::remote_resources::local_deployment_document(work_host, &source)
            .unwrap()
            .to_string()
    }

    // T46 (ADR 0035 §§2, 3, 11): a remote launch on two GPUs is sent the
    // document the server keeps for its set (keyed by both GPUs in rank
    // order), which names both claims; the plan lists the GPUs in rank order
    // by the host's own ids and carries the checkpoint's layout, so the host
    // derives the same per-GPU share.
    #[test]
    fn a_remote_launch_on_two_gpus_sends_its_sets_document_devices_and_layout() {
        let (_dir, owned, host, work) =
            remote_work(Some(serde_json::json!({"tensor_parallel": 2})), layout());
        assert_eq!(work.effective().local_tensor_parallel(), 2);
        let launch = remote_launch(&owned, "controller", &work).unwrap();
        assert_eq!(launch.plan.devices, ["gpu0", "gpu1"]);
        assert_eq!(launch.plan.checkpoint_layout, Some(layout()));
        let set = work
            .effective()
            .selected_devices
            .iter()
            .map(|claim| claim.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            set,
            ["gpu0", "gpu1"]
                .map(|gpu| capyctl_config::remote_resources::ledger_key(&host, "device", gpu))
        );
        let key = capyctl_config::instances::set_key(&set);
        assert_eq!(
            launch.plan.deployment_config,
            local_source(&owned, &host, &key, &work.fence().deployment_id)
        );
        let sent: serde_json::Value = serde_json::from_str(&launch.plan.deployment_config).unwrap();
        assert_eq!(sent["topology"], serde_json::json!({"tensor_parallel": 2}));
        let ids: Vec<&str> = sent["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|claim| claim["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["gpu0", "gpu1"], "all N claims, rank order");
    }

    // T46 (ADR 0035 §§2, 11, ADR 0019): a remote launch on one GPU is sent
    // that GPU's document, as before, with no device list and no layout (an
    // empty repeated field and an absent one encode no bytes), so its
    // command encodes exactly as it did before this ADR.
    #[test]
    fn a_remote_launch_on_one_gpu_is_unchanged() {
        let (_dir, owned, host, work) = remote_work(None, layout());
        assert_eq!(work.effective().local_tensor_parallel(), 1);
        let launch = remote_launch(&owned, "controller", &work).unwrap();
        assert!(launch.plan.devices.is_empty());
        assert_eq!(launch.plan.checkpoint_layout, None);
        let [claim] = work.effective().selected_devices.as_slice() else {
            panic!("one claim");
        };
        assert_eq!(
            launch.plan.deployment_config,
            local_source(&owned, &host, &claim.id, &work.fence().deployment_id)
        );
    }
    fn observed(role: &str, pid: u32, presence: &str) -> pb::OwnedProcessObservation {
        pb::OwnedProcessObservation {
            role: role.into(),
            pid,
            boot_id: "boot".into(),
            start_ticks: 9,
            presence: presence.into(),
            rank: None,
        }
    }
    fn result(processes: Vec<pb::OwnedProcessObservation>) -> pb::MemberExecutionResult {
        pb::MemberExecutionResult {
            state: "completed".into(),
            owned_handle: "launch".into(),
            processes,
            observed_at_unix_ms: 5,
            ..Default::default()
        }
    }

    /// ADR 0030 §4: a decisions launch's wake canary counts only the
    /// decision answer the host relayed, of this probe version and well
    /// formed; generated text never stands in for it, and a chat launch's
    /// canary is unchanged.
    // T43 T34
    #[test]
    fn a_decision_canary_counts_only_a_well_formed_relayed_decision() {
        use capyctl_adapters::completion_probe::CanaryEvidence;
        let decided = |choice: &str, ps: [f64; 3], version: u32| {
            let mut r = result(vec![observed("api", 10, "alive")]);
            r.probe_decision = Some(pb::DecisionProbeEvidence {
                probe_version: version,
                choice: choice.into(),
                probabilities: ["billing", "shipping", "technical"]
                    .iter()
                    .zip(ps)
                    .map(|(option, probability)| pb::DecisionProbability {
                        option: (*option).into(),
                        probability,
                    })
                    .collect(),
            });
            r
        };
        let version = capyctl_adapters::decision_probe::VERSION;
        let good = decided("billing", [0.95, 0.02, 0.03], version);
        assert!(matches!(
            canary_evidence(true, &good),
            Ok(CanaryEvidence::Decision(answer)) if answer.choice == "billing"
        ));
        let mut text_only = result(vec![observed("api", 10, "alive")]);
        text_only.probe_text = "Ready.".into();
        let mut pending = good.clone();
        pending.state = "attempted".into();
        for refused in [
            text_only.clone(),
            pending,
            decided("billing", [0.95, 0.2, 0.03], version),
            decided("other", [0.95, 0.02, 0.03], version),
            decided("billing", [0.95, 0.02, 0.03], version + 1),
        ] {
            assert!(canary_evidence(true, &refused).is_err(), "{refused:?}");
        }
        assert!(matches!(
            canary_evidence(false, &text_only),
            Ok(CanaryEvidence::Generated(_))
        ));
        assert!(canary_evidence(false, &good).is_err());
    }

    /// ADR 0014 amendment A12: the host's kernel builds reach the coordinator
    /// as reported; a result from an older host carries none.
    #[test]
    fn a_launch_result_carries_the_hosts_kernel_builds() {
        let mut ready = result(vec![observed("api", 10, "alive")]);
        assert!(kernel_builds(&ready).is_empty());
        ready.kernel_builds = vec![pb::KernelBuildSpan {
            from_unix_ms: 100,
            until_unix_ms: 250,
        }];
        assert_eq!(
            kernel_builds(&ready),
            [capyctl_domain::completion::KernelBuild {
                from_ms: 100,
                until_ms: 250,
            }]
        );
    }

    /// G1: the host's Terminate result settles a launch only when its claim is
    /// released, everything it reports is gone, and every recorded identity is
    /// among them. An empty report settles only an empty record.
    // T32 T34
    #[test]
    fn host_evidence_must_cover_every_recorded_identity_gone() {
        let b = binding();
        let api = process(&observed("api", 10, "gone"));
        let gone = result(vec![
            observed("api", 10, "gone"),
            observed("worker-0", 11, "gone"),
        ]);
        assert!(proven_gone(&b, std::slice::from_ref(&api), &gone).is_ok());
        assert!(
            proven_gone(&b, &[], &gone).is_ok(),
            "the host covers what it journaled"
        );
        assert!(proven_gone(&b, &[], &result(vec![])).is_ok());
        assert!(proven_gone(&b, std::slice::from_ref(&api), &result(vec![])).is_err());
        let edits: &[fn(&mut pb::MemberExecutionResult)] = &[
            |r| r.state = "attempted".into(),
            |r| r.claim_retained = true,
            |r| r.owned_handle = "other".into(),
            |r| r.processes[1].presence = "alive".into(),
            |r| r.processes[1].presence = "unknown".into(),
            |r| r.processes[0].start_ticks += 1,
        ];
        for edit in edits {
            let mut refused = gone.clone();
            edit(&mut refused);
            assert!(
                proven_gone(&b, std::slice::from_ref(&api), &refused).is_err(),
                "{refused:?}"
            );
        }
    }

    /// W4: a remote park or restore counts only on completed, claim-retaining
    /// evidence whose live group is exactly the recorded one; `parked` never
    /// carries a usable model and `restored` always does. `unchanged` is a
    /// refusal without effect; anything else stays uncertain.
    // T16 T20 T33 T34
    #[test]
    fn remote_residency_evidence_must_name_the_recorded_group() {
        let recorded = vec![
            process(&observed("api", 10, "alive")),
            process(&observed("worker-0", 11, "alive")),
        ];
        let with = |state: &str, usable: bool| {
            let mut r = result(vec![
                observed("worker-0", 11, "alive"),
                observed("api", 10, "alive"),
            ]);
            r.claim_retained = true;
            r.model_usable = usable;
            r.residency = Some(pb::ResidencyEvidence {
                state: state.into(),
                ..Default::default()
            });
            r
        };
        let (_, facts) = residency_evidence(true, &recorded, &with("parked", false)).unwrap();
        assert_eq!(facts, [Milestone::MemoryReleased]);
        let (alive, facts) = residency_evidence(false, &recorded, &with("restored", true)).unwrap();
        assert_eq!(facts.last(), Some(&Milestone::ModelUsable));
        assert_eq!(alive.len(), 2);
        assert_eq!(
            residency_evidence(true, &recorded, &with("unchanged", false)).unwrap_err(),
            RuntimeError::Unsupported
        );
        type Edit = fn(&mut pb::MemberExecutionResult);
        let uncertain: &[(bool, Edit)] = &[
            (true, |r| r.model_usable = true),
            (false, |r| r.model_usable = false),
            (true, |r| r.claim_retained = false),
            (true, |r| r.state = "attempted".into()),
            (true, |r| r.processes[0].start_ticks += 1),
            (true, |r| r.processes[1].presence = "gone".into()),
            (true, |r| {
                r.residency.as_mut().unwrap().state = "restored".into()
            }),
            (false, |r| {
                r.residency.as_mut().unwrap().state = "parked".into()
            }),
            (true, |r| {
                r.residency.as_mut().unwrap().state = "unknown".into()
            }),
            (true, |r| r.residency = None),
        ];
        for (park, edit) in uncertain {
            let mut r = with(if *park { "parked" } else { "restored" }, !park);
            edit(&mut r);
            assert!(
                matches!(
                    residency_evidence(*park, &recorded, &r),
                    Err(RuntimeError::Uncertain(_))
                ),
                "{r:?}"
            );
        }
        assert!(residency_evidence(true, &[], &with("parked", false)).is_err());
    }

    /// ADR 0027: a helper of the recorded group (a compile worker its
    /// scheduler started) that exited does not make a park or wake uncertain;
    /// the engine's own worker exiting still does.
    #[test]
    fn remote_residency_evidence_lets_a_helper_be_gone() {
        let recorded = vec![
            process(&observed("api", 10, "alive")),
            process(&observed("worker-0", 11, "alive")),
            process(&observed("helper-0", 12, "alive")),
        ];
        let parked = |helper: &str, worker: &str| {
            let mut r = result(vec![
                observed("api", 10, "alive"),
                observed("worker-0", 11, worker),
                observed("helper-0", 12, helper),
            ]);
            r.claim_retained = true;
            r.residency = Some(pb::ResidencyEvidence {
                state: "parked".into(),
                ..Default::default()
            });
            r
        };
        let (alive, facts) = residency_evidence(true, &recorded, &parked("gone", "alive")).unwrap();
        assert_eq!(facts, [Milestone::MemoryReleased]);
        assert_eq!(alive, recorded[..2].to_vec());
        assert!(matches!(
            residency_evidence(true, &recorded, &parked("alive", "gone")),
            Err(RuntimeError::Uncertain(_))
        ));
    }
    /// SPEC §§6.1, 6.4, 13.2: a launch whose engine the host reports exited
    /// before readiness (launched, not usable, every process gone) failed with
    /// the host's bounded reason; it is not ownership uncertainty. A launch
    /// still loading, or one that proves nothing, stays uncertain; a refusal
    /// stays a refusal; only a usable, claimed model is Ready.
    // T20 T26 T27
    #[test]
    fn an_engine_that_exited_before_readiness_is_a_launch_failure() {
        let mut exited = result(vec![
            observed("api", 10, "gone"),
            observed("worker-0", 11, "gone"),
        ]);
        exited.state = "launched".into();
        exited.claim_retained = true;
        exited.launch_failure =
            "the engine exited before readiness with exit code 2; it rejected argument --moe-backend".into();
        assert_eq!(
            launch_evidence(&exited).unwrap_err(),
            RuntimeError::LaunchFailed(exited.launch_failure.clone())
        );
        let mut unexplained = exited.clone();
        unexplained.launch_failure = "bad\nline".into();
        assert_eq!(
            launch_evidence(&unexplained).unwrap_err(),
            RuntimeError::LaunchFailed("the engine exited before readiness".into())
        );
        let mut loading = exited.clone();
        loading.processes[1].presence = "alive".into();
        assert!(matches!(
            launch_evidence(&loading),
            Err(RuntimeError::Uncertain(_))
        ));
        let mut refused = result(vec![]);
        refused.refused = "capability_missing:deep_park".into();
        assert_eq!(
            launch_evidence(&refused).unwrap_err(),
            RuntimeError::Refused("capability_missing:deep_park".into())
        );
        let mut ready = result(vec![observed("api", 10, "alive")]);
        ready.claim_retained = true;
        ready.model_usable = true;
        assert!(launch_evidence(&ready).is_ok());
    }

    // T34 T06: ADR 0016, ADR 0017. A Terminate carries the recorded
    // identities only to a host that declared the field, judged on its live
    // session or, offline, on the declaration the store recorded; a host
    // never seen declaring it is sent the baseline Terminate.
    #[test]
    fn recorded_identities_go_only_to_a_host_that_declared_them() {
        let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(
            dir.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let owned = Arc::new(Mutex::new(
            crate::OwnedCoordinatorState::open(dir.path()).unwrap(),
        ));
        let host = {
            let owner = owned.lock().unwrap();
            let store = owner.store();
            store
                .create_host_invitation(&"b".repeat(64), "spark", 100, 0)
                .unwrap();
            store
                .redeem_host_invitation(
                    &capyctl_store::enrollment::Redemption {
                        invitation_digest: "b".repeat(64),
                        transaction_id: "tx".into(),
                        host_name: "spark".into(),
                        key_digest: "c".repeat(64),
                        csr_digest: "d".repeat(64),
                    },
                    1,
                    |id| {
                        Ok(capyctl_store::enrollment::CertificateRecord {
                            host_id: id.into(),
                            fingerprint: "bf".repeat(32),
                            certificate_pem: "certificate".into(),
                            expires_unix: 500,
                        })
                    },
                )
                .unwrap()
                .host_id
        };
        let authority = Arc::new(crate::enrollment::EnrollmentAuthority::new(
            owned.clone(),
            capyctl_agent::identity::CertificateAuthority::generate(100).unwrap(),
        ));
        let sessions = AgentSessions::new(authority.clone());
        let mut remote = binding();
        remote.host_id = host.clone();
        let identities = vec![ProcessIdentity {
            role: "api".into(),
            pid: 10,
            boot_id: "boot".into(),
            start_ticks: 3,
        }];
        assert!(recorded_for(&sessions, &remote, &identities).is_empty());
        let declared = |capabilities: Vec<String>| capyctl_store::host_versions::HostVersion {
            binary_version: capyctl_protocol::version::BINARY_VERSION.into(),
            compatibility: "supported".into(),
            reason: String::new(),
            capabilities,
            recorded_at_ms: 1,
        };
        authority
            .record_host_version(&host, &declared(vec!["heartbeats".into()]))
            .unwrap();
        assert!(recorded_for(&sessions, &remote, &identities).is_empty());
        authority
            .record_host_version(&host, &declared(capabilities::agent_capabilities()))
            .unwrap();
        assert_eq!(recorded_for(&sessions, &remote, &identities), identities);
    }

    /// ADR 0033 (lifecycle design §10): a launch result's LMCache report
    /// reaches the coordinator as an embedded launch reports it. `On` names
    /// the server's group among the result's alive processes (never the
    /// engine's, never one gone) with the host's ports and pool; `Off` its
    /// closed reason, detail and whether the engine failed with the
    /// connector. A report that does not convert (an unknown reason, a
    /// malformed `on`, `connector_failed` beside another reason) is none.
    // T45 T30
    #[test]
    fn a_launch_results_lmcache_report_names_the_alive_server_group() {
        use capyctl_adapters::lmcache::LmcacheReport;
        use capyctl_config::lmcache::{LmcacheOffReason, GIB};
        use capyctl_protocol::execution::LaunchLmcache;
        let gib = GIB as i64;
        let mut ready = result(vec![
            observed("api", 10, "alive"),
            observed("worker-0", 11, "alive"),
            observed("lmcache", 60, "alive"),
            observed("lmcache-1", 61, "alive"),
            observed("lmcache-2", 62, "gone"),
        ]);
        assert_eq!(lmcache_report(&ready), None);
        ready.lmcache = Some(LaunchLmcache::on(8150, 8151, 2 * gib).to_wire());
        assert_eq!(
            lmcache_report(&ready),
            Some(LmcacheReport::On {
                server: vec![
                    process(&observed("lmcache", 60, "alive")),
                    process(&observed("lmcache-1", 61, "alive")),
                ],
                rpc_port: 8150,
                http_port: 8151,
                pool_bytes: 2 * gib,
            })
        );
        let mut failed = result(vec![observed("api", 10, "gone")]);
        failed.lmcache = Some(
            LaunchLmcache::off(
                LmcacheOffReason::EngineStartFailed,
                "RuntimeError: connector handshake failed",
                true,
            )
            .to_wire(),
        );
        assert_eq!(
            lmcache_report(&failed),
            Some(LmcacheReport::Off {
                reason: LmcacheOffReason::EngineStartFailed,
                detail: "RuntimeError: connector handshake failed".into(),
                connector_failed: true,
            })
        );
        let mut off = failed.clone();
        off.lmcache = Some(
            LaunchLmcache::off(LmcacheOffReason::ServerStartFailed, "no /status", false).to_wire(),
        );
        assert_eq!(
            lmcache_report(&off),
            Some(LmcacheReport::Off {
                reason: LmcacheOffReason::ServerStartFailed,
                detail: "no /status".into(),
                connector_failed: false,
            })
        );
        type Edit = fn(&mut pb::LaunchLmcacheReport);
        let malformed: &[(&pb::MemberExecutionResult, Edit)] = &[
            (&off, |r| r.reason = "lmcache_bored".into()),
            (&off, |r| r.connector_failed = true),
            (&off, |r| r.rpc_port = 8150),
            (&off, |r| r.detail = "a\u{1b}[2J".into()),
            (&off, |r| r.state = "maybe".into()),
            (&ready, |r| r.http_port = r.rpc_port),
            (&ready, |r| r.rpc_port = 0),
            (&ready, |r| r.rpc_port = 70_000),
            (&ready, |r| r.pool_bytes = 3 << 29),
            (&ready, |r| r.reason = "host_off".into()),
        ];
        for (base, edit) in malformed {
            let mut r = (*base).clone();
            edit(r.lmcache.as_mut().unwrap());
            assert_eq!(lmcache_report(&r), None, "{:?}", r.lmcache);
        }
    }

    fn residency(
        state: &str,
        engine_stopped: bool,
        usable: bool,
        processes: Vec<pb::OwnedProcessObservation>,
    ) -> pb::MemberExecutionResult {
        let mut r = result(processes);
        r.claim_retained = true;
        r.model_usable = usable;
        r.residency = Some(pb::ResidencyEvidence {
            state: state.into(),
            engine_stopped,
            ..Default::default()
        });
        r
    }

    /// ADR 0033 §7 (lifecycle design §4.1): a `restart_only` park stops the
    /// engine and keeps the LMCache server, so the host's `engine_stopped`
    /// evidence proves it only when what is alive is the recorded server
    /// alone: an engine process still alive or another server stays
    /// uncertain, and so does such evidence for a launch that recorded no
    /// server. A9: the server is its leader, so a member of its group that
    /// exited (a short-lived child, found live on SGLang) does not.
    // T45 T16 T34
    #[test]
    fn a_restart_park_is_proven_by_the_recorded_server_alone() {
        let recorded: Vec<_> = [
            ("api", 10),
            ("worker-0", 11),
            ("lmcache", 60),
            ("lmcache-1", 61),
        ]
        .into_iter()
        .map(|(role, pid)| process(&observed(role, pid, "alive")))
        .collect();
        let parked = |api: &str, server: u32, helper: &str| {
            residency(
                "parked",
                true,
                false,
                vec![
                    observed("api", 10, api),
                    observed("worker-0", 11, "gone"),
                    observed("lmcache", server, "alive"),
                    observed("lmcache-1", 61, helper),
                ],
            )
        };
        let (alive, facts) =
            residency_evidence(true, &recorded, &parked("gone", 60, "alive")).unwrap();
        assert_eq!(alive, recorded[2..].to_vec());
        assert_eq!(facts, [Milestone::MemoryReleased]);
        let (alive, _) = residency_evidence(true, &recorded, &parked("gone", 60, "gone")).unwrap();
        assert_eq!(alive, recorded[2..3].to_vec(), "a server member exited");
        for uncertain in [parked("alive", 60, "alive"), parked("gone", 99, "alive")] {
            assert!(
                matches!(
                    residency_evidence(true, &recorded, &uncertain),
                    Err(RuntimeError::Uncertain(_))
                ),
                "{uncertain:?}"
            );
        }
        // Without `engine_stopped` the park is judged as an ordinary one: the
        // engine stopped is not the engine held.
        let mut ordinary = parked("gone", 60, "alive");
        ordinary.residency.as_mut().unwrap().engine_stopped = false;
        assert!(matches!(
            residency_evidence(true, &recorded, &ordinary),
            Err(RuntimeError::Uncertain(_))
        ));
        // A launch that recorded no server is never restart-parked.
        assert!(matches!(
            residency_evidence(true, &recorded[..2], &parked("gone", 60, "alive")),
            Err(RuntimeError::Uncertain(_))
        ));
    }

    /// ADR 0033 §7 (lifecycle design §4.3): the restore of a restart-parked
    /// launch (which records its server alone) is proven by a new engine
    /// beside that same server group; another server, or the server with no
    /// engine, stays uncertain however usable the host calls it.
    // T45 T10 T34
    #[test]
    fn a_restart_park_restore_is_a_new_engine_beside_the_same_server() {
        let recorded = vec![
            process(&observed("lmcache", 60, "alive")),
            process(&observed("lmcache-1", 61, "alive")),
        ];
        let restored = |server: u32, engine: bool| {
            let mut processes = vec![
                observed("lmcache", server, "alive"),
                observed("lmcache-1", 61, "alive"),
            ];
            if engine {
                processes.extend([
                    observed("api", 70, "alive"),
                    observed("worker-0", 71, "alive"),
                ]);
            }
            residency("restored", false, true, processes)
        };
        let (alive, facts) = residency_evidence(false, &recorded, &restored(60, true)).unwrap();
        assert_eq!(alive.len(), 4);
        assert!(alive.contains(&process(&observed("api", 70, "alive"))));
        assert_eq!(facts.last(), Some(&Milestone::ModelUsable));
        for uncertain in [restored(99, true), restored(60, false)] {
            assert!(
                matches!(
                    residency_evidence(false, &recorded, &uncertain),
                    Err(RuntimeError::Uncertain(_))
                ),
                "{uncertain:?}"
            );
        }
        // Not usable: the restore proves nothing.
        let mut unusable = restored(60, true);
        unusable.model_usable = false;
        assert!(matches!(
            residency_evidence(false, &recorded, &unusable),
            Err(RuntimeError::Uncertain(_))
        ));
    }

    /// ADR 0028 note of 2026-10-16, ADR 0033 A14 (found live 2026-10-23): a
    /// restart-park's wake canary counts on the engine its restore named
    /// beside the server, as the coordinator passes it. The server alone the
    /// restore recorded names no engine, so judged on it every such canary
    /// failed; another engine, a recorded engine process gone, or the claim
    /// not retained still does not count.
    // T45 T20 T34
    #[test]
    fn a_restart_park_wake_canary_counts_on_the_new_engine_the_restore_named() {
        use capyctl_domain::completion::ExecutionIdentities;
        let server = vec![
            process(&observed("lmcache", 60, "alive")),
            process(&observed("lmcache-1", 61, "alive")),
        ];
        let restored = residency_evidence(false, &server, &{
            let mut processes = observed_all(&server);
            processes.extend([
                observed("api", 70, "alive"),
                observed("worker-0", 71, "alive"),
            ]);
            residency("restored", false, true, processes)
        })
        .unwrap()
        .0;
        let answer = |processes: Vec<pb::OwnedProcessObservation>| {
            let mut r = result(processes);
            r.state = "completed".into();
            r.claim_retained = true;
            r
        };
        let woken = answer(observed_all(&restored));
        canary_engine_alive(&ExecutionIdentities::Retained(restored.clone()), &woken).unwrap();
        assert!(matches!(
            canary_engine_alive(&ExecutionIdentities::Retained(server.clone()), &woken),
            Err(RuntimeError::Uncertain(_))
        ));
        let mut other = observed_all(&server);
        other.extend([
            observed("api", 80, "alive"),
            observed("worker-0", 81, "alive"),
        ]);
        let mut worker_gone = observed_all(&restored);
        worker_gone
            .iter_mut()
            .filter(|p| p.role == "worker-0")
            .for_each(|p| p.presence = "gone".into());
        let mut released = woken.clone();
        released.claim_retained = false;
        for uncertain in [answer(other), answer(worker_gone), released] {
            assert!(
                matches!(
                    canary_engine_alive(
                        &ExecutionIdentities::Retained(restored.clone()),
                        &uncertain
                    ),
                    Err(RuntimeError::Uncertain(_))
                ),
                "{uncertain:?}"
            );
        }
    }

    fn observed_all(identities: &[ProcessIdentity]) -> Vec<pb::OwnedProcessObservation> {
        identities
            .iter()
            .map(|p| pb::OwnedProcessObservation {
                role: p.role.clone(),
                pid: p.pid,
                boot_id: p.boot_id.clone(),
                start_ticks: p.start_ticks,
                presence: "alive".into(),
                rank: None,
            })
            .collect()
    }

    // T46 (ADR 0035 §6): the remote readiness receipt records the ranks the
    // host's own observation named, one per GPU in rank order; a launch
    // naming none (one GPU, llama.cpp) keeps the plain receipt, and ranks
    // that do not cover the GPUs prove nothing.
    #[test]
    fn the_remote_receipt_records_the_ranks_the_host_named() {
        let observed = |role: &str, pid: u32, rank: Option<u32>| pb::OwnedProcessObservation {
            role: role.into(),
            pid,
            boot_id: "boot".into(),
            start_ticks: u64::from(pid),
            presence: "alive".into(),
            rank,
        };
        let devices = vec!["gpu0".to_owned(), "gpu1".to_owned()];
        let result = |processes| pb::MemberExecutionResult {
            processes,
            ..Default::default()
        };
        let named = result(vec![
            observed("api", 10, None),
            observed("worker-0", 11, None),
            observed("rank-1", 13, Some(1)),
            observed("rank-0", 12, Some(0)),
        ]);
        let receipt = remote_receipt(&named, &devices).unwrap();
        assert_eq!(
            receipt,
            "authenticated host native model probe; tp ranks: 0=rank-0:12@gpu0 1=rank-1:13@gpu1"
        );
        assert_eq!(
            capyctl_domain::local_tp::rank_of(
                &receipt,
                &ProcessIdentity {
                    role: "rank-1".into(),
                    pid: 13,
                    boot_id: "boot".into(),
                    start_ticks: 13,
                }
            )
            .map(|r| r.device),
            Some("gpu1".into())
        );
        let plain = result(vec![observed("api", 10, None)]);
        assert_eq!(
            remote_receipt(&plain, &[]).unwrap(),
            "authenticated host native model probe"
        );
        let partial = result(vec![
            observed("api", 10, None),
            observed("rank-0", 12, Some(0)),
        ]);
        assert!(remote_receipt(&partial, &devices).is_err());
    }
}

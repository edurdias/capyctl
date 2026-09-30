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
    /// ADR 0013 §5: the instance every command of this launch names, so the
    /// host fences it by that instance's own generation. Zero on a host that
    /// does not advertise per-instance fencing: its journal fences per
    /// deployment, and a nonzero index would not survive its digest check.
    pub instance_index: u32,
}
struct RemoteEngine {
    sessions: Arc<AgentSessions>,
    owner: SharedCoordinatorState,
    binding: Arc<RemoteLaunchBinding>,
    readiness: ReadinessLedger,
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
    let engine = Arc::new(RemoteEngine {
        sessions: sessions.clone(),
        owner,
        binding: binding.clone(),
        readiness,
    });
    remote_binding(engine, sessions, binding)
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
    fn retain(
        &self,
        command: &MemberCommand,
        result: &pb::MemberExecutionResult,
    ) -> Result<(), Box<tonic::Status>> {
        let Some(api) = result.processes.iter().find(|p| p.role == "api") else {
            return Ok(());
        };
        let owner = self
            .owner
            .lock()
            .map_err(|_| tonic::Status::internal("ownership unavailable"))?;
        owner
            .store()
            .record_api_identity(
                owner.session(),
                &capyctl_store::lifecycle::DeploymentFence {
                    deployment_id: command.identity.deployment_id.clone(),
                    revision: command.identity.revision,
                    generation: command.identity.generation,
                },
                &self.binding,
                &process(api),
            )
            .map_err(|_| {
                Box::new(tonic::Status::internal(
                    "remote ownership could not be retained",
                ))
            })
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
        })
    }
}

/// SPEC §§6.1, 9.1, 13.2: what an authenticated Park or Restore result proves.
/// `parked` counts only without a usable-model claim, `restored` only with one,
/// and both only on completion with the claim retained and the live group
/// exactly the identities the controller recorded. `unchanged` is a refusal
/// without effect; every other result is uncertain.
fn residency_evidence(
    park: bool,
    recorded: &[ProcessIdentity],
    result: &pb::MemberExecutionResult,
) -> Result<(Vec<ProcessIdentity>, Vec<Milestone>), RuntimeError> {
    let key = |p: &ProcessIdentity| (p.role.clone(), p.pid, p.boot_id.clone(), p.start_ticks);
    let mut alive: Vec<_> = result
        .processes
        .iter()
        .filter(|p| p.presence == "alive")
        .map(process)
        .collect();
    let mut expected = recorded.to_vec();
    alive.sort_by_key(key);
    expected.sort_by_key(key);
    let held = result.state == "completed"
        && result.claim_retained
        && !expected.is_empty()
        && alive == expected;
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
        if b.instance_index != 0 {
            needs.push(capabilities::INSTANCE_INDEX);
        }
        if capyctl_protocol::execution::MaterializeSourcePlan::new(
            &plan.deployment_config,
            &plan.host_policy_fingerprint,
        )
        .is_some()
        {
            needs.push(capabilities::MODEL_SOURCES);
        }
        if b.device_memory {
            needs.push(capabilities::DEVICE_MEMORY_DOMAINS);
        }
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
        launch_evidence(&result)?;
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
            receipt: "authenticated host native model probe".into(),
            facts: vec![
                Milestone::AllocationsRestored,
                Milestone::WeightsUsable,
                Milestone::CacheValid,
                Milestone::ModelUsable,
            ],
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
    async fn cancel_work(
        &self,
        _: &MemberRef,
        _: &RequestRef,
        _: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
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
        let denied = || CoordinatorError::Service("remote frozen binding unavailable".into());
        let owner = self.owner.lock().map_err(|_| denied())?;
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
        // frozen launch names as its one selected device.
        let device = match work.effective().selected_devices.as_slice() {
            [claim] => Some(claim.id.as_str()),
            _ => None,
        };
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
        let plan = SingleLaunchPlan {
            // ADR 0014 §5: the host resolves with the facts this revision was
            // frozen with, so both sides derive the same memory request.
            checkpoint_weights_bytes: work.effective().engine_config.memory().weights_bytes,
            // Owner decision 2026-09-23: the peak this launch reserved.
            startup_bytes: Some(work.startup_reservation().bytes),
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
        Ok(binding(
            self.sessions.clone(),
            self.owner.clone(),
            RemoteLaunchBinding {
                controller_id: self.controller_id.clone(),
                host_id: host.clone(),
                member_id: "head".into(),
                profile_fingerprint: work.effective().profile.build_fingerprint.clone(),
                launch_command_id: work.step_id().into(),
                plan,
                ingress_gate_key: key,
                instance_index,
                // ADR 0019: the Ready footprint is charged to a device domain.
                device_memory: work.effective().ready_device_allocation().is_some(),
            },
            self.readiness.clone(),
        ))
    }
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
                startup_bytes: None,
            },
            ingress_gate_key: [7; 32],
            instance_index: 0,
            device_memory: false,
        }
    }
    fn observed(role: &str, pid: u32, presence: &str) -> pb::OwnedProcessObservation {
        pb::OwnedProcessObservation {
            role: role.into(),
            pid,
            boot_id: "boot".into(),
            start_ticks: 9,
            presence: presence.into(),
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
}

//! ADR 0028 §5–§9: activating a multi-node engine group.
//!
//! One activation runs under the instance's Initialize step, in this order:
//!
//! 1. every member host's live session must declare `engine_groups` (T34),
//!    and every member's resolved deployment passes the residency gate the
//!    single-host launch applies (`deep_wake_refusal`);
//! 2. the weights are materialized and measured on every host at once and the
//!    digests must agree; the agreed digest is recorded for the revision
//!    (`group_sources`);
//! 3. a deep SGLang group whose members' model paths differ is refused
//!    (`check_member_paths`, decided 2026-10-06); nothing is reserved yet;
//! 4. every member is reserved on its own host, judged against that host's
//!    own admission context, all or nothing (`reserve_group`);
//! 5. `Prepare` goes to every member at once; any refusal releases every
//!    reservation (no member was dispatched) and names its closed code. A
//!    head port held outside CapyCTL is excluded first, so the next attempt
//!    draws another (R15);
//! 6. the step is armed (the members already hold their charges);
//! 7. every member is fenced as dispatched, durably, before its `Launch` is
//!    sent and before every resend of it (R23), and the Launches go out at
//!    once, each carrying its host's own `SingleLaunchPlan` (R29); each
//!    reply's identities are recorded for its member;
//! 8. the head proves readiness (its engine's own native check, bounded by
//!    `timeouts.initialize`) while every member's reply is watched for an
//!    exit; then one 1-token completion through the head (`probe_head`)
//!    must answer before READY;
//! 9. the step completes: the binding on the head's ingress is the group's
//!    only replica, so the route opens there and nowhere else.
//!
//! Atomic accounting in the store, never an atomic launch: a member whose
//! reply is lost stays dispatched and charged until its own host reports it
//! (Task 17 reconciles and stops it). Nothing here qualifies an engine; the
//! live rows MN1–MN9 do.
use crate::{
    coordinator::{ServiceClock, ServiceObservation},
    group_sources::{agree_digests, materialize_on_all, MemberSource, RemoteGroupSources},
    ownership::SharedCoordinatorState,
};
use capyctl_config::{
    effective::{EffectiveDeployment, Residency},
    groups_policy::GroupsPolicy,
    topology::GroupShape,
};
use capyctl_domain::{
    completion::{CompletionEvidence, Milestone, OwnedLaunchReceipt, ProcessIdentity},
    group::{
        member_id, CommandIdentity, GroupEngine, GroupPlan, GroupTopology, MemberKey, MemberPlan,
        MemberRole,
    },
    resources::MemoryLimit,
};
use capyctl_protocol::{
    capabilities,
    execution::{MemberAction, MemberCommand, SingleLaunchPlan},
    pb,
};
use capyctl_scheduler::residency::AdmissionContext;
use capyctl_store::{
    groups::{member_owner_id, GroupReservation, GroupStoreError},
    ordinary_lifecycle::worker::{GroupMemberResolution, InitializeWork},
    resource_ledger::GrantRequest,
};
use std::{
    collections::BTreeMap, future::Future, net::IpAddr, path::PathBuf, pin::Pin, sync::Arc,
    time::Duration,
};

/// ADR 0028 §7, §8 (Task 13): the state a group `Prepare` and `Launch` are
/// authorized in on the member's agent, the value the single-rank
/// `LaunchSingle` path and the agent's `authorize` already pin.
pub const RESERVED_STATE: &str = "reserved";
/// ADR 0028 §9: a completion probe is authorized against a Ready launch.
const READY_STATE: &str = "ready";
/// ADR 0028 §5 (R15): how long a rendezvous port a `Prepare` found held
/// outside CapyCTL is skipped on its head before it is drawn again.
pub const RENDEZVOUS_EXCLUSION: Duration = Duration::from_secs(10 * 60);
/// ADR 0028 §9 (decided 2026-10-06): the readiness probe asks for one token.
pub const READINESS_PROBE_TOKENS: u32 = 1;
/// ADR 0028 §9 (decided 2026-10-06): the readiness probe's bound.
pub const READINESS_PROBE_DEADLINE: Duration = Duration::from_secs(60);
/// ADR 0028 §8 (R23): how many times one Launch with no answer is resent,
/// identically, before its member is left uncertain and charged.
pub const LAUNCH_RESENDS: u32 = 2;
/// ADR 0028 §8, §11: once a member's Launch failed, how long the Launches
/// still in flight are driven before the group is stopped, so each reaches
/// its host and a prompt reply has its identities recorded (R23).
pub const FAILED_LAUNCH_GRACE: Duration = Duration::from_secs(1);

pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// ADR 0028 §3: what one member host publishes for a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberHost {
    /// The fingerprint of the host's published policy, which every command a
    /// launch on that host carries.
    pub policy_fingerprint: String,
    /// `resource_policy.groups`: the peer address and, on the head, the
    /// rendezvous range.
    pub groups: GroupsPolicy,
}

/// ADR 0028 §8: the coordinator's transport to the member hosts of a group,
/// through their authenticated sessions. Construction is no authority to
/// send: the activation decides every command and its order.
pub trait GroupHosts: Send + Sync + 'static {
    /// The controller every member's agent is enrolled with.
    fn controller_id(&self) -> String;
    /// The host's current publication for a group.
    fn host(&self, host_id: &str) -> Result<MemberHost, String>;
    /// ADR 0017, T34: refuse, typed, a host whose live session lacks one of
    /// `needs`, before anything is sent to it.
    fn preflight(&self, host_id: &str, needs: &[&str]) -> Result<(), String>;
    /// Whether the host's live session declares `capability`.
    fn supports(&self, host_id: &str, capability: &str) -> bool;
    /// Send one command and wait for its terminal result. `Err` is no answer:
    /// nothing is known about the command's effect.
    fn execute(
        &self,
        command: MemberCommand,
    ) -> HostFuture<'_, Result<pb::MemberExecutionResult, String>>;
    /// ADR 0012: provision the head's private ingress with its launch's keys
    /// before its `Launch` is sent. A worker is never provisioned.
    fn provision_head<'a>(
        &'a self,
        command: &'a MemberCommand,
    ) -> HostFuture<'a, Result<(), String>>;
    /// SPEC §13.2 (G2): the head proved readiness for `binding_id` on its
    /// current session; dispatch to it is open while that session lasts.
    fn head_ready(&self, binding_id: &str, host_id: &str);
    /// SPEC §17: how an activation concluded (its outcome, or the refusal
    /// that stopped it before any member was dispatched).
    fn concluded(
        &self,
        _deployment_id: &str,
        _outcome: Result<&GroupActivation, &GroupActivationError>,
    ) {
    }
    /// ADR 0028 §12 (R12): the saver-map facts `member`'s own host reports
    /// for a SGLang member it runs, read from that host's own observation
    /// directory (enrolled at the member's launch): the bytes the member's
    /// saver still maps and the host time they were observed at. `Err` is no
    /// report; nothing falls back to process sampling.
    fn saver_mapped<'a>(
        &'a self,
        deployment_id: &'a str,
        member: &'a capyctl_store::ordinary_lifecycle::park::ArmedMember,
    ) -> HostFuture<'a, Result<(i64, i64), String>>;
    /// SPEC §17: how a group park or wake of `deployment_id` concluded.
    fn residency_concluded(
        &self,
        _deployment_id: &str,
        _kind: capyctl_store::ordinary_lifecycle::park::ResidencyKind,
        _outcome: Result<(), &crate::group_residency::GroupResidencyError>,
    ) {
    }
}

/// What one group activation needs from its coordinator.
pub struct GroupCtx {
    pub owner: SharedCoordinatorState,
    pub hosts: Arc<dyn GroupHosts>,
    pub observations: Arc<dyn ServiceObservation>,
    pub clock: ServiceClock,
}

/// How an activation that reached its Launches ended. `Failed` hands the
/// group to the group stop (Task 17): every dispatched member stays charged
/// until its own host proves it gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupActivation {
    Ready {
        plan: GroupPlan,
    },
    Failed {
        plan: GroupPlan,
        failed_rank: u32,
        failure: LaunchFailure,
        reason: String,
    },
}

/// ADR 0028 §11, §16: what happened at the rank an activation ended at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchFailure {
    /// The member exited or refused, or the head did not prove readiness
    /// (the readiness probe included).
    MemberFailed,
    /// The member's Launch went unanswered: it may run, so it stays charged
    /// and uncertain until its own host proves it gone.
    MemberUncertain,
}

impl LaunchFailure {
    /// The closed code (spec §16) the instance's status names.
    pub fn code(self) -> &'static str {
        match self {
            Self::MemberFailed => "group_member_failed",
            Self::MemberUncertain => "group_member_uncertain",
        }
    }
}

/// Why an activation stopped before any member was dispatched. Every
/// reservation it made is released; the step stays planned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GroupActivationError {
    /// A closed refusal (spec §16), with what it names.
    #[error("{code}: {detail}")]
    Refused { code: String, detail: String },
    /// The store or a host could not be read; nothing was reserved or sent.
    #[error("group activation unavailable: {0}")]
    Unavailable(String),
}

impl GroupActivationError {
    fn refused(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Refused {
            code: code.into(),
            detail: detail.into(),
        }
    }
    /// The closed code, or `unavailable`.
    pub fn code(&self) -> &str {
        match self {
            Self::Refused { code, .. } => code,
            Self::Unavailable(_) => "unavailable",
        }
    }
}

/// Why a completion probe through the head proved nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbeError {
    #[error("the head did not answer the completion probe within {0:?}")]
    Timeout(Duration),
    #[error("the completion probe through the head failed: {0}")]
    Failed(String),
}

/// The head launch a completion probe is sent to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadLaunch {
    pub plan: GroupPlan,
    pub deployment_id: String,
    pub revision: i64,
    pub operation_id: String,
    /// The head's `Launch` command id: the launch its agent retained.
    pub owned_handle: String,
    pub profile_fingerprint: String,
}

/// ADR 0028 §6 (decided 2026-10-06): a SGLang group with deep park whose
/// members' model paths differ is refused `group_model_path_mismatch`, naming
/// the head's path and the member's: SGLang's wake posts the head's path to
/// every rank. Restart-only SGLang groups, vLLM and TensorFold keep differing
/// paths.
pub fn check_member_paths(
    engine: GroupEngine,
    residency: Residency,
    head: &str,
    paths: &BTreeMap<String, PathBuf>,
) -> Result<(), GroupActivationError> {
    if engine != GroupEngine::Sglang || residency != Residency::Deep {
        return Ok(());
    }
    let head_path = paths
        .get(head)
        .ok_or_else(|| GroupActivationError::Unavailable("the head has no model path".into()))?;
    match paths.iter().find(|(_, path)| *path != head_path) {
        None => Ok(()),
        Some((host, path)) => Err(GroupActivationError::refused(
            "group_model_path_mismatch",
            format!(
                "a deep SGLang group needs one model path on every host: the head {head} \
                 loads {} and {host} loads {}",
                head_path.display(),
                path.display()
            ),
        )),
    }
}

/// ADR 0028 §9: one completion of at most `max_tokens` tokens (temperature
/// 0) through the head's agent, which runs it on loopback against its
/// retained launch with the per-launch key (ADR 0012), never through ingress
/// or the router. Worker hosts are never sent one. The one probe function of
/// readiness (1 token), the wake canary and the request-stall check; it
/// waits at most `deadline` and returns the generated token ids.
pub async fn probe_head(
    ctx: &GroupCtx,
    head: &HeadLaunch,
    max_tokens: u32,
    deadline: Duration,
) -> Result<Vec<u32>, ProbeError> {
    let command = probe_command(&ctx.hosts.controller_id(), head, max_tokens, deadline);
    let result = tokio::time::timeout(deadline, ctx.hosts.execute(command))
        .await
        .map_err(|_| ProbeError::Timeout(deadline))?
        .map_err(ProbeError::Failed)?;
    probe_answer(&result, head, max_tokens)
}

/// The completion probe [`probe_head`] sends, for a transport of its own
/// (the readiness supervisor re-proves a head on its new session with it).
pub fn probe_command(
    controller_id: &str,
    head: &HeadLaunch,
    max_tokens: u32,
    deadline: Duration,
) -> MemberCommand {
    let host = head.plan.head().member.host_id.clone();
    let id = ulid::Ulid::new().to_string();
    let mut command = MemberCommand {
        identity: CommandIdentity {
            controller_id: controller_id.to_owned(),
            member: MemberKey {
                host_id: host,
                member_id: member_id(0),
            },
            deployment_id: head.deployment_id.clone(),
            operation_id: head.operation_id.clone(),
            command_id: id.clone(),
            step_id: id,
            generation: head.plan.generation(),
            revision: head.revision,
            deadline_ms: capyctl_protocol::now_unix_ms()
                .saturating_add(i64::try_from(deadline.as_millis()).unwrap_or(i64::MAX)),
            payload_digest: [0; 32],
            expected_state: READY_STATE.into(),
            profile_fingerprint: head.profile_fingerprint.clone(),
            instance_index: 0,
        },
        action: MemberAction::Probe {
            owned_handle: head.owned_handle.clone(),
            max_tokens: Some(max_tokens),
        },
    };
    command.identity.payload_digest = command.canonical_digest();
    command
}

/// The generated token ids of a completion probe's answer: completed on the
/// head's own launch, at least one token and at most `max_tokens`.
pub fn probe_answer(
    result: &pb::MemberExecutionResult,
    head: &HeadLaunch,
    max_tokens: u32,
) -> Result<Vec<u32>, ProbeError> {
    if result.state != "completed" || result.owned_handle != head.owned_handle {
        return Err(ProbeError::Failed(
            "the head did not complete the probe on its launch".into(),
        ));
    }
    if result.probe_tokens.is_empty() || result.probe_tokens.len() > max_tokens as usize {
        return Err(ProbeError::Failed("the head generated no token".into()));
    }
    Ok(result.probe_tokens.clone())
}

/// One member as the activation prepares it.
struct Member {
    rank: u32,
    host: String,
    resolution: GroupMemberResolution,
    published: MemberHost,
    peer: IpAddr,
    local: serde_json::Value,
    profile_name: String,
    devices: Vec<String>,
}

fn engine_of(effective: &EffectiveDeployment) -> Result<GroupEngine, GroupActivationError> {
    use capyctl_config::engine_policy::Engine;
    Ok(match effective.profile.engine {
        Engine::Vllm => GroupEngine::Vllm,
        Engine::Sglang => GroupEngine::Sglang,
        Engine::Tensorfold => GroupEngine::Tensorfold,
        // ADR 0029 §1: multi-rank groups are out of llama.cpp's scope.
        Engine::Llamacpp => {
            return Err(GroupActivationError::refused(
                "group_shape_unsupported",
                "llamacpp: no multi-node mode",
            ))
        }
    })
}

fn locked<T>(
    ctx: &GroupCtx,
    f: impl FnOnce(&crate::ownership::OwnedCoordinatorState) -> Result<T, String>,
) -> Result<T, GroupActivationError> {
    let owner = ctx
        .owner
        .lock()
        .map_err(|_| GroupActivationError::Unavailable("ownership mutex poisoned".into()))?;
    f(&owner).map_err(GroupActivationError::Unavailable)
}

fn process(p: &pb::OwnedProcessObservation) -> ProcessIdentity {
    ProcessIdentity {
        role: p.role.clone(),
        pid: p.pid,
        boot_id: p.boot_id.clone(),
        start_ticks: p.start_ticks,
    }
}

fn alive(result: &pb::MemberExecutionResult) -> Vec<ProcessIdentity> {
    result
        .processes
        .iter()
        .filter(|p| p.presence == "alive")
        .map(process)
        .collect()
}

/// ADR 0028 §5–§9: activate the group `shape` for the planned Initialize
/// `work` (see the module documentation for the order). `Err` means no
/// member was dispatched and nothing it reserved is still held; the step is
/// still planned. Once the step is armed the outcome is a [`GroupActivation`].
pub async fn activate_group(
    ctx: &GroupCtx,
    work: &InitializeWork,
    shape: &GroupShape,
) -> Result<GroupActivation, GroupActivationError> {
    let outcome = activate(ctx, work, shape).await;
    if let Err(GroupActivationError::Refused { code, .. }) = &outcome {
        record_refusal(ctx, work, code)?;
    }
    outcome
}

/// SPEC §17, ADR 0028 §16: every refusal of an activation, wherever it
/// arises, leaves its closed code in the instance's status.
fn record_refusal(
    ctx: &GroupCtx,
    work: &InitializeWork,
    code: &str,
) -> Result<(), GroupActivationError> {
    let fence = work.fence();
    locked(ctx, |o| {
        o.store()
            .record_instance_error(
                &fence.deployment_id,
                work.instance_index(),
                fence.generation,
                code,
            )
            .map_err(|e| e.to_string())
    })
}

async fn activate(
    ctx: &GroupCtx,
    work: &InitializeWork,
    shape: &GroupShape,
) -> Result<GroupActivation, GroupActivationError> {
    let fence = work.fence().clone();
    let instance = work.instance_index();
    let head_host = shape.head().to_owned();
    // ADR 0028 §14, T34: a member host whose live session lacks
    // `engine_groups` is refused, typed, and is sent nothing; neither is any
    // other member.
    for host in &shape.hosts {
        ctx.hosts
            .preflight(host, &[capabilities::ENGINE_GROUPS])
            .map_err(|code| GroupActivationError::refused(code, format!("host {host}")))?;
    }
    let resolutions = locked(ctx, |o| {
        o.store()
            .group_member_resolutions(&fence.deployment_id, fence.revision, &shape.hosts)
            .map_err(|e| e.to_string())
    })?;
    let mut members = Vec::with_capacity(resolutions.len());
    for resolution in resolutions {
        let host = resolution.host_id.clone();
        let published = ctx
            .hosts
            .host(&host)
            .map_err(|e| GroupActivationError::Unavailable(format!("host {host}: {e}")))?;
        // ADR 0028 §3: every member needs its peer address.
        let peer = published.groups.peer_address.ok_or_else(|| {
            GroupActivationError::refused(
                "peer_address_missing",
                format!("host {host} declares no resource_policy.groups.peer_address"),
            )
        })?;
        // SPEC §§6.2, 9.1 (main #65): the residency gate a single-host launch
        // applies, decided for every member's own resolution.
        if let Some(reason) = resolution.effective.deep_wake_refusal() {
            return Err(GroupActivationError::refused(
                reason,
                format!("host {host} cannot honor the residency"),
            ));
        }
        let local =
            capyctl_config::remote_resources::local_deployment_document(&host, &resolution.source)
                .map_err(|e| GroupActivationError::Unavailable(e.to_string()))?;
        let profile_name = local["runtime_profile"]
            .as_str()
            .ok_or_else(|| GroupActivationError::Unavailable("no runtime profile".into()))?
            .to_owned();
        let devices: Vec<String> = local["devices"]
            .as_array()
            .map(|devices| {
                devices
                    .iter()
                    .filter_map(|d| d["id"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        members.push(Member {
            rank: resolution.rank,
            host,
            resolution,
            published,
            peer,
            local,
            profile_name,
            devices,
        });
    }
    let head = members
        .first()
        .ok_or_else(|| GroupActivationError::Unavailable("a group has no head".into()))?;
    let engine = engine_of(&head.resolution.effective)?;
    let residency = head.resolution.effective.residency;
    // ADR 0028 §6: the weights on every host at once, then digest agreement.
    let driver = RemoteGroupSources {
        owner: ctx.owner.clone(),
        hosts: ctx.hosts.clone(),
        controller_id: ctx.hosts.controller_id(),
        deployment_id: fence.deployment_id.clone(),
        revision: fence.revision,
        generation: fence.generation,
        deadline_ms: work.deadline_ms(),
        members: members
            .iter()
            .map(|m| {
                (
                    m.host.clone(),
                    MemberSource {
                        member_id: member_id(m.rank),
                        deployment_config: m.local.to_string(),
                        host_policy_fingerprint: m.published.policy_fingerprint.clone(),
                        profile_fingerprint: m
                            .resolution
                            .effective
                            .profile
                            .build_fingerprint
                            .clone(),
                        model_path: m
                            .resolution
                            .effective
                            .model
                            .resolved_path
                            .clone()
                            .unwrap_or_default(),
                    },
                )
            })
            .collect(),
    };
    let materialized = materialize_on_all(
        &shape.hosts,
        &head.resolution.effective.model.source,
        &driver,
    )
    .await
    .map_err(|e| GroupActivationError::refused(e.code().to_owned(), e.to_string()))?;
    let digests: BTreeMap<String, String> = materialized
        .iter()
        .map(|(host, m)| (host.clone(), m.digest.clone()))
        .collect();
    let digest = agree_digests(&digests)
        .map_err(|e| GroupActivationError::refused(e.code(), e.to_string()))?;
    // ADR 0014 §7, ADR 0028 §6: the agreed digest is the revision's, recorded
    // under the head's measurement; one that differs from what the revision
    // already recorded or declared refuses the start.
    let measured = &materialized[&head_host];
    let recorded = locked(ctx, |o| {
        o.store()
            .record_checkpoint_measurement(
                o.session(),
                &fence.deployment_id,
                fence.revision,
                &head_host,
                &digest,
                measured.weights_bytes,
                measured.state_slot_bytes,
                measured.layout,
                measured.provenance,
                measured.tables,
                capyctl_protocol::now_unix_ms(),
            )
            .map_err(|e| e.to_string())
    })?;
    match recorded {
        capyctl_store::checkpoint_digests::RecordOutcome::Recorded { digest: d, .. }
            if d == digest => {}
        _ => {
            return Err(GroupActivationError::refused(
                "checkpoint_mismatch",
                "the group's agreed digest is not the revision's recorded digest",
            ))
        }
    }
    // ADR 0028 §6 (decided 2026-10-06): before anything is reserved.
    let paths: BTreeMap<String, PathBuf> = materialized
        .iter()
        .map(|(host, m)| (host.clone(), PathBuf::from(&m.path)))
        .collect();
    check_member_paths(engine, residency, &head_host, &paths)?;
    let plan = reserve(ctx, work, shape, &members, engine, &materialized, &digest).await?;
    // ADR 0028 §7: every member checks its own host at once.
    if let Err((rank, code)) = prepare_all(ctx, work, &plan, &members).await {
        release_refused(ctx, work, &plan, &head_host, rank, &code)?;
        return Err(GroupActivationError::refused(
            code,
            format!(
                "host {} refused the group",
                plan.members()[rank as usize].member.host_id
            ),
        ));
    }
    // The step is armed only now: the members already hold their charges.
    let now = (ctx.clock)().map_err(|e| GroupActivationError::Unavailable(e.to_string()))?;
    let context = match locked(ctx, |o| {
        o.store()
            .arm_group_initialize(o.session(), work.step_id(), now)
            .map_err(|e| e.to_string())
    }) {
        Ok(context) => context,
        Err(error) => {
            // Nothing was dispatched: every reservation is released.
            let _ = locked(ctx, |o| {
                o.store()
                    .release_undispatched_group(&fence.deployment_id, instance, fence.generation)
                    .map(drop)
                    .map_err(|e| e.to_string())
            });
            return Err(error);
        }
    };
    let launches = launch_commands(ctx, work, &context, &plan, &members, &digest)?;
    let outcome = launch_all(ctx, work, &members, &launches).await;
    let outcome = match outcome {
        Ok((head_reply, head_identities)) => {
            ready(
                ctx,
                work,
                &plan,
                &members,
                &launches,
                head_reply,
                head_identities,
            )
            .await
        }
        Err((rank, failure, reason)) => GroupActivation::Failed {
            plan: plan.clone(),
            failed_rank: rank,
            failure,
            reason,
        },
    };
    Ok(outcome)
}

/// ADR 0028 §5, §12: the memory limits a member host's own policy admits
/// its member against, one per domain.
pub(crate) fn host_limits(
    controls: &capyctl_config::resource_controls::ResourceControls,
) -> Vec<MemoryLimit> {
    controls
        .domains
        .iter()
        .map(|(domain, d)| MemoryLimit {
            domain: domain.clone(),
            managed_bytes: d.managed_limit,
            free_reserve_bytes: d.free_reserve,
            reserve_absorbs_unmanaged: d.memory == capyctl_config::effective::DomainMemory::Device,
            host_kv_bytes: d.host_kv_limit,
            parked_bytes: d.parked_limit,
        })
        .collect()
}

/// ADR 0028 §5, §12: one admission context per member host, from that host's
/// own policy and observations, and every member reserved at once.
async fn reserve(
    ctx: &GroupCtx,
    work: &InitializeWork,
    shape: &GroupShape,
    members: &[Member],
    engine: GroupEngine,
    materialized: &BTreeMap<String, crate::group_sources::Materialized>,
    digest: &str,
) -> Result<GroupPlan, GroupActivationError> {
    let fence = work.fence();
    let instance = work.instance_index();
    let mut observed = BTreeMap::new();
    for member in members {
        let observations = ctx
            .observations
            .observe(member.host.clone())
            .await
            .map_err(|e| GroupActivationError::Unavailable(e.to_string()))?;
        let controls = locked(ctx, |o| {
            o.store()
                .resource_policy(&member.host)
                .map_err(|e| e.to_string())?
                .map(|policy| policy.controls)
                .ok_or_else(|| format!("host {} has no resource policy", member.host))
        })?;
        let limits = host_limits(&controls);
        observed.insert(member.host.clone(), (observations, limits, controls));
    }
    let head = &members[0];
    let service_port: u16 = work
        .endpoint()
        .rsplit(':')
        .next()
        .and_then(|port| port.parse().ok())
        .ok_or_else(|| GroupActivationError::Unavailable("the head has no endpoint".into()))?;
    let topology = GroupTopology {
        tensor_parallel: shape.topology.tensor_parallel,
        pipeline_parallel: shape.topology.pipeline_parallel,
        local_ranks: shape.local_ranks,
    };
    // ADR 0028 §5: SGLang workers listen on a loopback port of their own host.
    let worker_ports: BTreeMap<String, std::ops::RangeInclusive<u16>> =
        if engine == GroupEngine::Sglang {
            members[1..]
                .iter()
                .map(|m| {
                    let range = &m.resolution.effective.host.endpoint_port_range;
                    (m.host.clone(), range.start..=range.end)
                })
                .collect()
        } else {
            BTreeMap::new()
        };
    for attempt in 0..2 {
        let now = (ctx.clock)().map_err(|e| GroupActivationError::Unavailable(e.to_string()))?;
        let epoch = locked(ctx, |o| {
            o.store()
                .resource_snapshot()
                .map(|s| s.epoch)
                .map_err(|e| e.to_string())
        })?;
        // ADR 0028 §5: the members are charged under the start's own
        // Initialize operation, each attempt with fresh grant ids.
        let operation = work.operation_id().to_owned();
        let reservation = GroupReservation {
            deployment_id: fence.deployment_id.clone(),
            instance_index: instance,
            members: members
                .iter()
                .map(|m| {
                    (
                        m.host.clone(),
                        GrantRequest {
                            id: ulid::Ulid::new().to_string(),
                            owner_id: member_owner_id(&fence.deployment_id, instance, m.rank),
                            deployment_id: fence.deployment_id.clone(),
                            operation_id: operation.clone(),
                            revision: fence.revision,
                            generation: fence.generation,
                            expected_epoch: epoch,
                            next: m.resolution.cold.clone(),
                        },
                    )
                })
                .collect(),
            head_host: head.host.clone(),
            port_range: head.published.groups.rendezvous_ports.clone(),
            worker_ports: worker_ports.clone(),
        };
        let contexts: BTreeMap<String, AdmissionContext<'_>> = observed
            .iter()
            .map(|(host, (observations, limits, controls))| {
                (
                    host.clone(),
                    AdmissionContext::new(
                        observations,
                        limits,
                        now,
                        controls.observation_ttl_ms,
                        controls.max_parked as usize,
                    ),
                )
            })
            .collect();
        let plan_for = |port: u16, workers: &BTreeMap<String, u16>| {
            GroupPlan::new(
                engine,
                members
                    .iter()
                    .map(|m| MemberPlan {
                        member: MemberKey {
                            host_id: m.host.clone(),
                            member_id: member_id(m.rank),
                        },
                        rank: m.rank,
                        role: if m.rank == 0 {
                            MemberRole::Head
                        } else {
                            MemberRole::Worker
                        },
                        profile_name: m.profile_name.clone(),
                        profile_fingerprint: m
                            .resolution
                            .effective
                            .profile
                            .build_fingerprint
                            .clone(),
                        // Task 13: the plan's checkpoint fingerprint is the
                        // agreed checkpoint digest.
                        checkpoint_fingerprint: digest.to_owned(),
                        model_path: materialized[&m.host].path.clone(),
                        devices: m.devices.clone(),
                        peer_address: m.peer,
                        service_port: (m.rank == 0).then_some(service_port),
                        worker_port: workers.get(&m.host).copied(),
                    })
                    .collect(),
                topology,
                port,
                fence.generation,
            )
        };
        let reserved = locked(ctx, |o| {
            Ok(o.store().reserve_group(&reservation, plan_for, &contexts))
        })?;
        match reserved {
            Ok(plan) => return Ok(plan),
            Err(GroupStoreError::PortsExhausted) => {
                return Err(GroupActivationError::refused(
                    "rendezvous_ports_exhausted",
                    format!("no free rendezvous port on the head {}", head.host),
                ))
            }
            Err(GroupStoreError::Admission(error)) => {
                return Err(GroupActivationError::refused(
                    "insufficient_memory",
                    format!("a member does not fit on its host: {error}"),
                ))
            }
            // An interrupted activation of this instance left a plan no member
            // of which was dispatched: it holds charges nothing uses. Activation
            // is single per instance, so it is released and the reservation
            // tried once more. A dispatched member keeps its charge (R23).
            Err(GroupStoreError::Conflict) if attempt == 0 => {
                let leftover = locked(ctx, |o| {
                    o.store()
                        .group_plan(&fence.deployment_id, instance)
                        .map_err(|e| e.to_string())
                })?;
                let Some((left, rows)) = leftover else {
                    continue;
                };
                let unsettled = rows
                    .iter()
                    .any(|r| r.state != capyctl_store::groups::MemberState::Settled);
                if unsettled && rows.iter().any(|r| r.dispatched) {
                    return Err(GroupActivationError::refused(
                        "group_member_uncertain",
                        "a member of this instance's previous plan may still run",
                    ));
                }
                if unsettled {
                    locked(ctx, |o| {
                        o.store()
                            .release_undispatched_group(
                                &fence.deployment_id,
                                instance,
                                left.generation(),
                            )
                            .map(drop)
                            .map_err(|e| e.to_string())
                    })?;
                }
            }
            Err(error) => return Err(GroupActivationError::Unavailable(error.to_string())),
        }
    }
    Err(GroupActivationError::Unavailable(
        "the group could not be reserved".into(),
    ))
}

fn identity(
    ctx: &GroupCtx,
    work: &InitializeWork,
    plan: &GroupPlan,
    member: &Member,
    command_id: String,
    expected_state: &str,
) -> CommandIdentity {
    CommandIdentity {
        controller_id: ctx.hosts.controller_id(),
        member: MemberKey {
            host_id: member.host.clone(),
            member_id: member_id(member.rank),
        },
        deployment_id: work.fence().deployment_id.clone(),
        operation_id: work.operation_id().to_owned(),
        command_id,
        step_id: work.step_id().to_owned(),
        generation: plan.generation(),
        revision: work.fence().revision,
        deadline_ms: work.deadline_ms(),
        payload_digest: [0; 32],
        expected_state: expected_state.into(),
        profile_fingerprint: member
            .resolution
            .effective
            .profile
            .build_fingerprint
            .clone(),
        // ADR 0028 §2: a group has one instance.
        instance_index: 0,
    }
}

/// ADR 0028 §7: `Prepare` to every member at once. The first refusal in rank
/// order, or a member that did not answer (`unavailable`).
async fn prepare_all(
    ctx: &GroupCtx,
    work: &InitializeWork,
    plan: &GroupPlan,
    members: &[Member],
) -> Result<(), (u32, String)> {
    let replies = futures::future::join_all(members.iter().map(|member| {
        let mut command = MemberCommand {
            // ADR 0028 §7 (Task 13): Prepare is authorized in `reserved`.
            identity: identity(
                ctx,
                work,
                plan,
                member,
                ulid::Ulid::new().to_string(),
                RESERVED_STATE,
            ),
            action: MemberAction::Prepare(plan.clone()),
        };
        command.identity.payload_digest = command.canonical_digest();
        async move { (member.rank, ctx.hosts.execute(command).await) }
    }))
    .await;
    for (rank, reply) in replies {
        match reply {
            Ok(result) if result.state == "completed" && result.refused.is_empty() => {}
            Ok(result) if !result.refused.is_empty() => return Err((rank, result.refused)),
            _ => return Err((rank, "unavailable".into())),
        }
    }
    Ok(())
}

/// ADR 0028 §5, §7 (R15): a refused `Prepare` releases every reservation
/// (nothing was dispatched) and the port; a head port held outside CapyCTL
/// is excluded first so the next attempt draws another.
fn release_refused(
    ctx: &GroupCtx,
    work: &InitializeWork,
    plan: &GroupPlan,
    head: &str,
    rank: u32,
    code: &str,
) -> Result<(), GroupActivationError> {
    let fence = work.fence();
    if let Some(port) = code
        .strip_prefix("rendezvous_port_in_use:")
        .and_then(|port| port.parse::<u16>().ok())
        .filter(|_| rank == 0)
    {
        let until =
            capyctl_protocol::now_unix_ms().saturating_add(RENDEZVOUS_EXCLUSION.as_millis() as i64);
        locked(ctx, |o| {
            o.store()
                .exclude_rendezvous_port(head, port, until)
                .map_err(|e| e.to_string())
        })?;
    }
    locked(ctx, |o| {
        o.store()
            .release_undispatched_group(
                &fence.deployment_id,
                work.instance_index(),
                plan.generation(),
            )
            .map(drop)
            .map_err(|e| e.to_string())
    })
}

/// ADR 0028 §8 (R29): every member's `Launch`, built once so a resend is the
/// identical command (same id, same digest), each carrying its own host's
/// `SingleLaunchPlan`. The head's is the step's own launch (its binding, its
/// incarnation, its grant, its service port); each worker gets fresh ids that
/// name nothing leased and service port 0.
fn launch_commands(
    ctx: &GroupCtx,
    work: &InitializeWork,
    context: &capyctl_domain::completion::StepExecutionContext,
    plan: &GroupPlan,
    members: &[Member],
    digest: &str,
) -> Result<Vec<MemberCommand>, GroupActivationError> {
    let session = locked(ctx, |o| Ok(o.session().id().to_owned()))?;
    let grant = context
        .grant_id
        .clone()
        .ok_or_else(|| GroupActivationError::Unavailable("the arm carries no grant".into()))?;
    let service_port = plan.head().service_port.unwrap_or(0);
    Ok(members
        .iter()
        .map(|member| {
            let effective = &member.resolution.effective;
            let memory = effective.engine_config.memory();
            let head = member.rank == 0;
            let fresh = || ulid::Ulid::new().to_string();
            let launch = SingleLaunchPlan {
                deployment_config: member.local.to_string(),
                profile_name: member.profile_name.clone(),
                checkpoint_fingerprint: effective.model.content_fingerprint.clone(),
                host_policy_fingerprint: member.published.policy_fingerprint.clone(),
                binding_id: if head {
                    work.binding_id().into()
                } else {
                    fresh()
                },
                incarnation: if head {
                    work.incarnation().into()
                } else {
                    fresh()
                },
                grant_id: if head { grant.clone() } else { fresh() },
                service_port: if head { service_port } else { 0 },
                issued_at_ms: context.issued_at_ms,
                coordinator_session_id: session.clone(),
                checkpoint_digest: digest.to_owned(),
                // ADR 0028 §5 (amendment of 2026-10-07): the facts the
                // member's share was resolved with: the whole checkpoint the
                // host verifies, and the layout its share is taken with.
                checkpoint_weights_bytes: memory.checkpoint_weights_bytes(),
                checkpoint_state_slot_bytes: memory.state_slot_bytes,
                checkpoint_layout: memory.member.and_then(|member| member.layout),
                // ADR 0014 amendment A20: and the tables an engine that keeps
                // them on disk was sized without.
                checkpoint_tables: memory.disk_tables.map(|recorded| recorded.tables),
                startup_bytes: None,
            };
            let command_id = if head {
                work.step_id().to_owned()
            } else {
                fresh()
            };
            let mut command = MemberCommand {
                // ADR 0028 §8 (Task 13): Launch is authorized in `reserved`.
                identity: identity(ctx, work, plan, member, command_id, RESERVED_STATE),
                action: MemberAction::Launch {
                    plan: plan.clone(),
                    member: launch,
                },
            };
            command.identity.payload_digest = command.canonical_digest();
            command
        })
        .collect())
}

/// ADR 0028 §8 (R23): fence one member as dispatched, durably, with its
/// Launch's handle (the command id), then send its Launch; a Launch with no
/// answer is fenced again and resent identically.
async fn dispatch(
    ctx: &GroupCtx,
    work: &InitializeWork,
    rank: u32,
    command: &MemberCommand,
) -> Result<pb::MemberExecutionResult, String> {
    let fence = work.fence();
    let mut last = String::from("no answer");
    for _ in 0..=LAUNCH_RESENDS {
        // The fence commits before anything is sent; a fence that cannot be
        // written sends nothing.
        locked(ctx, |o| {
            o.store()
                .mark_member_dispatching(
                    &fence.deployment_id,
                    work.instance_index(),
                    fence.generation,
                    rank,
                    &command.identity.command_id,
                )
                .map_err(|e| e.to_string())
        })
        .map_err(|e| e.to_string())?;
        match ctx.hosts.execute(command.clone()).await {
            Ok(result) => return Ok(result),
            Err(reason) => last = reason,
        }
        if capyctl_protocol::now_unix_ms() >= command.identity.deadline_ms {
            break;
        }
    }
    Err(last)
}

/// ADR 0028 §8, §9: every Launch at once. The head's reply is its native
/// readiness, bounded by `timeouts.initialize`; every member's reply is
/// watched for an exit, and the first member that fails ends the wait (the
/// group is stopped, ADR 0028 §11: a head never forms without it). Returns
/// the head's reply and identities, or the first rank that failed and why.
/// Every identity a reply reports is recorded for its member, gone ones too:
/// they are the host's journal of that Launch, which its stop settles on. A
/// member whose reply was lost stays dispatched and charged (R23); its
/// host's journal names its identities when it is stopped.
async fn launch_all(
    ctx: &GroupCtx,
    work: &InitializeWork,
    members: &[Member],
    launches: &[MemberCommand],
) -> Result<(pb::MemberExecutionResult, Vec<ProcessIdentity>), (u32, LaunchFailure, String)> {
    use futures::StreamExt;
    use LaunchFailure::{MemberFailed, MemberUncertain};
    let fence = work.fence();
    // ADR 0012: the head's ingress is provisioned before its Launch.
    ctx.hosts
        .provision_head(&launches[0])
        .await
        .map_err(|reason| {
            (
                0,
                MemberFailed,
                format!("the head's ingress was not provisioned: {reason}"),
            )
        })?;
    let initialize = Duration::from_millis(
        u64::try_from(members[0].resolution.effective.timeouts.initialize_ms).unwrap_or(0),
    );
    let left = Duration::from_millis(
        u64::try_from(work.deadline_ms() - capyctl_protocol::now_unix_ms()).unwrap_or(0),
    );
    let judge = |rank: u32, reply: Result<pb::MemberExecutionResult, String>| {
        let result = match reply {
            Ok(result) => result,
            Err(reason) => {
                // ADR 0028 §11: unreachable, charged, uncertain.
                let _ = locked(ctx, |o| {
                    o.store()
                        .mark_member_uncertain(
                            &fence.deployment_id,
                            work.instance_index(),
                            fence.generation,
                            rank,
                        )
                        .map_err(|e| e.to_string())
                });
                return Err((
                    rank,
                    MemberUncertain,
                    format!("its Launch went unanswered: {reason}"),
                ));
            }
        };
        if !result.refused.is_empty() {
            return Err((rank, MemberFailed, result.refused.clone()));
        }
        let reported: Vec<ProcessIdentity> = result.processes.iter().map(process).collect();
        if result.state == "launched" && result.claim_retained && !reported.is_empty() {
            locked(ctx, |o| {
                o.store()
                    .mark_member_launched(
                        &fence.deployment_id,
                        work.instance_index(),
                        fence.generation,
                        rank,
                        &reported,
                    )
                    .map_err(|e| e.to_string())
            })
            .map_err(|error| {
                (
                    rank,
                    MemberFailed,
                    format!("identities not recorded: {error}"),
                )
            })?;
        }
        let identities = alive(&result);
        if crate::agent_sessions::launch_ended_before_readiness(&result) || identities.is_empty() {
            return Err((rank, MemberFailed, "the member exited".into()));
        }
        if rank == 0 && !result.model_usable {
            return Err((0, MemberFailed, "the head did not prove readiness".into()));
        }
        Ok((rank == 0).then_some((result, identities)))
    };
    let mut sends: futures::stream::FuturesUnordered<_> = members
        .iter()
        .zip(launches)
        .map(|(member, command)| async move {
            (member.rank, dispatch(ctx, work, member.rank, command).await)
        })
        .collect();
    // ADR 0028 §9, owner decision 5: head readiness within `timeouts.initialize`.
    let failed = tokio::time::timeout(initialize.min(left), async {
        let mut head = None;
        while let Some((rank, reply)) = sends.next().await {
            match judge(rank, reply) {
                Ok(Some(found)) => head = Some(found),
                Ok(None) => {}
                Err(failed) => return Err(failed),
            }
        }
        head.ok_or((0, MemberFailed, "the head did not answer".to_owned()))
    })
    .await
    .unwrap_or_else(|_| {
        Err((
            0,
            MemberFailed,
            "the head did not become ready within timeouts.initialize".to_owned(),
        ))
    });
    let failed = match failed {
        Ok(head) => return Ok(head),
        Err(failed) => failed,
    };
    // The Launches still in flight get out to their hosts, and whatever
    // answers soon has its identities recorded, before the group is stopped.
    let _ = tokio::time::timeout(FAILED_LAUNCH_GRACE, async {
        while let Some((rank, reply)) = sends.next().await {
            let _ = judge(rank, reply);
        }
    })
    .await;
    Err(failed)
}

/// ADR 0028 §9 (decided 2026-10-06): one 1-token completion through the
/// head, then READY: the head's identities are the step's launch and the
/// binding on the head's ingress opens as the group's only replica.
async fn ready(
    ctx: &GroupCtx,
    work: &InitializeWork,
    plan: &GroupPlan,
    members: &[Member],
    launches: &[MemberCommand],
    head_reply: pb::MemberExecutionResult,
    head_identities: Vec<ProcessIdentity>,
) -> GroupActivation {
    let failed = |reason: String| GroupActivation::Failed {
        plan: plan.clone(),
        failed_rank: 0,
        failure: LaunchFailure::MemberFailed,
        reason,
    };
    let head = HeadLaunch {
        plan: plan.clone(),
        deployment_id: work.fence().deployment_id.clone(),
        revision: work.fence().revision,
        operation_id: work.operation_id().to_owned(),
        owned_handle: launches[0].identity.command_id.clone(),
        profile_fingerprint: members[0]
            .resolution
            .effective
            .profile
            .build_fingerprint
            .clone(),
    };
    if let Err(error) =
        probe_head(ctx, &head, READINESS_PROBE_TOKENS, READINESS_PROBE_DEADLINE).await
    {
        return failed(error.to_string());
    }
    // ADR 0028 §12 (decided 2026-10-06): the wake canary's reference is
    // recorded at first readiness, after the readiness probe; a recording
    // that fails leaves the group ready and its next wake records it.
    crate::group_residency::record_canary(ctx, &head, work.instance_index()).await;
    let step = work.step_id().to_owned();
    let observed_at = head_reply.observed_at_unix_ms;
    let receipt = OwnedLaunchReceipt {
        binding_id: work.binding_id().into(),
        incarnation: work.incarnation().into(),
        identities: head_identities.clone(),
        observed_at_ms: observed_at,
        receipt: "authenticated group head native readiness and completion probe".into(),
    };
    let ttl = work.policy().controls.observation_ttl_ms;
    let token = capyctl_domain::completion::TransitionToken {
        deployment_id: work.fence().deployment_id.clone(),
        revision: work.fence().revision,
        generation: work.fence().generation,
        operation_id: work.operation_id().to_owned(),
        step_id: step.clone(),
    };
    let evidence = CompletionEvidence {
        token,
        identities: head_identities,
        observed_at_ms: observed_at,
        control_receipt: Some(receipt.receipt.clone()),
        milestones: vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ],
    };
    // SPEC §13.2 (G2): readiness belongs to the head's current session.
    ctx.hosts
        .head_ready(work.binding_id(), &plan.head().member.host_id);
    let completed = locked(ctx, |o| {
        let now = (ctx.clock)().map_err(|e| e.to_string())?;
        o.store()
            .record_owned_launch(o.session(), &step, &receipt, now)
            .map_err(|e| e.to_string())?;
        o.store()
            .complete_step(o.session(), &step, &evidence, now, ttl)
            .map_err(|e| e.to_string())
    });
    match completed {
        Ok(()) => GroupActivation::Ready { plan: plan.clone() },
        Err(error) => failed(format!("the group's readiness was not recorded: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(entries: &[(&str, &str)]) -> BTreeMap<String, PathBuf> {
        entries
            .iter()
            .map(|(h, p)| ((*h).to_owned(), PathBuf::from(p)))
            .collect()
    }

    // T14 (decided 2026-10-06): only a deep SGLang group needs one model path.
    #[test]
    fn member_paths_must_agree_only_for_a_deep_sglang_group() {
        let differ = paths(&[("host-a", "/models/a"), ("host-b", "/models/b")]);
        let error = check_member_paths(GroupEngine::Sglang, Residency::Deep, "host-a", &differ)
            .unwrap_err();
        assert_eq!(error.code(), "group_model_path_mismatch");
        let text = error.to_string();
        assert!(
            text.contains("/models/a") && text.contains("/models/b"),
            "{text}"
        );
        for (engine, residency) in [
            (GroupEngine::Sglang, Residency::RestartOnly),
            (GroupEngine::Vllm, Residency::Deep),
            (GroupEngine::Tensorfold, Residency::RestartOnly),
        ] {
            check_member_paths(engine, residency, "host-a", &differ).unwrap();
        }
        let same = paths(&[("host-a", "/models/a"), ("host-b", "/models/a")]);
        check_member_paths(GroupEngine::Sglang, Residency::Deep, "host-a", &same).unwrap();
    }
}

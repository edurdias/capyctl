//! ADR 0028 §12: parking and waking a multi-node engine group.
//!
//! Park and wake follow the deployment's effective residency, never the
//! engine (R34, [`GroupResidency`]). A `restart_only` group, on any engine
//! (every TensorFold group: its `group_support` admits nothing else), parks
//! by a group stop and wakes by a group relaunch: the store turns its park
//! into an ordinary stop of the instance, whose cleanup is the group stop
//! (`group_settlement::stop_group`), and its wake is a new activation
//! (`group_activation::activate_group`). A `deep` group (vLLM and SGLang)
//! parks and wakes here, as the effect of its armed park or restore step:
//!
//! - **Park.** The head's agent alone is sent `Park` (member id `head`), once:
//!   it invokes the engine's collective (`/sleep`, `/release_memory_occupation`)
//!   through the head's loopback control endpoint. The park settles only when
//!   every member's own host then reports the member at or below its parked
//!   budget: a vLLM member through its host's `process_residency` for the
//!   member's recorded processes, a SGLang member through the saver-map facts
//!   its host reads from its own observation directory (R12; never a fallback
//!   to process sampling). A member's charge moves to its parked budget only
//!   on its own report.
//! - **Wake.** The head's agent alone is sent `Restore`, once; every member's
//!   own host must report it resident again; the head must pass the 1-token
//!   readiness probe; and the canary (an 8-token completion through the head
//!   at temperature 0) must match the reference recorded at the plan's first
//!   readiness, judged by [`canary_matches`] alone.
//!
//! The collective is never repeated. A member that reports nothing by the
//! step's deadline keeps its full charge and is `uncertain`
//! (`group_member_uncertain`); a member still resident after a park, not
//! resident after a wake, a head that fails its readiness, or a canary that
//! differs (`group_member_failed`, `group_wake_mismatch`) fails the group.
//! Either stops the group, each member released only on its own host's
//! evidence (ADR 0028 §11). CPU and fake-engine tests only cover this; the
//! live rows MN1–MN9 qualify it.
use crate::group_activation::{
    probe_head, GroupCtx, HeadLaunch, READINESS_PROBE_DEADLINE, READINESS_PROBE_TOKENS,
};
use crate::group_settlement::GroupTarget;
use capyctl_config::effective::Residency;
use capyctl_domain::group::{member_id, CommandIdentity, GroupEngine, MemberKey};
use capyctl_protocol::{
    execution::{MemberAction, MemberCommand},
    pb,
};
use capyctl_store::{
    groups::StoredCanary,
    ordinary_lifecycle::park::{ArmedMember, MemberResidency, ResidencyKind},
};
use std::{collections::BTreeMap, time::Duration};

/// ADR 0028 §12 (decided 2026-10-06): the wake canary asks the head for this
/// many tokens, at temperature 0, through the same probe as readiness.
pub const CANARY_TOKENS: u32 = 8;
/// ADR 0028 §12: the canary probe's bound.
pub const CANARY_DEADLINE: Duration = READINESS_PROBE_DEADLINE;
/// How often each member's own host is asked again for its report while a
/// park or wake waits for it.
pub const MEMBER_EVIDENCE_POLL: Duration = Duration::from_millis(100);
/// Time kept back from the step's deadline to record its outcome.
pub const EVIDENCE_MARGIN: Duration = Duration::from_millis(500);

/// ADR 0028 §12 (decided 2026-10-06, revisited at MN4 with live evidence):
/// the rule a wake's canary is judged by. Today one rule exists, exact token
/// equality: the woken group generates, at temperature 0, exactly the tokens
/// it generated for the same prompt at first readiness. A tolerance rule
/// later is a new variant here and one arm in [`canary_matches`], nothing
/// else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryRule {
    ExactTokens,
}

/// The canary rule in force (see [`CanaryRule`]).
pub const CANARY_RULE: CanaryRule = CanaryRule::ExactTokens;

/// ADR 0028 §12: the canary's reference, recorded at the plan's first
/// readiness: the head probe's fixed prompt and the tokens it generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanaryReference {
    pub prompt: String,
    pub tokens: Vec<u32>,
}

/// ADR 0028 §12 (decided 2026-10-06): the one comparison of a wake's canary
/// with its reference, under [`CANARY_RULE`].
pub fn canary_matches(reference: &CanaryReference, observed: &[u32]) -> bool {
    match CANARY_RULE {
        CanaryRule::ExactTokens => reference.tokens == observed,
    }
}

/// ADR 0028 §12 (R34): how a group parks and wakes, from its effective
/// residency alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupResidency {
    /// The head's collective, each member settled on its own host's report.
    Deep,
    /// A group stop, and a group relaunch.
    RestartOnly,
}

impl GroupResidency {
    /// The group's residency: `restart_only` when any member's own host
    /// resolved it so, `deep` otherwise.
    pub fn of(members: &[ArmedMember]) -> Self {
        if members
            .iter()
            .any(|m| m.residency == Residency::RestartOnly)
        {
            Self::RestartOnly
        } else {
            Self::Deep
        }
    }
}

/// Why a group park or wake did not settle. Every variant but `Refused`
/// stops the group (ADR 0028 §11, §12).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GroupResidencyError {
    /// The member's own host reported nothing by the deadline (the head's
    /// collective unanswered included, as rank 0): its charge stays whole.
    #[error("group_member_uncertain: rank {rank} reported nothing of its own")]
    MemberSilent { rank: u32 },
    /// Review Focus 4: the head's call succeeded but this rank still holds
    /// its memory after the park.
    #[error("group_member_failed: rank {rank} is still resident after the park")]
    MemberResident { rank: u32 },
    /// The rank's memory did not come back after the wake.
    #[error("group_member_failed: rank {rank} is not resident after the wake")]
    MemberAsleep { rank: u32 },
    /// The woken head failed its readiness probe.
    #[error("group_member_failed: the head is not ready after the wake: {0}")]
    HeadNotReady(String),
    /// The canary's tokens differ from the reference.
    #[error("group_wake_mismatch: the canary differs from its reference")]
    CanaryMismatch,
    /// The head refused before any effect: nothing changed.
    #[error("{code}: {reason}")]
    Refused { code: String, reason: String },
}

impl GroupResidencyError {
    /// The closed code (spec §16).
    pub fn code(&self) -> &str {
        match self {
            Self::MemberSilent { .. } => "group_member_uncertain",
            Self::MemberResident { .. } | Self::MemberAsleep { .. } | Self::HeadNotReady(_) => {
                "group_member_failed"
            }
            Self::CanaryMismatch => "group_wake_mismatch",
            Self::Refused { code, .. } => code,
        }
    }

    /// The rank the group failed at, for a failure (not an uncertainty or a
    /// refusal).
    pub fn failed_rank(&self) -> Option<u32> {
        match self {
            Self::MemberResident { rank } | Self::MemberAsleep { rank } => Some(*rank),
            Self::HeadNotReady(_) | Self::CanaryMismatch => Some(0),
            Self::MemberSilent { .. } | Self::Refused { .. } => None,
        }
    }
}

/// One armed group park or wake: the group, its armed members, the head's
/// launch and the step it runs under.
#[derive(Debug, Clone)]
pub struct ResidencyTarget {
    pub group: GroupTarget,
    pub members: Vec<ArmedMember>,
    pub step_id: String,
    pub deadline_ms: i64,
}

impl ResidencyTarget {
    fn head(&self) -> Result<HeadLaunch, GroupResidencyError> {
        let head = self.group.plan.head();
        let owned_handle = self
            .members
            .iter()
            .find(|m| m.rank == 0)
            .and_then(|m| m.launch_handle.clone())
            .ok_or(GroupResidencyError::MemberSilent { rank: 0 })?;
        Ok(HeadLaunch {
            plan: self.group.plan.clone(),
            deployment_id: self.group.deployment_id.clone(),
            revision: self.group.revision,
            operation_id: self.group.operation_id.clone(),
            owned_handle,
            profile_fingerprint: head.profile_fingerprint.clone(),
        })
    }
}

/// ADR 0028 §12, SPEC §11: the lead agent invokes each collective once. A
/// `deep` group parks: the head's agent alone is sent `Park`, once, then
/// every member's own host must report it at or below its parked budget
/// before the step's deadline. Returns each member's report. A
/// `restart_only` group never reaches here: its park is a group stop (the
/// store accepts it as one, as do switching and the idle policy).
pub async fn park_group(
    ctx: &GroupCtx,
    target: &ResidencyTarget,
) -> Result<Vec<MemberResidency>, GroupResidencyError> {
    if GroupResidency::of(&target.members) == GroupResidency::RestartOnly {
        return Err(restart_only(ResidencyKind::Park));
    }
    let since = (ctx.clock)().unwrap_or_default();
    // ADR 0028 §12, SPEC §11: the lead agent invokes each collective once.
    let reply = head_residency(ctx, target, ResidencyKind::Park).await?;
    expect(&reply, ResidencyKind::Park)?;
    collect(ctx, target, ResidencyKind::Park, since).await
}

/// ADR 0028 §12: a `deep` group wakes: the head's agent alone is sent
/// `Restore`, once; every member's own host must report it resident; the
/// head must pass its readiness probe; and the canary must match the
/// reference recorded at first readiness. With no reference recorded (its
/// recording failed at first readiness) this wake records one and passes.
pub async fn wake_group(
    ctx: &GroupCtx,
    target: &ResidencyTarget,
) -> Result<Vec<MemberResidency>, GroupResidencyError> {
    if GroupResidency::of(&target.members) == GroupResidency::RestartOnly {
        return Err(restart_only(ResidencyKind::Restore));
    }
    let since = (ctx.clock)().unwrap_or_default();
    // ADR 0028 §12, SPEC §11: the lead agent invokes each collective once.
    // ADR 0028 §12 (decided 2026-10-06): SGLang's wake posts the head's model
    // path to every rank and each rank loads it from its own disk, which is
    // why a deep SGLang group whose members' paths differ is refused at
    // activation (`group_model_path_mismatch`, `check_member_paths`).
    let reply = head_residency(ctx, target, ResidencyKind::Restore).await?;
    expect(&reply, ResidencyKind::Restore)?;
    let reports = collect(ctx, target, ResidencyKind::Restore, since).await?;
    let head = target.head()?;
    probe_head(ctx, &head, READINESS_PROBE_TOKENS, READINESS_PROBE_DEADLINE)
        .await
        .map_err(|error| GroupResidencyError::HeadNotReady(error.to_string()))?;
    let observed = probe_head(ctx, &head, CANARY_TOKENS, CANARY_DEADLINE)
        .await
        .map_err(|error| GroupResidencyError::HeadNotReady(error.to_string()))?;
    match canary_reference(ctx, &target.group) {
        Some(reference) if !canary_matches(&reference, &observed) => {
            Err(GroupResidencyError::CanaryMismatch)
        }
        Some(_) => Ok(reports),
        None => {
            // The first readiness recorded none: this wake's output becomes
            // the reference, as at first readiness.
            store_canary(ctx, &target.group, observed);
            Ok(reports)
        }
    }
}

/// ADR 0028 §12 (decided 2026-10-06): record the canary's reference at the
/// plan's first readiness, after the 1-token readiness probe: one 8-token
/// completion through the head at temperature 0. A probe or a store write
/// that fails records nothing and the group stays ready; its next wake
/// records the reference instead of comparing.
pub async fn record_canary(ctx: &GroupCtx, head: &HeadLaunch, instance_index: u32) {
    let Ok(tokens) = probe_head(ctx, head, CANARY_TOKENS, CANARY_DEADLINE).await else {
        return;
    };
    let target = GroupTarget {
        deployment_id: head.deployment_id.clone(),
        instance_index,
        revision: head.revision,
        operation_id: head.operation_id.clone(),
        plan: head.plan.clone(),
    };
    store_canary(ctx, &target, tokens);
}

fn store_canary(ctx: &GroupCtx, target: &GroupTarget, tokens: Vec<u32>) {
    if let Ok(o) = ctx.owner.lock() {
        let _ = o.store().record_canary_reference(
            &target.deployment_id,
            target.instance_index,
            target.plan.generation(),
            &StoredCanary {
                prompt: capyctl_adapters::completion_probe::PROMPT.to_owned(),
                tokens,
            },
        );
    }
}

fn canary_reference(ctx: &GroupCtx, target: &GroupTarget) -> Option<CanaryReference> {
    let o = ctx.owner.lock().ok()?;
    o.store()
        .canary_reference(
            &target.deployment_id,
            target.instance_index,
            target.plan.generation(),
        )
        .ok()
        .flatten()
        .map(|stored| CanaryReference {
            prompt: stored.prompt,
            tokens: stored.tokens,
        })
}

fn restart_only(kind: ResidencyKind) -> GroupResidencyError {
    GroupResidencyError::Refused {
        code: format!("{}_refused", kind.operation_kind()),
        reason: "a restart_only group parks by a group stop and wakes by a group relaunch".into(),
    }
}

/// The head's one `Park` or `Restore`, under the step's own id (an identical
/// resend replays on the head's agent; this sends it once).
async fn head_residency(
    ctx: &GroupCtx,
    target: &ResidencyTarget,
    kind: ResidencyKind,
) -> Result<pb::MemberExecutionResult, GroupResidencyError> {
    let head = target.head()?;
    let member = target.group.plan.head();
    let owned_handle = head.owned_handle.clone();
    let mut command = MemberCommand {
        identity: CommandIdentity {
            controller_id: ctx.hosts.controller_id(),
            member: MemberKey {
                host_id: member.member.host_id.clone(),
                member_id: member_id(0),
            },
            deployment_id: target.group.deployment_id.clone(),
            operation_id: target.group.operation_id.clone(),
            command_id: target.step_id.clone(),
            step_id: target.step_id.clone(),
            generation: target.group.plan.generation(),
            revision: target.group.revision,
            deadline_ms: target.deadline_ms,
            payload_digest: [0; 32],
            expected_state: match kind {
                ResidencyKind::Park => "ready",
                ResidencyKind::Restore => "parked",
            }
            .into(),
            profile_fingerprint: head.profile_fingerprint.clone(),
            // ADR 0028 §2: a group has one instance.
            instance_index: 0,
        },
        action: match kind {
            ResidencyKind::Park => MemberAction::Park { owned_handle },
            ResidencyKind::Restore => MemberAction::Restore {
                owned_handle,
                // Task 13: the plan's checkpoint fingerprint is the agreed
                // checkpoint digest every member measured.
                checkpoint_digest: member.checkpoint_fingerprint.clone(),
            },
        },
    };
    command.identity.payload_digest = command.canonical_digest();
    let bound = remaining(ctx, target.deadline_ms);
    match tokio::time::timeout(bound, ctx.hosts.execute(command)).await {
        Ok(Ok(reply)) => Ok(reply),
        // Unanswered: the collective's outcome is unknown; nothing is
        // resent (SPEC §13.2).
        _ => Err(GroupResidencyError::MemberSilent { rank: 0 }),
    }
}

/// What the head's reply proves: the collective ran (`parked` or
/// `restored`), it was refused before any effect (`unchanged`), or its
/// outcome is unknown (the head's member is then uncertain).
fn expect(
    reply: &pb::MemberExecutionResult,
    kind: ResidencyKind,
) -> Result<(), GroupResidencyError> {
    let state = reply.residency.as_ref().map(|r| r.state.as_str());
    match (kind, state) {
        (ResidencyKind::Park, Some("parked"))
            if reply.state == "completed" && reply.claim_retained && !reply.model_usable =>
        {
            Ok(())
        }
        (ResidencyKind::Restore, Some("restored"))
            if reply.state == "completed" && reply.claim_retained && reply.model_usable =>
        {
            Ok(())
        }
        (_, Some("unchanged")) if reply.state == "completed" => Err(GroupResidencyError::Refused {
            code: format!("{}_refused", kind.operation_kind()),
            reason: if reply.refused.is_empty() {
                "the head refused before any effect".into()
            } else {
                format!("the head refused before any effect ({})", reply.refused)
            },
        }),
        _ => Err(GroupResidencyError::MemberSilent { rank: 0 }),
    }
}

fn remaining(ctx: &GroupCtx, deadline_ms: i64) -> Duration {
    let now = (ctx.clock)().unwrap_or(deadline_ms);
    Duration::from_millis(u64::try_from(deadline_ms.saturating_sub(now)).unwrap_or(0))
}

/// ADR 0028 §12 (R12): every member's own host's report, asked again until
/// each one settles the transition or the step's deadline (less the time to
/// record it) passes. A park needs every member at or below its parked
/// budget, a wake every member above it. At the deadline a member that
/// reported but did not settle fails the group; one that reported nothing
/// is silent.
async fn collect(
    ctx: &GroupCtx,
    target: &ResidencyTarget,
    kind: ResidencyKind,
    since: i64,
) -> Result<Vec<MemberResidency>, GroupResidencyError> {
    let engine = target.group.plan.engine();
    let settles = |report: &MemberResidency, member: &ArmedMember| match kind {
        ResidencyKind::Park => report.released(member),
        ResidencyKind::Restore => report.resident(member),
    };
    let bound = remaining(ctx, target.deadline_ms).saturating_sub(EVIDENCE_MARGIN);
    let until = tokio::time::Instant::now() + bound;
    let mut latest: BTreeMap<u32, MemberResidency> = BTreeMap::new();
    loop {
        let pending: Vec<&ArmedMember> = target
            .members
            .iter()
            .filter(|m| !latest.get(&m.rank).is_some_and(|r| settles(r, m)))
            .collect();
        if pending.is_empty() {
            return Ok(target
                .members
                .iter()
                .filter_map(|m| latest.remove(&m.rank))
                .collect());
        }
        let reports = futures::future::join_all(
            pending
                .iter()
                .map(|m| member_report(ctx, &target.group.deployment_id, engine, m, since)),
        )
        .await;
        for report in reports.into_iter().flatten() {
            latest.insert(report.rank, report);
        }
        if target
            .members
            .iter()
            .all(|m| latest.get(&m.rank).is_some_and(|r| settles(r, m)))
        {
            continue;
        }
        if tokio::time::Instant::now() >= until {
            // Review Focus 4: a member proven still holding (or not holding)
            // its memory fails the group; otherwise the first silent one.
            let unsettled = target.members.iter().find(|m| {
                latest
                    .get(&m.rank)
                    .is_some_and(|report| !settles(report, m))
            });
            return Err(match (unsettled, kind) {
                (Some(m), ResidencyKind::Park) => {
                    GroupResidencyError::MemberResident { rank: m.rank }
                }
                (Some(m), ResidencyKind::Restore) => {
                    GroupResidencyError::MemberAsleep { rank: m.rank }
                }
                (None, _) => GroupResidencyError::MemberSilent {
                    rank: target
                        .members
                        .iter()
                        .find(|m| !latest.contains_key(&m.rank))
                        .map_or(0, |m| m.rank),
                },
            });
        }
        tokio::time::sleep(MEMBER_EVIDENCE_POLL.min(until - tokio::time::Instant::now())).await;
    }
}

/// ADR 0028 §12 (R12): one member's residency, reported by its own host only
/// and observed after the collective was sent. vLLM: the memory the host's
/// `process_residency` attributes to the member's recorded processes (a
/// sample naming none of them is no report). SGLang: the saver-map facts the
/// host reads from its own observation directory for the member; never
/// process sampling. TensorFold never parks.
async fn member_report(
    ctx: &GroupCtx,
    deployment_id: &str,
    engine: GroupEngine,
    member: &ArmedMember,
    since: i64,
) -> Option<MemberResidency> {
    let (resident_bytes, observed_at_ms) = match engine {
        GroupEngine::Vllm => {
            let (observed, residents) = tokio::time::timeout(
                MEMBER_EVIDENCE_POLL.max(Duration::from_secs(1)),
                ctx.observations
                    .observe_with_residents(member.host_id.clone()),
            )
            .await
            .ok()?
            .ok()?;
            let observed_at = observed.iter().map(|o| o.sampled_at_ms).min()?;
            let own: Vec<_> = residents
                .iter()
                .filter(|r| {
                    member.identities.iter().any(|p| {
                        p.pid == r.pid && p.boot_id == r.boot_id && p.start_ticks == r.start_ticks
                    })
                })
                .collect();
            if own.is_empty() {
                return None;
            }
            (
                own.iter().map(|r| r.bytes).fold(0_i64, i64::saturating_add),
                observed_at,
            )
        }
        GroupEngine::Sglang => ctx.hosts.saver_mapped(deployment_id, member).await.ok()?,
        GroupEngine::Tensorfold => return None,
    };
    (observed_at_ms >= since).then(|| MemberResidency {
        rank: member.rank,
        host_id: member.host_id.clone(),
        resident_bytes,
        observed_at_ms,
    })
}

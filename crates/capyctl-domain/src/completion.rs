use std::collections::BTreeSet;

use crate::resources::{validate_footprint, PhaseFootprint, ResourcePhase};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedLaunchReceipt {
    pub binding_id: String,
    pub incarnation: String,
    pub identities: Vec<ProcessIdentity>,
    pub observed_at_ms: i64,
    pub receipt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupEvidence {
    pub binding_id: String,
    pub incarnation: String,
    pub identities: Vec<ProcessIdentity>,
    pub observed_at_ms: i64,
    pub receipt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepExecutionContext {
    pub token: TransitionToken,
    pub binding_id: String,
    pub incarnation: String,
    pub issued_at_ms: i64,
    pub deadline_ms: i64,
    pub identities: ExecutionIdentities,
    pub completion_target: Option<PhaseFootprint>,
    pub grant_id: Option<String>,
    pub launch_settings: Option<crate::launch::LaunchSettings>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionIdentities {
    Retained(Vec<ProcessIdentity>),
    OwnedLaunch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionToken {
    pub deployment_id: String,
    pub revision: i64,
    pub generation: i64,
    pub operation_id: String,
    pub step_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProcessIdentity {
    pub role: String,
    pub pid: u32,
    pub boot_id: String,
    pub start_ticks: u64,
}

/// ADR 0027: the role prefix of a helper. A helper is a process in the launch's
/// group that the engine's own processes did not start directly, such as a
/// torch inductor compile worker. It is recorded, so cleanup terminates it and
/// proves it gone, but its exit is not the engine's exit.
pub const HELPER_ROLE_PREFIX: &str = "helper-";

/// ADR 0033 §3: the role of a launch's LMCache server, the leader of its own
/// owned process group beside the engine's.
pub const LMCACHE_ROLE: &str = "lmcache";

/// ADR 0033 §3: the role prefix of any other process of the server's group
/// (`lmcache-N`, N a decimal number, numbered by start order).
pub const LMCACHE_MEMBER_PREFIX: &str = "lmcache-";

/// ADR 0035 §6: the role prefix of a rank process of a launch on several GPUs
/// of one host (`rank-N`, N the rank: the position of its GPU in the
/// launch's device list) that is neither `api` nor a `worker-N`. It is an
/// engine process: its exit is the engine's.
pub const RANK_ROLE_PREFIX: &str = "rank-";

impl ProcessIdentity {
    /// ADR 0027: whether this recorded process is a helper rather than one of
    /// the engine's own processes (`api` and `worker-N`). A helper's role is
    /// `helper-N`, with N a decimal number.
    pub fn is_helper(&self) -> bool {
        is_helper_role(&self.role)
    }

    /// ADR 0033 §3: whether this recorded process belongs to the launch's
    /// LMCache server group (`lmcache` or `lmcache-N`).
    pub fn is_lmcache(&self) -> bool {
        is_lmcache_role(&self.role)
    }

    /// ADR 0033 §3: whether this recorded process is one of the engine's own
    /// processes: neither a helper nor a member of the LMCache server group.
    pub fn is_engine(&self) -> bool {
        !self.is_helper() && !self.is_lmcache()
    }

    /// ADR 0035 §6: the rank a `rank-N` role names.
    pub fn rank_role(&self) -> Option<u32> {
        rank_role(&self.role)
    }

    /// Whether `other` is the same kernel process (pid, boot id and start
    /// ticks), whatever role either names it by.
    pub fn same_process(&self, other: &ProcessIdentity) -> bool {
        self.pid == other.pid
            && self.boot_id == other.boot_id
            && self.start_ticks == other.start_ticks
    }
}

/// ADR 0035 §6: the rank of a `rank-N` role (N a decimal number).
pub fn rank_role(role: &str) -> Option<u32> {
    role.strip_prefix(RANK_ROLE_PREFIX)
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
}

/// ADR 0027: whether `role` is a helper's (`helper-N`, N a decimal number).
pub fn is_helper_role(role: &str) -> bool {
    role.strip_prefix(HELPER_ROLE_PREFIX)
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// ADR 0033 §3: whether `role` names the LMCache server (`lmcache`) or another
/// process of its group (`lmcache-N`, N a decimal number).
pub fn is_lmcache_role(role: &str) -> bool {
    role == LMCACHE_ROLE
        || role
            .strip_prefix(LMCACHE_MEMBER_PREFIX)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// ADR 0027, ADR 0033 §3: the engine's own processes of a recorded launch,
/// without helpers and without the LMCache server's group.
pub fn engine_members(identities: &[ProcessIdentity]) -> Vec<ProcessIdentity> {
    identities
        .iter()
        .filter(|identity| identity.is_engine())
        .cloned()
        .collect()
}

/// ADR 0033 §3: the LMCache server's group of a recorded launch.
pub fn lmcache_members(identities: &[ProcessIdentity]) -> Vec<ProcessIdentity> {
    identities
        .iter()
        .filter(|identity| identity.is_lmcache())
        .cloned()
        .collect()
}

/// ADR 0027: whether `observed` (the owned processes of a launch alive now) is
/// still the engine `recorded` names: every engine process of `recorded` is
/// there, and every engine process there is one `recorded` names. Helpers do
/// not decide it: one that exited since, or one the engine started later, does
/// not change the engine. ADR 0033 §3: neither does the LMCache server's group
/// (`same_lmcache` judges it).
///
/// ADR 0035 §6: a recorded `rank-N` process is named by readiness from the
/// per-GPU readings, not by the group scan, which names it a helper or a
/// worker again. Ranks are identified by kernel facts, never by names: an
/// observed process that is the same kernel process as a recorded rank is
/// that rank, so every proof requires each rank alive.
pub fn same_engine(recorded: &[ProcessIdentity], observed: &[ProcessIdentity]) -> bool {
    let observed = with_recorded_ranks(recorded, observed);
    let observed = observed.as_slice();
    let recorded_set = recorded.iter().collect::<BTreeSet<_>>();
    let observed_set = observed.iter().collect::<BTreeSet<_>>();
    recorded.iter().any(ProcessIdentity::is_engine)
        && observed_set.len() == observed.len()
        && observed
            .iter()
            .filter(|identity| identity.is_engine())
            .all(|identity| recorded_set.contains(identity))
        && recorded
            .iter()
            .filter(|identity| identity.is_engine())
            .all(|identity| observed_set.contains(identity))
}

/// ADR 0035 §6: `observed` with every process that is the same kernel process
/// as a recorded `rank-N` named by that recorded role.
pub fn with_recorded_ranks(
    recorded: &[ProcessIdentity],
    observed: &[ProcessIdentity],
) -> Vec<ProcessIdentity> {
    observed
        .iter()
        .map(|identity| {
            match recorded
                .iter()
                .find(|r| r.rank_role().is_some() && r.same_process(identity))
            {
                Some(rank) => rank.clone(),
                None => identity.clone(),
            }
        })
        .collect()
}

/// ADR 0033 §3: whether `observed` still holds the LMCache server `recorded`
/// names. The server is its group's leader (`lmcache`): exactly the recorded
/// leader is there, by identity, and no other. Any other process of its
/// group (`lmcache-N`) may have exited or started since, as a helper of the
/// engine may (ADR 0027; found live on SGLang: a short-lived child recorded
/// at Ready exited 90 s later); the group's ownership and its verified stop
/// still cover every member recorded.
pub fn same_lmcache(recorded: &[ProcessIdentity], observed: &[ProcessIdentity]) -> bool {
    let leader = |set: &[ProcessIdentity]| {
        let members = lmcache_members(set);
        let unique = members.iter().collect::<BTreeSet<_>>().len() == members.len();
        let mut leaders = members.into_iter().filter(|i| i.role == LMCACHE_ROLE);
        match (unique, leaders.next(), leaders.next()) {
            (true, Some(leader), None) => Some(leader),
            _ => None,
        }
    };
    matches!((leader(recorded), leader(observed)), (Some(a), Some(b)) if a == b)
}

/// Whether a recorded process still exists, judged by pid, boot id and start ticks
/// together. A pid alone is not an identity: the kernel reuses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// The exact recorded process still exists. Never release.
    Alive,
    /// Proven absent: the boot differs, the pid is unused, or the pid was reused by
    /// a different process. Only this authorises release.
    Gone,
    /// Could not be established. Treated as retained, never as absent.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Milestone {
    Quiesced,
    MemoryReleased,
    AllocationsRestored,
    WeightsUsable,
    CacheValid,
    ModelUsable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionExpectation {
    pub token: TransitionToken,
    pub identities: Vec<ProcessIdentity>,
    pub target: PhaseFootprint,
    pub issued_at_ms: i64,
    pub deadline_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionEvidence {
    pub token: TransitionToken,
    pub identities: Vec<ProcessIdentity>,
    pub observed_at_ms: i64,
    pub control_receipt: Option<String>,
    pub milestones: Vec<Milestone>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCompletion {
    token: TransitionToken,
    target: PhaseFootprint,
    evidence: CompletionEvidence,
    observed_at_ms: i64,
    valid_until_ms: i64,
}

impl VerifiedCompletion {
    pub fn token(&self) -> &TransitionToken {
        &self.token
    }

    pub fn target(&self) -> &PhaseFootprint {
        &self.target
    }

    pub fn evidence(&self) -> &CompletionEvidence {
        &self.evidence
    }

    pub fn observed_at_ms(&self) -> i64 {
        self.observed_at_ms
    }

    pub fn valid_until_ms(&self) -> i64 {
        self.valid_until_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CompletionError {
    #[error("invalid completion expectation")]
    Invalid,
    #[error("completion token is stale or mismatched")]
    StaleToken,
    #[error("runtime process identity changed or is incomplete")]
    RuntimeChanged,
    #[error("completion evidence is outside its time bounds")]
    Expired,
    #[error("completion milestones or control acknowledgement are missing")]
    Incomplete,
}

/// An api process plus its workers, or (ADR 0023 §6: TensorFold serves from one
/// process) the api process alone.
fn identities_valid(identities: &[ProcessIdentity]) -> bool {
    crate::group::validate_local_processes(identities).is_ok()
        && identities.iter().any(|identity| identity.role == "api")
        && (identities.len() == 1
            || identities
                .iter()
                .any(|identity| identity.role.starts_with("worker-") && identity.role.len() > 7))
}

pub fn verify_completion(
    expected: &CompletionExpectation,
    evidence: &CompletionEvidence,
    now_ms: i64,
    ttl_ms: i64,
) -> Result<VerifiedCompletion, CompletionError> {
    let token = &expected.token;
    if token.deployment_id.is_empty()
        || token.operation_id.is_empty()
        || token.step_id.is_empty()
        || token.revision < 1
        || token.generation < 1
        || expected.issued_at_ms < 0
        || expected.deadline_ms < expected.issued_at_ms
        || ttl_ms <= 0
        || validate_footprint(&expected.target).is_err()
    {
        return Err(CompletionError::Invalid);
    }
    if token != &evidence.token {
        return Err(CompletionError::StaleToken);
    }
    if !identities_valid(&expected.identities)
        || !identities_valid(&evidence.identities)
        || expected.identities.iter().collect::<BTreeSet<_>>()
            != evidence.identities.iter().collect::<BTreeSet<_>>()
    {
        return Err(CompletionError::RuntimeChanged);
    }
    let expiry = evidence
        .observed_at_ms
        .checked_add(ttl_ms)
        .ok_or(CompletionError::Expired)?
        .min(expected.deadline_ms);
    if evidence.observed_at_ms < expected.issued_at_ms
        || now_ms < evidence.observed_at_ms
        || now_ms > expiry
    {
        return Err(CompletionError::Expired);
    }
    if evidence
        .control_receipt
        .as_ref()
        .is_none_or(|receipt| receipt.is_empty())
    {
        return Err(CompletionError::Incomplete);
    }
    let required: &[Milestone] = match expected.target.phase {
        ResourcePhase::Parked => &[Milestone::Quiesced, Milestone::MemoryReleased],
        ResourcePhase::Ready => &[
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ],
        _ => return Err(CompletionError::Invalid),
    };
    if evidence.milestones.as_slice() != required {
        return Err(CompletionError::Incomplete);
    }
    Ok(VerifiedCompletion {
        token: token.clone(),
        target: expected.target.clone(),
        evidence: evidence.clone(),
        observed_at_ms: evidence.observed_at_ms,
        valid_until_ms: expiry,
    })
}

/// Observation data shared with trusted collectors. These values confer no authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectObservation {
    pub token: TransitionToken,
    pub binding_id: String,
    pub incarnation: String,
    pub identities: Vec<ProcessIdentity>,
    pub observed_at_ms: i64,
    pub receipt: String,
    pub facts: Vec<Milestone>,
    /// ADR 0014 amendment A12: when the engine's process group ran a kernel
    /// build during this step, in the host's clock. Empty when none was seen
    /// or the launcher cannot see one.
    pub kernel_builds: Vec<KernelBuild>,
}

/// ADR 0014 amendment A12: a span of host time, in Unix milliseconds, during
/// which a compiler ran in an engine's process group. It may start before the
/// compiler did and end after it left (the watcher's poll brackets it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelBuild {
    pub from_ms: i64,
    pub until_ms: i64,
}

impl KernelBuild {
    /// Whether a host sample taken at `at_ms` falls inside this build.
    pub fn covers(&self, at_ms: i64) -> bool {
        self.from_ms <= at_ms && at_ms <= self.until_ms
    }
}

/// Local parked-state observation; no engine command or inference request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedStatusObservation {
    pub token: TransitionToken,
    pub binding_id: String,
    pub incarnation: String,
    pub identities: Vec<ProcessIdentity>,
    pub observed_at_ms: i64,
    pub receipt: String,
    pub allocations: bool,
    pub weights: bool,
    pub cache: bool,
    pub quiesced: bool,
    pub unknown_work: bool,
    pub activity_before: (u64, u64, u64),
    pub activity_after: (u64, u64, u64),
}

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

fn identities_valid(identities: &[ProcessIdentity]) -> bool {
    crate::group::validate_local_processes(identities).is_ok()
        && identities.iter().any(|identity| identity.role == "api")
        && identities.iter().any(|identity| identity.role.starts_with("worker-") && identity.role.len() > 7)
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

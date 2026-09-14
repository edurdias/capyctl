//! Observation data shared with trusted collectors. These values confer no authority.
use crate::completion::{Milestone, ProcessIdentity, TransitionToken};

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
pub struct CandidateParkedStatusObservation {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CandidateTerminal {
    Completed,
    RejectedWithoutWork,
    FailedTerminal,
    Uncertain,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CandidateResponseObservation {
    Nonstreaming {
        model: String,
        content: String,
        finish_reason: String,
    },
    Streaming {
        chunks: Vec<CandidateStreamChunk>,
        completed: bool,
    },
    SecurityRejection {
        endpoint: CandidateSecurityEndpoint,
        status: u16,
        no_work: bool,
        separate_credentials: bool,
    },
    NoResponse,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateSecurityEndpoint {
    AdminControl,
    Inference,
    HealthGeneration,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateSecurityControlObservation {
    pub effect: EffectObservation,
    pub terminal: CandidateTerminal,
    pub response: CandidateResponseObservation,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateStreamChunk {
    pub index: u32,
    pub model: String,
    pub content: String,
    pub finish_reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateRequestObservation {
    pub request_operation_id: String,
    pub lease_id: String,
    pub token: TransitionToken,
    pub binding_id: String,
    pub incarnation: String,
    pub identities: Vec<ProcessIdentity>,
    pub observed_at_ms: i64,
    pub receipt: String,
    pub terminal: CandidateTerminal,
    pub response: CandidateResponseObservation,
}

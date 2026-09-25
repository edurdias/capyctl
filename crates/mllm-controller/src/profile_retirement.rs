//! ADR 0018 §4: the server side of removing a published runtime profile. The
//! session layer only relays; the service (mllm-management's
//! `StoreRetirements`) writes the durable retirement, issues ordinary stops,
//! and confirms on their evidence alone.
use std::time::Duration;

/// How often a draining retirement's progress is read.
pub const RETIREMENT_POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetirementStep {
    /// No deployment on the host uses the profile; confirmed.
    Confirmed,
    /// Refused without drain; the named deployments use it.
    InUse(Vec<String>),
    /// Stops issued for the named deployments; a terminal step follows.
    Draining(Vec<String>),
    /// Stops unsettled or failed at the bound; nothing confirmed.
    Holding(Vec<String>),
    /// Could not be started; the phrase says why.
    Refused(String),
}

impl RetirementStep {
    /// `ProfileRetirement.outcome`.
    pub fn outcome(&self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::InUse(_) => "in_use",
            Self::Draining(_) => "draining",
            Self::Holding(_) => "holding",
            Self::Refused(_) => "refused",
        }
    }
    pub fn deployments(&self) -> &[String] {
        match self {
            Self::InUse(d) | Self::Draining(d) | Self::Holding(d) => d,
            Self::Confirmed | Self::Refused(_) => &[],
        }
    }
    /// The wire message for `request_id`, bounded (ADR 0018 §4).
    pub fn to_wire(&self, request_id: &str) -> mllm_protocol::pb::ProfileRetirement {
        use mllm_protocol::capabilities::{MAX_REASON, MAX_RETIREMENT_DEPLOYMENTS};
        mllm_protocol::pb::ProfileRetirement {
            request_id: request_id.into(),
            outcome: self.outcome().into(),
            deployments: self
                .deployments()
                .iter()
                .take(MAX_RETIREMENT_DEPLOYMENTS)
                .cloned()
                .collect(),
            reason: match self {
                Self::Refused(reason) => reason.chars().take(MAX_REASON).collect(),
                _ => String::new(),
            },
        }
    }
}

/// ADR 0018 §4. Both calls are blocking store work; the session runs them
/// off its task.
pub trait ProfileRetirements: Send + Sync {
    /// Phase one: write the retirement and check references; with `drain`,
    /// issue the ordinary stops. Idempotent per `key`.
    fn begin(&self, host: &str, profile: &str, key: &str, drain: bool) -> RetirementStep;
    /// A draining retirement's progress: `None` while stops are unsettled,
    /// otherwise the terminal step (`Confirmed` or `Holding`).
    fn poll(&self, host: &str, profile: &str, key: &str) -> Option<RetirementStep>;
}

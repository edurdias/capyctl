//! ADR 0040 §10: the server's half of host administration. [`request`]
//! carries one operation to a host's live control session and waits for the
//! host's answer, or refuses with a closed code (ADR 0017) without waiting.
use crate::agent_sessions::AgentSessions;
use capyctl_protocol::pb::{self, host_admin_request::Op};
use std::time::Duration;

/// Why a host administration request got no answer from its host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// ADR 0017: `host_capability_missing:host_admin`; nothing was sent.
    CapabilityMissing(String),
    /// The host has no live session; refused at once, nothing was sent.
    ObservationStale,
    /// ADR 0017: the host's version allows it no change; nothing was sent.
    HostUpgradeRequired,
    /// The session ended after the request went out; it may or may not have
    /// taken effect on the host.
    OutcomeUnknown,
    /// No answer by the deadline; it may or may not have taken effect.
    DeadlineExceeded,
    /// Too many requests outstanding, or the session's queue is full;
    /// nothing was sent.
    Busy,
    /// The complete encoded request exceeds the server-to-host message
    /// limit; nothing was sent and the control session remains usable.
    RequestTooLarge,
}

impl Refusal {
    /// The closed code a caller reports.
    pub fn code(&self) -> &str {
        match self {
            Self::CapabilityMissing(reason) => reason,
            Self::ObservationStale => "observation_stale",
            Self::HostUpgradeRequired => capyctl_protocol::capabilities::HOST_UPGRADE_REQUIRED,
            Self::OutcomeUnknown => "outcome_unknown",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Busy => "host_admin_busy",
            Self::RequestTooLarge => "request_too_large",
        }
    }
}

/// ADR 0040 §10: run `op` on `host` and wait at most `deadline` for its
/// answer. The reply is the host's: `result_json`, `pin_and_size`, or the
/// host's own refusal in `error_code` and `error_message`.
///
/// ADR 0040 §10 / KTD7, KTD11: cancellation or expiry ends only this wait,
/// not an operation already dispatched to the host. An uncertain outcome
/// is never retried automatically.
pub async fn request(
    sessions: &AgentSessions,
    host: &str,
    op: Op,
    deadline: Duration,
) -> Result<pb::HostAdminReply, Refusal> {
    let deadline_unix_ms = capyctl_protocol::now_unix_ms()
        .saturating_add(i64::try_from(deadline.as_millis()).unwrap_or(i64::MAX));
    let (_guard, reply) = sessions.send_host_admin(host, op, deadline_unix_ms)?;
    match tokio::time::timeout(deadline, reply).await {
        Ok(Ok(reply)) => Ok(reply),
        // The session ended with the request outstanding.
        Ok(Err(_)) => Err(Refusal::OutcomeUnknown),
        Err(_) => Err(Refusal::DeadlineExceeded),
    }
}

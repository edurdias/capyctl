//! ADR 0037 §§1, 4, 6, 13: the host role's tracing, installed once when the
//! role starts and read wherever the host records a span (its ingress, its
//! lifecycle steps) or reports its tracing status.
//!
//! Never installed is tracing off: [`get`] answers [`Tracing::off`], so no
//! header is read for a span and no span is recorded (§1).

use std::sync::OnceLock;

use capyctl_protocol::pb;
use capyctl_trace::Tracing;

static HOST: OnceLock<Tracing> = OnceLock::new();

/// Install the host role's tracing. The role calls this once, at its start;
/// a later call changes nothing (the first installed tracing stays).
pub fn install(tracing: Tracing) {
    let _ = HOST.set(tracing);
}

/// The host role's tracing: off when none was installed.
pub fn get() -> Tracing {
    HOST.get().cloned().unwrap_or_default()
}

/// ADR 0037 §13: the role's tracing status as the host's periodic report
/// carries it: on or off, the endpoint, spans dropped and the last export
/// error since the start. Off reports all fields empty.
pub fn status(tracing: &Tracing) -> pb::HostTracingStatus {
    let stats = tracing.stats();
    pb::HostTracingStatus {
        on: tracing.is_on(),
        endpoint: tracing.endpoint().unwrap_or_default().to_owned(),
        spans_dropped: stats.as_ref().map_or(0, |s| s.dropped),
        last_export_error: stats.and_then(|s| s.last_export_error).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T48: a host that never installed tracing is off, and reports off.
    #[test]
    fn a_host_without_installed_tracing_reports_off() {
        let off = Tracing::off();
        assert_eq!(status(&off), pb::HostTracingStatus::default());
    }

    // T48: a host with tracing on says so in its report (a test tracing has
    // no endpoint and no export queue, so neither is reported).
    #[test]
    fn a_host_with_tracing_reports_on() {
        let (tracing, _) = capyctl_trace::testing::capture(
            1.0,
            capyctl_trace::CallerMode::Parent,
            capyctl_trace::Service::Host,
        );
        let reported = status(&tracing);
        assert!(reported.on);
        assert_eq!(reported.spans_dropped, 0);
        assert!(reported.endpoint.is_empty() && reported.last_export_error.is_empty());
    }
}

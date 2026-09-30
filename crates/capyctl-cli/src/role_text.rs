//! ADR 0021: the text form of role output — the ready banner, one line per
//! event, and the shutdown summary. JSON mode prints the objects unchanged.

use serde_json::Value;

use crate::detail::Detail;
use crate::output::OutputFormat;
use capyctl_domain::role_log::{self, Mode};

/// Set the role output mode and install the event formatter. Call once,
/// before the role prints anything.
pub fn install(format: OutputFormat) {
    role_log::set_mode(match format {
        OutputFormat::Text => Mode::Text,
        OutputFormat::Json => Mode::Json,
    });
    role_log::set_formatter(event_line);
}

fn short(id: &Value) -> String {
    id.as_str()
        .map(|s| s.chars().take(12).collect())
        .unwrap_or_default()
}

fn clock() -> String {
    // Local wall-clock time; the journal adds its own stamps in JSON mode.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// One event line in text mode: local time, then the sentence, or the
/// key=value fallback for events without one (spec §3).
fn event_line(value: &Value) -> Option<String> {
    Some(
        format_event(value).unwrap_or_else(|| format!("{} {}", clock(), role_log::fallback(value))),
    )
}

/// The text of one event, prefixed with local time; `None` for events
/// without a sentence.
pub fn format_event(value: &Value) -> Option<String> {
    let text = match value["event"].as_str()? {
        "switch" => format!(
            "switch {}: {}; {}",
            short(&value["switch_id"]),
            value["phase"].as_str().unwrap_or("").to_lowercase(),
            crate::detail::one_line(value["detail"].as_str().unwrap_or(""))
        ),
        "router_selection" => {
            // The ranking's first candidate is the pick; `chosen` is its generation.
            match value["candidates"].get(0).filter(|pick| pick.is_object()) {
                Some(pick) => format!(
                    "request {} -> {} instance {}",
                    short(&value["deployment"]),
                    pick["host"].as_str().unwrap_or("?"),
                    pick["instance"]
                ),
                None => format!(
                    "request {}: no instance available ({} skipped)",
                    short(&value["deployment"]),
                    value["skipped"].as_array().map_or(0, Vec::len)
                ),
            }
        }
        "router_failover" => {
            let reason = crate::detail::one_line(
                value["reason"]
                    .as_str()
                    .unwrap_or("another instance was chosen"),
            );
            match value["detail"].as_str().filter(|d| !d.is_empty()) {
                Some(detail) => format!(
                    "request {} failed over: {reason} ({})",
                    short(&value["deployment"]),
                    crate::detail::one_line(detail)
                ),
                None => format!(
                    "request {} failed over: {reason}",
                    short(&value["deployment"])
                ),
            }
        }
        _ => return None,
    };
    Some(format!("{} {text}", clock()))
}

pub(crate) fn banner_text(value: &Value) -> String {
    let role = value["role"].as_str().unwrap_or("role");
    let version = value["version"]
        .as_str()
        .unwrap_or(env!("CARGO_PKG_VERSION"));
    let inference =
        value["inference"]
            .as_str()
            .map(|bind| match value["inference_auth"].as_str() {
                Some("none") => format!("{bind} (no API key)"),
                _ => format!("{bind} (API key required)"),
            });
    let s = |key: &str| value[key].as_str().map(str::to_owned);
    Detail::new(format!("capyctl {version} {role} ready"))
        .row_opt("Inference", inference)
        .row_opt("Management", s("management"))
        .row_opt("Bootstrap", s("bootstrap"))
        .row_opt("Control", s("control"))
        .row_opt("Ingress", s("ingress"))
        .row_opt("Host ID", s("host_id"))
        .row_opt("State", s("state_dir"))
        .row_opt("Credentials", s("credentials"))
        .render()
}

pub(crate) fn stopped_text(value: &Value) -> String {
    let role = value["role"].as_str().unwrap_or("role");
    let drain = &value["drain"];
    let how = if drain["forced"] == true {
        "forced"
    } else if drain["drained"] == true {
        "drained"
    } else {
        "stopped"
    };
    let in_flight = drain["in_flight_at_close"].as_u64().unwrap_or(0);
    let cancelled = drain["cancelled"].as_u64().unwrap_or(0);
    let cancelled = if how == "stopped" && cancelled > 0 {
        format!(", {cancelled} cancelled")
    } else {
        String::new()
    };
    let requests = if in_flight == 1 {
        "request"
    } else {
        "requests"
    };
    let engines = if value["engines"] == "retained" {
        "; engines kept running"
    } else {
        ""
    };
    format!("capyctl {role} stopped: {how}, {in_flight} {requests} in flight{cancelled}{engines}\n")
}

/// The ready banner in the current mode (stdout).
pub fn banner(value: &Value) -> String {
    match role_log::mode() {
        Mode::Text => banner_text(value),
        Mode::Json => format!("{value}\n"),
    }
}

/// The shutdown summary in the current mode (stdout).
pub fn stopped(value: &Value) -> String {
    match role_log::mode() {
        Mode::Text => stopped_text(value),
        Mode::Json => format!("{value}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn without_time(line: &str) -> &str {
        line.get(9..).unwrap_or(line)
    }

    // T02 (ADR 0021)
    #[test]
    fn switch_and_router_events_read_as_sentences() {
        let switch = json!({"event": "switch", "phase": "Planned", "switch_id": "01M3QRK1AP7MFYHMZ3AG",
            "target": "01M3QRCJJ0MXR2F8ZAM3", "host": "gpu-box",
            "detail": "instance 0 wakes on gpu-box after releasing 1 instance(s)"});
        let line = format_event(&switch).unwrap();
        assert_eq!(&line[2..3], ":", "{line}");
        assert_eq!(without_time(&line), "switch 01M3QRK1AP7M: planned; instance 0 wakes on gpu-box after releasing 1 instance(s)");
        // `chosen` is the generation of the first candidate, which is the pick.
        let route = json!({"event": "router_selection", "deployment": "01M3QRCJJ0MXR2F8ZAM3", "chosen": 1,
            "candidates": [{"host": "gpu-box", "instance": 0, "generation": 1}]});
        assert_eq!(
            without_time(&format_event(&route).unwrap()),
            "request 01M3QRCJJ0MX -> gpu-box instance 0"
        );
        assert_eq!(format_event(&json!({"event": "unknown_thing"})), None);
    }

    // T02 (ADR 0021)
    #[test]
    fn banner_and_shutdown_in_text() {
        let banner_value = json!({"role": "standalone", "ready": true, "version": "0.1.0",
            "inference": "0.0.0.0:8443", "inference_auth": "api_key", "management": "127.0.0.1:7443",
            "state_dir": "/s", "credentials": "/s/identity/credentials"});
        assert_eq!(
            banner_text(&banner_value),
            "capyctl 0.1.0 standalone ready\n\n  Inference     0.0.0.0:8443 (API key required)\n  Management    127.0.0.1:7443\n  State         /s\n  Credentials   /s/identity/credentials\n"
        );
        let stopped_value = json!({"role": "standalone", "stopped": true, "engines": "retained",
            "drain": {"drained": true, "in_flight_at_close": 0, "cancelled": 0}});
        assert_eq!(
            stopped_text(&stopped_value),
            "capyctl standalone stopped: drained, 0 requests in flight; engines kept running\n"
        );
    }

    // T02 (ADR 0021): review fixes for the router lines and the shutdown summary.
    #[test]
    fn router_and_shutdown_edge_cases() {
        let none = json!({"event": "router_selection", "deployment": "01M3QRCJJ0MXR2F8ZAM3",
            "chosen": null, "candidates": [], "skipped": [{"instance": 0}, {"instance": 1}]});
        assert_eq!(
            without_time(&format_event(&none).unwrap()),
            "request 01M3QRCJJ0MX: no instance available (2 skipped)"
        );
        let failover = json!({"event": "router_failover", "deployment": "01M3QRCJJ0MXR2F8ZAM3",
            "reason": "not_accepted", "detail": "host is shutting down"});
        assert_eq!(
            without_time(&format_event(&failover).unwrap()),
            "request 01M3QRCJJ0MX failed over: not_accepted (host is shutting down)"
        );
        let stopped = json!({"role": "host", "engines": "retained",
            "drain": {"drained": false, "in_flight_at_close": 1, "cancelled": 2}});
        assert_eq!(
            stopped_text(&stopped),
            "capyctl host stopped: stopped, 1 request in flight, 2 cancelled; engines kept running\n"
        );
    }

    // T02 (ADR 0021, spec §3): events without a sentence still carry the time.
    #[test]
    fn fallback_events_carry_the_time() {
        let line = event_line(&json!({"event": "other", "n": 2})).unwrap();
        assert_eq!(&line[2..3], ":", "{line}");
        assert_eq!(without_time(&line), "other n=2");
    }

    // T02 (ADR 0021)
    #[test]
    fn banner_says_when_no_api_key_is_needed() {
        let value = json!({"role": "server", "ready": true, "version": "0.1.0",
            "inference": "0.0.0.0:8443", "inference_auth": "none"});
        assert!(banner_text(&value).contains("0.0.0.0:8443 (no API key)"));
    }

    // T02 (ADR 0021)
    #[test]
    fn server_and_host_banners_render_their_rows() {
        let server = json!({"role": "server", "ready": true, "version": "0.1.0",
            "management": "127.0.0.1:7443", "inference": "0.0.0.0:8443", "inference_auth": "api_key",
            "bootstrap": "0.0.0.0:7444", "control": "0.0.0.0:7445",
            "state_dir": "/s", "credentials": "/s/identity/server-credentials.json"});
        assert_eq!(
            banner_text(&server),
            "capyctl 0.1.0 server ready\n\n  Inference     0.0.0.0:8443 (API key required)\n  Management    127.0.0.1:7443\n  Bootstrap     0.0.0.0:7444\n  Control       0.0.0.0:7445\n  State         /s\n  Credentials   /s/identity/server-credentials.json\n"
        );
        let host = json!({"role": "host", "ready": true, "version": "0.1.0",
            "host_id": "01H", "state_dir": "/h", "ingress": null, "credentials": "/h/identity/host-identity.json"});
        assert_eq!(
            banner_text(&host),
            "capyctl 0.1.0 host ready\n\n  Host ID       01H\n  State         /h\n  Credentials   /h/identity/host-identity.json\n"
        );
    }
}

//! ADR 0031 §5, §6: the bounded reads of a custom engine's loopback port:
//! whether the port accepts a connection, the manifest's `health` path, and
//! the manifest's idle signal. Every read carries the launch's key when the
//! manifest uses `{api_key}` (§7). `/v1/models` is never read (§5).
use std::time::Duration;

use capyctl_config::custom::IdleSignal;

use crate::traits::{AdapterError, EngineWork};

/// A read of the engine's own surfaces must finish within this. The host's
/// stop gate leaves this much margin for the read in flight at its bound
/// (`native_execution/idle_gate.rs`), as for TensorFold and llama.cpp.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a connection attempt to the leased port may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// The longest idle-signal body read.
pub const MAX_IDLE_BODY: usize = 1024 * 1024;

pub(crate) struct Http {
    base: reqwest::Url,
    key: Option<reqwest::header::HeaderValue>,
    /// Reads of the engine's own surfaces, bounded by [`READ_TIMEOUT`].
    client: reqwest::Client,
    /// Probes, bounded per request by their caller.
    probes: reqwest::Client,
}

impl Http {
    pub(crate) fn new(base: reqwest::Url, key: Option<&str>) -> Self {
        let build = |timeout: Option<Duration>| {
            let builder = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(CONNECT_TIMEOUT);
            match timeout {
                Some(timeout) => builder.timeout(timeout),
                None => builder,
            }
            .build()
            .expect("static client configuration")
        };
        Self {
            base,
            key: key.and_then(crate::completion_probe::bearer),
            client: build(Some(READ_TIMEOUT)),
            probes: build(None),
        }
    }

    pub(crate) fn base(&self) -> &reqwest::Url {
        &self.base
    }

    pub(crate) fn key(&self) -> Option<&reqwest::header::HeaderValue> {
        self.key.as_ref()
    }

    pub(crate) fn probes(&self) -> &reqwest::Client {
        &self.probes
    }

    /// ADR 0031 §5: the leased port accepts a TCP connection.
    pub(crate) async fn port_accepts(&self) -> bool {
        let (Some(host), Some(port)) = (self.base.host_str(), self.base.port_or_known_default())
        else {
            return false;
        };
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect((host.as_str(), port)),
        )
        .await
        .is_ok_and(|connected| connected.is_ok())
    }

    fn get(&self, path: &str) -> Result<reqwest::RequestBuilder, AdapterError> {
        let url = self
            .base
            .join(path)
            .map_err(|_| AdapterError::Uncertain("bad engine URL".into()))?;
        let mut request = self.client.get(url);
        if let Some(key) = &self.key {
            request = request.header(reqwest::header::AUTHORIZATION, key.clone());
        }
        Ok(request)
    }

    /// ADR 0031 §5: the manifest's `health` path answers 200. Anything else
    /// (not listening, another status, no answer in time) is not yet.
    pub(crate) async fn health(&self, path: &str) -> Result<bool, AdapterError> {
        Ok(match self.get(path)?.send().await {
            Ok(response) => response.status() == reqwest::StatusCode::OK,
            Err(_) => false,
        })
    }

    /// ADR 0031 §6: one read of the idle signal. Idle only when every named
    /// gauge or field reads 0 (or `false`); nothing listening is
    /// `NotListening`; anything missing, malformed, non-200 or unanswered is
    /// `Unanswered`, never idle.
    pub(crate) async fn idle(&self, signal: &IdleSignal) -> EngineWork {
        let Ok(request) = self.get(&signal.url) else {
            return EngineWork::Unanswered;
        };
        let mut response = match request.send().await {
            Ok(response) => response,
            Err(error) if error.is_connect() => return EngineWork::NotListening,
            Err(_) => return EngineWork::Unanswered,
        };
        if response.status() != reqwest::StatusCode::OK
            || response
                .content_length()
                .is_some_and(|length| length > MAX_IDLE_BODY as u64)
        {
            return EngineWork::Unanswered;
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if body.len() + chunk.len() <= MAX_IDLE_BODY => {
                    body.extend_from_slice(&chunk)
                }
                Ok(None) => break,
                _ => return EngineWork::Unanswered,
            }
        }
        match idle_reading(signal, &body) {
            Some(true) => EngineWork::Idle,
            Some(false) => EngineWork::Busy,
            None => EngineWork::Unanswered,
        }
    }
}

/// ADR 0031 §6: what one 200 answer of the idle signal says. `Some(true)`
/// when every named gauge (Prometheus text, summed over its labels) or field
/// (a JSON object, dot-separated paths) is present and 0, or `false`;
/// `Some(false)` when they are all present and readable and one is not;
/// `None` when any is missing or malformed, or the body is not what the
/// manifest says: unknown, never idle.
pub fn idle_reading(signal: &IdleSignal, body: &[u8]) -> Option<bool> {
    if !signal.gauges.is_empty() {
        let text = std::str::from_utf8(body).ok()?;
        let mut idle = true;
        for name in &signal.gauges {
            idle &= gauge_sum(text, name)? == 0.0;
        }
        return Some(idle);
    }
    if signal.json.is_empty() {
        return None;
    }
    let document: serde_json::Value = serde_json::from_slice(body).ok()?;
    if !document.is_object() {
        return None;
    }
    let mut idle = true;
    for path in &signal.json {
        let value = path
            .split('.')
            .try_fold(&document, |value, part| value.as_object()?.get(part))?;
        idle &= match value {
            serde_json::Value::Bool(flag) => !flag,
            serde_json::Value::Number(number) => {
                let number = number.as_f64().filter(|n| n.is_finite())?;
                number == 0.0
            }
            _ => return None,
        };
    }
    Some(idle)
}

/// The sum of every sample of `name` in a Prometheus text exposition, over
/// all its label sets. `None` when no sample is present or one is not a
/// finite, non-negative number.
pub fn gauge_sum(text: &str, name: &str) -> Option<f64> {
    let mut sum = None;
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        let rest = match rest.chars().next() {
            Some('{') => after_labels(rest)?,
            Some(' ' | '\t') => rest,
            // A longer metric name sharing the prefix.
            _ => continue,
        };
        let value: f64 = rest.split_whitespace().next()?.parse().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        sum = Some(sum.unwrap_or(0.0) + value);
    }
    sum
}

/// What follows a sample's `{...}` label set, which may hold quoted `}`.
fn after_labels(text: &str) -> Option<&str> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, c) in text.char_indices().skip(1) {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '}' if !quoted => return Some(&text[index + 1..]),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gauges(names: &[&str]) -> IdleSignal {
        IdleSignal {
            url: "/metrics".into(),
            gauges: names.iter().map(|n| (*n).into()).collect(),
            json: Vec::new(),
        }
    }

    fn fields(paths: &[&str]) -> IdleSignal {
        IdleSignal {
            url: "/stats".into(),
            gauges: Vec::new(),
            json: paths.iter().map(|n| (*n).into()).collect(),
        }
    }

    // T44 (ADR 0031 §6): each named gauge present and 0, summed over its
    // labels; a missing, malformed or non-finite one is unknown.
    #[test]
    fn gauges_are_summed_over_their_labels() {
        let signal = gauges(&["requests_running", "requests_waiting"]);
        let idle = b"# HELP requests_running Running\n# TYPE requests_running gauge\n\
            requests_running{model=\"a\"} 0\nrequests_running{model=\"b}\"} 0\n\
            requests_waiting 0\nrequests_running_total 9\n";
        assert_eq!(idle_reading(&signal, idle), Some(true));
        let busy = b"requests_running{model=\"a\"} 0\nrequests_running{model=\"b\"} 1\nrequests_waiting 0\n";
        assert_eq!(idle_reading(&signal, busy), Some(false));
        for unknown in [
            &b"requests_running 0\n"[..],
            b"requests_running 0\nrequests_waiting NaN\n",
            b"requests_running 0\nrequests_waiting +Inf\n",
            b"requests_running 0\nrequests_waiting -1\n",
            b"requests_running 0\nrequests_waiting\n",
            b"requests_running{model=\"a\" 0\nrequests_waiting 0\n",
            b"\xff\xfe",
            b"",
        ] {
            assert_eq!(
                idle_reading(&signal, unknown),
                None,
                "{}",
                String::from_utf8_lossy(unknown)
            );
        }
    }

    // T44 (ADR 0031 §6): each dot path present and 0 or false; anything
    // else is unknown.
    #[test]
    fn json_fields_must_all_be_zero_or_false() {
        let signal = fields(&["queue.running", "queue.waiting", "busy"]);
        assert_eq!(
            idle_reading(
                &signal,
                br#"{"queue": {"running": 0, "waiting": 0.0}, "busy": false}"#
            ),
            Some(true)
        );
        assert_eq!(
            idle_reading(
                &signal,
                br#"{"queue": {"running": 2, "waiting": 0}, "busy": false}"#
            ),
            Some(false)
        );
        assert_eq!(
            idle_reading(
                &signal,
                br#"{"queue": {"running": 0, "waiting": 0}, "busy": true}"#
            ),
            Some(false)
        );
        for unknown in [
            &br#"{"queue": {"running": 0}, "busy": false}"#[..],
            br#"{"queue": {"running": "0", "waiting": 0}, "busy": false}"#,
            br#"{"queue": {"running": null, "waiting": 0}, "busy": false}"#,
            br#"{"queue": 0, "busy": false}"#,
            br#"[0]"#,
            b"not json",
        ] {
            assert_eq!(
                idle_reading(&signal, unknown),
                None,
                "{}",
                String::from_utf8_lossy(unknown)
            );
        }
    }
}

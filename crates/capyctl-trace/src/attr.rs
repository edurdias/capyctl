//! ADR 0037 §5: the only attributes a CapyCTL span may carry. The type is
//! closed: a free-form key cannot be written, and each value is either one of
//! CapyCTL's own names and codes, an integer, a boolean, or a member of a
//! closed set. Nothing from a request or response body has a variant.

use opentelemetry::{Array, KeyValue, StringValue, Value};

/// Every attribute key a CapyCTL span may carry (ADR 0037 §5). T48 fails on
/// any exported key outside this list.
pub const ALLOWED_KEYS: &[&str] = &[
    "capyctl.request_id",
    "capyctl.endpoint",
    "capyctl.deployment",
    "capyctl.revision",
    "capyctl.instance",
    "capyctl.host",
    "capyctl.generation",
    "capyctl.attempt",
    "capyctl.engine",
    "capyctl.engine_version",
    "capyctl.stream",
    "capyctl.outcome",
    "capyctl.code",
    "capyctl.cancel_cause",
    "capyctl.activation.kind",
    "capyctl.activation.started",
    "capyctl.park_tier",
    "capyctl.victim",
    "http.request.method",
    "http.route",
    "http.response.status_code",
    "gen_ai.usage.input_tokens",
    "gen_ai.usage.output_tokens",
    "gen_ai.response.finish_reasons",
];

/// Every resource key (ADR 0037 §5): no machine ID, host identifier or
/// address.
pub const RESOURCE_KEYS: &[&str] = &["service.name", "service.version", "capyctl.role"];

/// The inference endpoint a request came in on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Chat,
    Decisions,
    Embeddings,
}

impl Endpoint {
    fn as_str(self) -> &'static str {
        match self {
            Endpoint::Chat => "chat",
            Endpoint::Decisions => "decisions",
            Endpoint::Embeddings => "embeddings",
        }
    }
}

/// How a request or a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Refused,
    Failed,
    Cancelling,
    Uncertain,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Refused => "refused",
            Outcome::Failed => "failed",
            Outcome::Cancelling => "cancelling",
            Outcome::Uncertain => "uncertain",
        }
    }
}

/// Why a request was cut (SPEC §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelCause {
    ClientHangup,
    Deadline,
    StreamIdle,
}

impl CancelCause {
    fn as_str(self) -> &'static str {
        match self {
            CancelCause::ClientHangup => "client_hangup",
            CancelCause::Deadline => "deadline",
            CancelCause::StreamIdle => "stream_idle",
        }
    }
}

/// What an activation did (ADR 0037 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationKind {
    ColdStart,
    Wake,
    Switch,
    Restart,
}

impl ActivationKind {
    fn as_str(self) -> &'static str {
        match self {
            ActivationKind::ColdStart => "cold_start",
            ActivationKind::Wake => "wake",
            ActivationKind::Switch => "switch",
            ActivationKind::Restart => "restart",
        }
    }
}

/// Finish reasons an engine may report that a span may carry (ADR 0037 §5:
/// "from the engine's closed set"). Anything else is left out rather than
/// copied: an engine-supplied string is not CapyCTL's to export.
pub const FINISH_REASONS: &[&str] = &[
    "stop",
    "length",
    "tool_calls",
    "content_filter",
    "function_call",
    "abort",
];

/// One allow-listed attribute.
#[derive(Debug, Clone, PartialEq)]
pub enum Attr {
    /// `X-CapyCTL-Request-Id` (a ULID). Never the caller's `X-Request-Id`
    /// (ADR 0037 §10, owner decision 2026-10-27 q5).
    RequestId(String),
    Endpoint(Endpoint),
    /// CapyCTL's names, only after the request resolved to a deployment.
    Deployment(String),
    Revision(String),
    Instance(String),
    Host(String),
    Generation(i64),
    Attempt(i64),
    Engine(String),
    EngineVersion(String),
    Stream(bool),
    Outcome(Outcome),
    /// A CapyCTL error or refusal code; see [`code`].
    Code(String),
    CancelCause(CancelCause),
    ActivationKind(ActivationKind),
    ActivationStarted(bool),
    ParkTier(String),
    Victim(String),
    HttpMethod(&'static str),
    HttpRoute(&'static str),
    HttpStatus(u16),
    InputTokens(i64),
    OutputTokens(i64),
    /// Filtered to [`FINISH_REASONS`].
    FinishReasons(Vec<String>),
}

/// A CapyCTL code as a span may carry it: the code's own shape (lowercase
/// letters, digits, `_`, `:`, `.` and `-`, at most 128 bytes) or `internal`.
/// Codes come from CapyCTL's closed sets; the shape check keeps a message or
/// an engine's text from ever being passed by mistake.
pub fn code(value: &str) -> String {
    let shaped = !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b':' | b'.' | b'-')
        });
    if shaped {
        value.to_owned()
    } else {
        "internal".to_owned()
    }
}

impl Attr {
    /// The attribute's key, always one of [`ALLOWED_KEYS`].
    pub fn key(&self) -> &'static str {
        match self {
            Attr::RequestId(_) => "capyctl.request_id",
            Attr::Endpoint(_) => "capyctl.endpoint",
            Attr::Deployment(_) => "capyctl.deployment",
            Attr::Revision(_) => "capyctl.revision",
            Attr::Instance(_) => "capyctl.instance",
            Attr::Host(_) => "capyctl.host",
            Attr::Generation(_) => "capyctl.generation",
            Attr::Attempt(_) => "capyctl.attempt",
            Attr::Engine(_) => "capyctl.engine",
            Attr::EngineVersion(_) => "capyctl.engine_version",
            Attr::Stream(_) => "capyctl.stream",
            Attr::Outcome(_) => "capyctl.outcome",
            Attr::Code(_) => "capyctl.code",
            Attr::CancelCause(_) => "capyctl.cancel_cause",
            Attr::ActivationKind(_) => "capyctl.activation.kind",
            Attr::ActivationStarted(_) => "capyctl.activation.started",
            Attr::ParkTier(_) => "capyctl.park_tier",
            Attr::Victim(_) => "capyctl.victim",
            Attr::HttpMethod(_) => "http.request.method",
            Attr::HttpRoute(_) => "http.route",
            Attr::HttpStatus(_) => "http.response.status_code",
            Attr::InputTokens(_) => "gen_ai.usage.input_tokens",
            Attr::OutputTokens(_) => "gen_ai.usage.output_tokens",
            Attr::FinishReasons(_) => "gen_ai.response.finish_reasons",
        }
    }

    pub(crate) fn into_key_value(self) -> KeyValue {
        let key = self.key();
        let value: Value = match self {
            Attr::RequestId(s)
            | Attr::Deployment(s)
            | Attr::Revision(s)
            | Attr::Instance(s)
            | Attr::Host(s)
            | Attr::Engine(s)
            | Attr::EngineVersion(s)
            | Attr::ParkTier(s)
            | Attr::Victim(s) => s.into(),
            Attr::Code(s) => code(&s).into(),
            Attr::Endpoint(e) => e.as_str().into(),
            Attr::Generation(n)
            | Attr::Attempt(n)
            | Attr::InputTokens(n)
            | Attr::OutputTokens(n) => n.into(),
            Attr::Stream(b) | Attr::ActivationStarted(b) => b.into(),
            Attr::Outcome(o) => o.as_str().into(),
            Attr::CancelCause(c) => c.as_str().into(),
            Attr::ActivationKind(k) => k.as_str().into(),
            Attr::HttpMethod(m) | Attr::HttpRoute(m) => m.into(),
            Attr::HttpStatus(s) => i64::from(s).into(),
            Attr::FinishReasons(reasons) => Value::Array(Array::String(
                reasons
                    .into_iter()
                    .filter(|r| FINISH_REASONS.contains(&r.as_str()))
                    .map(StringValue::from)
                    .collect(),
            )),
        };
        KeyValue::new(key, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T48: every key an `Attr` can produce is in the allow-list.
    #[test]
    fn every_variant_key_is_allowed() {
        let all = [
            Attr::RequestId("01J".into()),
            Attr::Endpoint(Endpoint::Chat),
            Attr::Deployment("d".into()),
            Attr::Revision("r".into()),
            Attr::Instance("i".into()),
            Attr::Host("h".into()),
            Attr::Generation(1),
            Attr::Attempt(1),
            Attr::Engine("sglang".into()),
            Attr::EngineVersion("0.5.22".into()),
            Attr::Stream(true),
            Attr::Outcome(Outcome::Completed),
            Attr::Code("queue_full".into()),
            Attr::CancelCause(CancelCause::Deadline),
            Attr::ActivationKind(ActivationKind::Wake),
            Attr::ActivationStarted(true),
            Attr::ParkTier("deep".into()),
            Attr::Victim("v".into()),
            Attr::HttpMethod("POST"),
            Attr::HttpRoute("/v1/chat/completions"),
            Attr::HttpStatus(200),
            Attr::InputTokens(1),
            Attr::OutputTokens(1),
            Attr::FinishReasons(vec!["stop".into()]),
        ];
        assert_eq!(all.len(), ALLOWED_KEYS.len());
        for attr in all {
            assert!(ALLOWED_KEYS.contains(&attr.key()));
            assert_eq!(attr.clone().into_key_value().key.as_str(), attr.key());
        }
    }

    // T48: a code is CapyCTL's own shape or `internal`; free text never passes.
    #[test]
    fn codes_are_shaped() {
        assert_eq!(code("queue_full"), "queue_full");
        assert_eq!(
            code("host_capability_missing:trace_context"),
            "host_capability_missing:trace_context"
        );
        assert_eq!(code("bad json: expected value at line 1"), "internal");
        assert_eq!(code(""), "internal");
    }

    // T48: finish reasons outside the closed set are left out.
    #[test]
    fn finish_reasons_are_filtered() {
        let kv = Attr::FinishReasons(vec!["stop".into(), "SENTINEL text".into()]).into_key_value();
        assert!(!format!("{:?}", kv.value).contains("SENTINEL"));
    }
}

//! ADR 0037 §§2, 3, 10: one request's spans at the router.
//!
//! A request that authenticated gets `capyctl.request` (server kind, the
//! caller's context as parent or link under `caller_context`) and
//! `capyctl.admission`; the phases that follow hang their own spans under the
//! request span. With tracing off, nothing here exists: [`RequestTrace`] is
//! never built, no header is read, and every span is empty (§1).

use std::sync::Mutex;

use axum::http::HeaderMap;
use capyctl_adapters::traits::TraceHeaders;
use capyctl_trace::{
    Attr, CancelCause, Endpoint as TraceEndpoint, Name, Outcome, Parent, Span, SpanCx, Tracing,
};

use crate::endpoint::Endpoint;

/// The router's view of one request's trace.
pub struct RequestTrace {
    tracing: Tracing,
    state: Mutex<State>,
}

impl std::fmt::Debug for RequestTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestTrace")
    }
}

#[derive(Default)]
struct State {
    request: Span,
    /// Open until the request resolved to a deployment or was refused.
    admission: Option<Span>,
    attempts: i64,
    /// The request span's context, kept after it ends for a later
    /// `capyctl.settle` (ADR 0037 §3: 25 bytes beside the open lease).
    cx: Option<SpanCx>,
    /// A streamed answer ends the request span at its own end.
    deferred: bool,
    ended: bool,
}

/// How a request ended, for its span (ADR 0037 §5).
pub struct Ending<'a> {
    /// The answer's HTTP status; `None` when the client got none (it went
    /// away, or the request was cut).
    pub status: Option<u16>,
    pub outcome: Outcome,
    pub code: Option<&'a str>,
    pub cause: Option<CancelCause>,
}

impl RequestTrace {
    /// ADR 0037 §2: called only after the request authenticated; `None` when
    /// tracing is off, so nothing is read.
    pub(crate) fn begin(
        tracing: &Tracing,
        headers: &HeaderMap,
        endpoint: Endpoint,
    ) -> Option<Self> {
        if !tracing.is_on() {
            return None;
        }
        let caller = tracing.caller(headers);
        let mut request = tracing.request(caller.as_ref());
        let (name, route) = match endpoint {
            Endpoint::ChatCompletions => (TraceEndpoint::Chat, "/v1/chat/completions"),
            Endpoint::Decisions => (TraceEndpoint::Decisions, "/v1/systemone"),
        };
        request.set(Attr::Endpoint(name));
        request.set(Attr::HttpMethod("POST"));
        request.set(Attr::HttpRoute(route));
        let admission = tracing.span(Name::Admission, Parent::Span(&request), &[]);
        Some(Self {
            tracing: tracing.clone(),
            state: Mutex::new(State {
                cx: request.cx(),
                request,
                admission: Some(admission),
                ..State::default()
            }),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn tracing(&self) -> &Tracing {
        &self.tracing
    }

    /// ADR 0037 §3: admission ends when the request resolved to a
    /// deployment; CapyCTL's names go on spans only from then on (§5).
    pub(crate) fn resolved(&self, deployment: &str, engine: &str) {
        let mut state = self.state();
        state.request.set(Attr::Deployment(deployment.to_owned()));
        if !engine.is_empty() {
            state.request.set(Attr::Engine(engine.to_owned()));
        }
        if let Some(mut admission) = state.admission.take() {
            admission.set(Attr::Deployment(deployment.to_owned()));
            admission.set(Attr::Outcome(Outcome::Completed));
            admission.end();
        }
    }

    /// A phase span under the request span.
    pub(crate) fn child(&self, name: Name) -> Span {
        let state = self.state();
        self.tracing.span(name, Parent::Span(&state.request), &[])
    }

    /// One more dispatch attempt; the first is 1.
    pub(crate) fn next_attempt(&self) -> i64 {
        let mut state = self.state();
        state.attempts += 1;
        state.attempts
    }

    /// The request span's context: a later `capyctl.settle` parents to it.
    pub(crate) fn cx(&self) -> Option<SpanCx> {
        self.state().cx
    }

    /// Set one attribute on the request span.
    pub(crate) fn set(&self, attr: Attr) {
        self.state().request.set(attr);
    }

    /// The streamed answer ends the request span itself.
    pub(crate) fn defer(&self) {
        self.state().deferred = true;
    }

    /// End the request span, unless a streamed answer ends it later.
    pub(crate) fn finish(&self, request_id: &str, ending: Ending<'_>) {
        if self.state().deferred {
            return;
        }
        self.end(request_id, ending);
    }

    /// End the request span now; later calls do nothing.
    pub(crate) fn end(&self, request_id: &str, ending: Ending<'_>) {
        let mut state = self.state();
        if std::mem::replace(&mut state.ended, true) {
            return;
        }
        if let Some(mut admission) = state.admission.take() {
            // Refused before it resolved to a deployment.
            admission.set(Attr::Outcome(ending.outcome));
            if let Some(code) = ending.code {
                admission.fail(code);
            }
            admission.end();
        }
        let request = &mut state.request;
        request.set(Attr::RequestId(request_id.to_owned()));
        if let Some(status) = ending.status {
            request.set(Attr::HttpStatus(status));
        }
        request.set(Attr::Outcome(ending.outcome));
        if let Some(cause) = ending.cause {
            request.set(Attr::CancelCause(cause));
        }
        if let Some(code) = ending.code {
            request.fail(code);
        }
        std::mem::take(&mut state.request).end();
    }
}

/// ADR 0037 §5: how an answer that is not a stream ended, from its status
/// and CapyCTL code.
pub(crate) fn outcome_of(status: u16, code: Option<&str>) -> Outcome {
    match (status, code) {
        (200..=299, _) => Outcome::Completed,
        // `backend completion unverified`: the engine may have the request.
        (500, Some("engine_error")) => Outcome::Uncertain,
        (500, Some("activation_uncertain")) | (503, Some("activation_uncertain")) => {
            Outcome::Uncertain
        }
        (502, _) => Outcome::Failed,
        (400..=499, _) | (503, _) => Outcome::Refused,
        _ => Outcome::Failed,
    }
}

/// ADR 0037 §5: a collected answer's token counts and finish reasons on its
/// request span.
pub(crate) fn record_answer(ids: &crate::request_id::RequestIds, answer: &serde_json::Value) {
    if let Some(trace) = ids.trace() {
        let mut usage = Usage::default();
        usage.observe(answer);
        usage.record(trace);
    }
}

/// ADR 0037 §6: the trace headers a forward's span hands its forwarder, on
/// a lane that carries them only.
pub(crate) fn headers_for(span: &Span, carries: bool) -> Option<TraceHeaders> {
    if !carries {
        return None;
    }
    let outbound = span.outbound()?;
    Some(TraceHeaders {
        traceparent: outbound.traceparent,
        tracestate: outbound.tracestate,
    })
}

/// ADR 0037 §5: the token counts and finish reasons an answer reported, as
/// span attributes. Read from `usage` and `choices[].finish_reason` only.
#[derive(Default)]
pub(crate) struct Usage {
    input: Option<i64>,
    output: Option<i64>,
    finish: Vec<String>,
}

impl Usage {
    /// Whether one streamed chunk may carry what [`Usage::observe`] reads.
    pub(crate) fn may_carry(chunk: &str) -> bool {
        chunk.contains("\"finish_reason\":\"")
            || (chunk.contains("\"usage\"") && !chunk.contains("\"usage\":null"))
    }

    pub(crate) fn observe(&mut self, value: &serde_json::Value) {
        if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
            if let Some(n) = usage
                .get("prompt_tokens")
                .and_then(serde_json::Value::as_i64)
            {
                self.input = Some(n);
            }
            if let Some(n) = usage
                .get("completion_tokens")
                .and_then(serde_json::Value::as_i64)
            {
                self.output = Some(n);
            }
        }
        if let Some(choices) = value.get("choices").and_then(serde_json::Value::as_array) {
            for reason in choices
                .iter()
                .filter_map(|c| c.get("finish_reason").and_then(serde_json::Value::as_str))
            {
                if !self.finish.iter().any(|r| r == reason) {
                    self.finish.push(reason.to_owned());
                }
            }
        }
    }

    pub(crate) fn record(self, trace: &RequestTrace) {
        if let Some(n) = self.input {
            trace.set(Attr::InputTokens(n));
        }
        if let Some(n) = self.output {
            trace.set(Attr::OutputTokens(n));
        }
        if !self.finish.is_empty() {
            trace.set(Attr::FinishReasons(self.finish));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T48 (§5): only token counts and finish reasons are read from an answer.
    #[test]
    fn usage_reads_counts_and_reasons_only() {
        let mut usage = Usage::default();
        usage.observe(&serde_json::json!({
            "choices": [{"finish_reason": "stop", "message": {"content": "SENTINEL"}}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3},
        }));
        assert_eq!(usage.input, Some(7));
        assert_eq!(usage.output, Some(3));
        assert_eq!(usage.finish, vec!["stop".to_owned()]);
    }

    // T48 (§5): the outcome of an answer.
    #[test]
    fn outcomes_follow_the_answer() {
        assert_eq!(outcome_of(200, None), Outcome::Completed);
        assert_eq!(outcome_of(429, Some("queue_full")), Outcome::Refused);
        assert_eq!(outcome_of(503, Some("unavailable")), Outcome::Refused);
        assert_eq!(outcome_of(502, Some("engine_error")), Outcome::Failed);
        assert_eq!(outcome_of(500, Some("engine_error")), Outcome::Uncertain);
        assert_eq!(outcome_of(500, Some("activation_failed")), Outcome::Failed);
    }
}

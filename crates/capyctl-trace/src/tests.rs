//! T48: the trace context core (ADR 0037 §§1, 2, 5, 8, 9).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::{HeaderMap, HeaderValue};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;

use crate::export::{Notices, Queue, TraceStats, NOTICE_INTERVAL};
use crate::testing::{capture, contains};
use crate::*;

const CALLER: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const CALLER_UNSAMPLED: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";

fn with_traceparent(value: &str, state: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(TRACEPARENT, HeaderValue::from_str(value).unwrap());
    if let Some(state) = state {
        headers.insert(TRACESTATE, HeaderValue::from_str(state).unwrap());
    }
    headers
}

// T48: off reads nothing, records nothing and propagates nothing.
#[test]
fn off_is_inert() {
    let tracing = Tracing::off();
    assert!(!tracing.is_on());
    assert_eq!(tracing.caller(&with_traceparent(CALLER, Some("a=1"))), None);
    let mut span = tracing.request(None);
    span.set(Attr::RequestId("01J".into()));
    assert_eq!(span.cx(), None);
    assert_eq!(span.outbound(), None);
    assert!(!span.is_recording());
    assert_eq!(tracing.stats(), None);
    assert_eq!(tracing.endpoint(), None);
}

// T48: `caller_context: parent` makes the caller's span the request's parent
// and keeps its `tracestate` for the next hop.
#[test]
fn parent_mode_joins_the_callers_trace() {
    let (tracing, capture) = capture(1.0, CallerMode::Parent, Service::Server);
    let caller = tracing.caller(&with_traceparent(CALLER, Some("vendor=abc")));
    let request = tracing.request(caller.as_ref());
    let forward = tracing.span(Name::Forward, Parent::Span(&request), &[]);
    let outbound = forward.outbound().expect("outbound");
    assert!(outbound
        .traceparent
        .starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"));
    assert!(outbound.traceparent.ends_with("-01"));
    assert_eq!(outbound.tracestate.as_deref(), Some("vendor=abc"));
    forward.end();
    request.end();
    let spans = capture.spans();
    let request = spans.iter().find(|s| s.name == "capyctl.request").unwrap();
    assert_eq!(request.kind, Kind::Server);
    assert_eq!(
        request.parent,
        Some([0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7])
    );
    let forward = spans.iter().find(|s| s.name == "capyctl.forward").unwrap();
    assert_eq!(forward.kind, Kind::Client);
    assert_eq!(forward.parent, Some(request.cx.span_id));
    assert_eq!(forward.cx.trace_id, request.cx.trace_id);
}

// T48: `caller_context: link` starts a new trace linked to the caller's.
#[test]
fn link_mode_starts_a_linked_trace() {
    let (tracing, capture) = capture(1.0, CallerMode::Link, Service::Server);
    let caller = tracing.caller(&with_traceparent(CALLER, None)).unwrap();
    tracing.request(Some(&caller)).end();
    let request = &capture.named("capyctl.request")[0];
    assert_ne!(request.cx.trace_id, caller.parent.trace_id);
    assert_eq!(request.parent, None);
    assert_eq!(request.links, vec![caller.parent]);
}

// T48: `caller_context: ignore` reads neither header.
#[test]
fn ignore_mode_reads_nothing() {
    let (tracing, capture) = capture(1.0, CallerMode::Ignore, Service::Server);
    assert_eq!(tracing.caller(&with_traceparent(CALLER, Some("a=1"))), None);
    tracing.request(None).end();
    let request = &capture.named("capyctl.request")[0];
    assert_eq!(request.parent, None);
    assert!(request.links.is_empty());
}

// T48: a malformed `traceparent` starts a new trace.
#[test]
fn malformed_traceparent_starts_a_new_trace() {
    let (tracing, capture) = capture(1.0, CallerMode::Parent, Service::Server);
    let caller = tracing.caller(&with_traceparent("00-zz-yy-01", None));
    assert_eq!(caller, None);
    tracing.request(caller.as_ref()).end();
    assert_eq!(capture.named("capyctl.request")[0].parent, None);
}

// T48 (§9): sampling by ratio; an unsampled request still propagates its
// context, with the sampled flag clear.
#[test]
fn ratio_zero_records_nothing_but_propagates() {
    let (tracing, capture) = capture(0.0, CallerMode::Parent, Service::Server);
    let request = tracing.request(None);
    assert!(!request.is_recording());
    let outbound = request.outbound().expect("context propagated");
    assert!(outbound.traceparent.ends_with("-00"));
    request.end();
    assert!(capture.spans().is_empty());
}

// T48 (§9): the caller's sampled flag decides, whatever the ratio.
#[test]
fn callers_flag_decides() {
    let (tracing, capture) = capture(0.0, CallerMode::Parent, Service::Server);
    let caller = tracing.caller(&with_traceparent(CALLER, None));
    tracing.request(caller.as_ref()).end();
    assert_eq!(capture.named("capyctl.request").len(), 1);

    let (tracing, capture) = crate::testing::capture(1.0, CallerMode::Parent, Service::Server);
    let caller = tracing.caller(&with_traceparent(CALLER_UNSAMPLED, None));
    let request = tracing.request(caller.as_ref());
    assert!(request.outbound().unwrap().traceparent.ends_with("-00"));
    request.end();
    assert!(capture.spans().is_empty());
}

// T48 (§4): an activation trace is recorded at ratio 0, with its steps, and
// a waiting request's `activation_wait` links to it.
#[test]
fn activations_ignore_the_ratio_and_are_linked() {
    let (tracing, capture) = capture(0.0, CallerMode::Parent, Service::Server);
    let caller = tracing.caller(&with_traceparent(CALLER, None));
    let request = tracing.request(caller.as_ref());
    let activation = tracing.span(Name::Activation, Parent::Root, &[]);
    let wake = tracing.span(Name::Wake, Parent::Span(&activation), &[]);
    let mut wait = tracing.span(Name::ActivationWait, Parent::Span(&request), &[]);
    wait.link(activation.cx().unwrap());
    wait.set(Attr::ActivationStarted(true));
    wake.end();
    let activation_cx = activation.cx().unwrap();
    activation.end();
    wait.end();
    request.end();
    let root = &capture.named("capyctl.activation")[0];
    assert_eq!(root.parent, None);
    assert_ne!(
        root.cx.trace_id,
        [
            0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e,
            0x47, 0x36
        ]
    );
    assert_eq!(
        capture.named("capyctl.wake")[0].parent,
        Some(root.cx.span_id)
    );
    let wait = &capture.named("capyctl.activation_wait")[0];
    assert_eq!(wait.links.len(), 1);
    assert_eq!(wait.links[0].span_id, activation_cx.span_id);
    assert_eq!(wait.attr("capyctl.activation.started"), Some("true"));
}

// T48 (§3): a settlement after the request span ended still parents to it.
#[test]
fn settle_parents_to_a_stored_context() {
    let (tracing, capture) = capture(1.0, CallerMode::Parent, Service::Server);
    let request = tracing.request(None);
    let stored = request.cx().unwrap();
    request.end();
    tracing.span(Name::Settle, Parent::Cx(stored), &[]).end();
    let settle = &capture.named("capyctl.settle")[0];
    assert_eq!(settle.parent, Some(stored.span_id));
    assert_eq!(settle.cx.trace_id, stored.trace_id);
}

// T48 (§5): an error span's description is its code; resource keys are
// exactly the three allowed.
#[test]
fn failures_carry_only_the_code() {
    let (tracing, capture) = capture(1.0, CallerMode::Parent, Service::Host);
    let mut span = tracing.request(None);
    span.fail("queue_full");
    span.end();
    let mut span = tracing.request(None);
    span.fail("engine said: SENTINEL-PROMPT");
    span.end();
    let spans = capture.spans();
    assert_eq!(spans[0].error.as_deref(), Some("queue_full"));
    assert_eq!(spans[0].attr("capyctl.code"), Some("queue_full"));
    assert_eq!(spans[1].error.as_deref(), Some("internal"));
    assert!(!contains(&capture.encoded(), b"SENTINEL"));
    let decoded = ExportTraceServiceRequest::decode(capture.encoded().as_slice()).unwrap();
    let mut keys: Vec<String> = decoded.resource_spans[0]
        .resource
        .as_ref()
        .unwrap()
        .attributes
        .iter()
        .map(|kv| kv.key.clone())
        .collect();
    keys.sort();
    let mut allowed = RESOURCE_KEYS.to_vec();
    allowed.sort();
    assert_eq!(keys, allowed);
    assert!(capture.disallowed_keys().is_empty());
}

// T48 (§8): `OTEL_*` in the role's environment changes nothing: not the
// sampler, the limits, the resource nor the endpoint.
#[test]
fn ambient_otel_variables_change_nothing() {
    // SAFETY (edition 2021: not `unsafe`): the variables are set for this
    // test's duration; every other test must be unaffected by them anyway.
    for (k, v) in [
        ("OTEL_TRACES_SAMPLER", "always_off"),
        ("OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT", "1"),
        ("OTEL_SERVICE_NAME", "sentinel-service"),
        ("OTEL_RESOURCE_ATTRIBUTES", "host.id=sentinel-host"),
    ] {
        std::env::set_var(k, v);
    }
    let (tracing, capture) = capture(1.0, CallerMode::Parent, Service::Server);
    let mut span = tracing.request(None);
    span.set(Attr::RequestId("01J".into()));
    span.set(Attr::Endpoint(Endpoint::Chat));
    span.end();
    for k in [
        "OTEL_TRACES_SAMPLER",
        "OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT",
        "OTEL_SERVICE_NAME",
        "OTEL_RESOURCE_ATTRIBUTES",
    ] {
        std::env::remove_var(k);
    }
    let request = &capture.named("capyctl.request")[0];
    assert_eq!(request.attributes.len(), 2);
    let encoded = capture.encoded();
    assert!(!contains(&encoded, b"sentinel"));
    assert!(contains(&encoded, b"capyctl-server"));
}

// T48 (§8): a full queue drops and counts.
#[test]
fn full_queue_drops_and_counts() {
    let queue = Queue::new(2, 512);
    let (tracing, capture) = capture(1.0, CallerMode::Parent, Service::Server);
    for _ in 0..3 {
        tracing.request(None).end();
    }
    let spans = capture.spans.lock().unwrap().clone();
    for span in spans {
        queue.push(span);
    }
    assert_eq!(queue.len(), 2);
    assert_eq!(queue.stats().dropped, 1);
}

// T48 (§8): the notice is at most once a minute, and only for news.
#[test]
fn notices_are_rate_limited() {
    let mut notices = Notices::default();
    let now = tokio::time::Instant::now();
    let quiet = TraceStats::default();
    assert_eq!(notices.due(&quiet, now), None);
    let dropped = TraceStats {
        dropped: 3,
        export_failures: 1,
        last_export_error: Some("HTTP 503".into()),
    };
    let first = notices.due(&dropped, now).expect("notice");
    assert!(first.contains("3 spans dropped") && first.contains("HTTP 503"));
    let more = TraceStats {
        dropped: 4,
        ..dropped.clone()
    };
    assert_eq!(notices.due(&more, now + Duration::from_secs(30)), None);
    assert!(notices.due(&more, now + NOTICE_INTERVAL).is_some());
    assert_eq!(notices.due(&more, now + NOTICE_INTERVAL * 3), None);
}

/// What a test collector saw: each export's headers and body.
type Seen = Arc<Mutex<Vec<(HeaderMap, Vec<u8>)>>>;

/// A loopback collector recording each export's headers and body.
async fn collector(status: u16) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let app = axum::Router::new().route(
        "/v1/traces",
        axum::routing::post(move |headers: HeaderMap, body: axum::body::Bytes| {
            let record = record.clone();
            async move {
                record.lock().unwrap().push((headers, body.to_vec()));
                axum::http::StatusCode::from_u16(status).unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1/traces"), seen)
}

fn settings(endpoint: String) -> Settings {
    Settings {
        endpoint,
        headers: vec![("x-api-key".into(), "secret-value".into())],
        sample_ratio: 1.0,
        caller_mode: CallerMode::Parent,
        service: Service::Server,
    }
}

// T48 (§8): OTLP/HTTP protobuf with the configured headers, flushed on
// shutdown.
#[tokio::test]
async fn exports_protobuf_with_configured_headers() {
    let (endpoint, seen) = collector(200).await;
    let tracing = Tracing::start(settings(endpoint.clone())).unwrap();
    assert_eq!(tracing.endpoint(), Some(endpoint.as_str()));
    let mut span = tracing.request(None);
    span.set(Attr::RequestId("01JREQUEST".into()));
    span.end();
    tracing.shutdown().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (headers, body) = &seen[0];
    assert_eq!(headers["content-type"], "application/x-protobuf");
    assert_eq!(headers["x-api-key"], "secret-value");
    let decoded = ExportTraceServiceRequest::decode(body.as_slice()).unwrap();
    let span = &decoded.resource_spans[0].scope_spans[0].spans[0];
    assert_eq!(span.name, "capyctl.request");
    assert_eq!(tracing.stats().unwrap(), TraceStats::default());
}

// T48 (§8): a failing collector is counted, never retried past shutdown,
// and the failure text is the status class only.
#[tokio::test]
async fn export_failures_are_counted() {
    let (endpoint, _seen) = collector(400).await;
    let tracing = Tracing::start(settings(endpoint)).unwrap();
    tracing.request(None).end();
    tracing.shutdown().await;
    let stats = tracing.stats().unwrap();
    assert_eq!(stats.export_failures, 1);
    assert_eq!(stats.dropped, 1);
    assert_eq!(stats.last_export_error.as_deref(), Some("HTTP 400"));
}

// T48 (§8): an unreachable collector delays no request, and a stopping role
// waits at most 2 seconds for the flush.
#[tokio::test]
async fn unreachable_collector_delays_nothing() {
    // A listener that accepts and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            if let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        }
    });
    let tracing = Tracing::start(settings(format!("http://{addr}/v1/traces"))).unwrap();
    let started = std::time::Instant::now();
    for _ in 0..100 {
        let mut span = tracing.request(None);
        span.set(Attr::Outcome(Outcome::Completed));
        span.end();
    }
    assert!(started.elapsed() < Duration::from_millis(500));
    let stopping = std::time::Instant::now();
    tracing.shutdown().await;
    assert!(stopping.elapsed() < Duration::from_millis(2500));
}

// T48 (§8): bad settings refuse the start.
#[tokio::test]
async fn invalid_settings_refuse_the_start() {
    let mut bad = settings("ftp://127.0.0.1/v1/traces".into());
    assert_eq!(
        Tracing::start(bad.clone()).unwrap_err(),
        StartError::Endpoint
    );
    bad.endpoint = "http://127.0.0.1:4318/v1/traces".into();
    bad.headers = vec![("bad header".into(), "v".into())];
    assert_eq!(Tracing::start(bad).unwrap_err(), StartError::Header(1));
}

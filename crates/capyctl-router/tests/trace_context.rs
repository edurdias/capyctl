//! T48 (ADR 0037 §§1–3, 5, 6, 10): trace context at the router. Through the
//! real router and engine forwarder to an engine double that records what
//! reached it, with spans recorded by the test exporter. CPU tests, not
//! qualification.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use capyctl_config::effective::DeploymentApi;
use capyctl_controller::{
    LeaseEnd, LeaseRefused, LifecycleFault, RequestLease, RuntimeEndpoint, ServingInstance,
};
use capyctl_router::request_id::{CALLER_HEADER, CAPYCTL_HEADER};
use capyctl_router::{QueueLimits, RouterDeps};
use capyctl_trace::testing::{capture, contains, Capture};
use capyctl_trace::{CallerMode, Kind, Service, SpanCx, Tracing};
use serde_json::{json, Value};
use tower::ServiceExt;

const DEPLOYMENT: &str = "dep-trace";
const ROUTE: &str = "traced";
const SERVED: &str = "served-traced";
const GENERATION: i64 = 3;
const CALLER: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const CALLER_TRACE: [u8; 16] = [
    0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47, 0x36,
];
const CALLER_SPAN: [u8; 8] = [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7];

/// One READY deployment with one instance, local or behind a host's ingress.
struct Authority {
    api: DeploymentApi,
    endpoint: String,
    remote: bool,
    engine_trace: AtomicBool,
    trace_context: AtomicBool,
    leases: Mutex<Vec<Option<LeaseEnd>>>,
}

impl Authority {
    fn row(&self) -> capyctl_store::deployments::DeploymentRow {
        capyctl_store::deployments::DeploymentRow {
            id: DEPLOYMENT.into(),
            name: ROUTE.into(),
            kind: "sglang".into(),
            route_model_id: Some(ROUTE.into()),
            desired_state: capyctl_domain::LifecycleState::Ready,
            observed_state: capyctl_domain::LifecycleState::Ready,
            schema_version: 1,
            current_generation: GENERATION,
        }
    }

    fn runtime(&self) -> RuntimeEndpoint {
        RuntimeEndpoint {
            endpoint: self.endpoint.clone(),
            served_model: SERVED.into(),
            engine_key: Some("launch-key".into()),
            incarnation: "1".into(),
            engine: Some("sglang".into()),
            deployment_id: DEPLOYMENT.into(),
            revision: 1,
            request_abort: false,
            engine_trace: self.engine_trace.load(Ordering::SeqCst),
        }
    }
}

fn refuse(what: &str) -> LifecycleFault {
    LifecycleFault::Blocked(format!("the test authority cannot {what}"))
}

#[async_trait::async_trait]
impl capyctl_controller::LifecyclePort for Authority {
    fn find_deployment_by_route(
        &self,
        route: &str,
    ) -> Result<Option<capyctl_store::deployments::DeploymentRow>, LifecycleFault> {
        Ok((route == ROUTE).then(|| self.row()))
    }
    fn list_enabled_routes(
        &self,
    ) -> Result<Vec<capyctl_store::deployments::PublicRoute>, LifecycleFault> {
        Ok(vec![capyctl_store::deployments::PublicRoute {
            id: ROUTE.into(),
            api: self.api,
        }])
    }
    fn deployment_api(&self, _deployment: &str) -> Result<DeploymentApi, LifecycleFault> {
        Ok(self.api)
    }
    fn get_deployment(
        &self,
        id: &str,
    ) -> Result<Option<capyctl_store::deployments::DeploymentRow>, LifecycleFault> {
        Ok((id == DEPLOYMENT).then(|| self.row()))
    }
    fn latest_operation(
        &self,
        _d: &str,
    ) -> Result<Option<capyctl_store::deployments::OperationRow>, LifecycleFault> {
        Ok(None)
    }
    fn ready_deployments_excluding(&self, _d: &str) -> Result<Vec<String>, LifecycleFault> {
        Ok(Vec::new())
    }
    fn runtime_endpoint(
        &self,
        deployment: &str,
    ) -> Result<Option<RuntimeEndpoint>, LifecycleFault> {
        Ok((deployment == DEPLOYMENT).then(|| self.runtime()))
    }
    fn instance_endpoint(
        &self,
        deployment: &str,
        generation: i64,
    ) -> Result<Option<RuntimeEndpoint>, LifecycleFault> {
        Ok((deployment == DEPLOYMENT && generation == GENERATION).then(|| self.runtime()))
    }
    fn serving_instances(
        &self,
        _deployment: &str,
    ) -> Result<Option<Vec<ServingInstance>>, LifecycleFault> {
        Ok(Some(vec![ServingInstance {
            instance_index: 0,
            generation: GENERATION,
            host_id: Some("host-a".into()),
            remote_host: self.remote.then(|| "host-a".into()),
            launch_command_id: Some("launch-3".into()),
            dispatch_open: true,
            host_live: true,
            host_unresponsive: false,
            engine_exited: false,
            load: None,
            group: false,
            local_tensor_parallel: 1,
        }]))
    }
    fn trace_context(&self, _deployment: &str, _generation: i64) -> bool {
        self.trace_context.load(Ordering::SeqCst)
    }
    async fn open_request_lease(
        &self,
        _d: &str,
        _max: usize,
    ) -> Result<Option<RequestLease>, LeaseRefused> {
        Err(LeaseRefused::Unavailable("instances only".into()))
    }
    async fn open_instance_lease(
        &self,
        _deployment: &str,
        _generation: i64,
        _max: usize,
    ) -> Result<Option<RequestLease>, LeaseRefused> {
        let mut leases = self.leases.lock().unwrap();
        leases.push(None);
        Ok(Some(RequestLease::unrecorded(&format!(
            "01JLEASE{:018}",
            leases.len() - 1
        ))))
    }
    async fn close_request_lease(
        &self,
        lease: RequestLease,
        end: LeaseEnd,
    ) -> Result<(), LeaseRefused> {
        let index: usize = lease.id().trim_start_matches("01JLEASE").parse().unwrap();
        self.leases.lock().unwrap()[index] = Some(end);
        Ok(())
    }
    fn clear_suspension(&self, _d: &str) -> Result<(), LifecycleFault> {
        Err(refuse("clear a suspension"))
    }
    fn journal(
        &self,
        _h: Option<&str>,
        _o: Option<&str>,
        _s: Option<&str>,
        _e: &str,
    ) -> Result<(), LifecycleFault> {
        Err(refuse("journal"))
    }
    async fn observe_adapter(
        &self,
        _d: &str,
    ) -> Result<capyctl_adapters::traits::WorkObservation, LifecycleFault> {
        Err(refuse("observe"))
    }
    async fn idle_stop(
        &self,
        _d: &str,
    ) -> Result<capyctl_controller::OperationHandle, LifecycleFault> {
        Err(refuse("stop"))
    }
    async fn wait_terminal(
        &self,
        _h: &capyctl_controller::OperationHandle,
    ) -> Result<capyctl_domain::LifecycleState, LifecycleFault> {
        Err(refuse("wait"))
    }
    async fn auto_activate(
        &self,
        _d: &str,
    ) -> Result<capyctl_controller::OperationHandle, LifecycleFault> {
        Err(refuse("activate outside a request"))
    }
    async fn request_transition(
        &self,
        _d: &str,
        _a: capyctl_domain::LifecycleAction,
    ) -> Result<capyctl_controller::OperationHandle, LifecycleFault> {
        Err(refuse("transition"))
    }
    async fn activate_for_request(&self, _deployment: &str) -> Result<(), LifecycleFault> {
        Err(refuse("activate a deployment that is already ready"))
    }
}

/// What reached the engine: each request's headers and body.
type Seen = Arc<Mutex<Vec<(HeaderMap, String)>>>;

fn chunk(delta: Value, finish: Value) -> String {
    let chunk = json!({"id": "c1", "object": "chat.completion.chunk", "created": 1,
        "model": SERVED, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
    format!("data: {chunk}\n\n")
}

/// An engine double: a short stream for chat (an engine validation error
/// quoting the request when it carries `QUOTE-ME`), a decision answer for
/// `/v1/systemone`.
async fn engine() -> (String, Seen) {
    let seen: Seen = Arc::default();
    let recorded = seen.clone();
    let app = axum::Router::new().fallback(
        move |uri: axum::http::Uri, headers: HeaderMap, body: String| {
            let recorded = recorded.clone();
            async move {
                recorded.lock().unwrap().push((headers, body.clone()));
                let (status, content_type, answer) = if uri.path() == "/v1/systemone" {
                    let answer = json!({"model": SERVED,
                        "answers": {"angry": {"type": "noul", "noul": 0.5}},
                        "usage": {"input_tokens": 9, "output_tokens": 0}});
                    (200, "application/json", answer.to_string())
                } else if body.contains("QUOTE-ME") {
                    let error = json!({"object": "error",
                        "message": format!("invalid request: {body}"), "code": 400});
                    (400, "application/json", error.to_string())
                } else {
                    let stream = [
                        chunk(json!({"role": "assistant", "content": "hi"}), Value::Null),
                        chunk(json!({}), json!("stop")),
                        "data: [DONE]\n\n".to_owned(),
                    ]
                    .concat();
                    (200, "text/event-stream", stream)
                };
                axum::response::Response::builder()
                    .status(status)
                    .header("content-type", content_type)
                    .body(axum::body::Body::from(answer))
                    .unwrap()
            }
        },
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (base, seen)
}

struct Stack {
    authority: Arc<Authority>,
    seen: Seen,
    router: axum::Router,
}

async fn stack(api: DeploymentApi, remote: bool, tracing: Tracing, key: Option<&str>) -> Stack {
    let (endpoint, seen) = engine().await;
    let authority = Arc::new(Authority {
        api,
        endpoint,
        remote,
        engine_trace: AtomicBool::new(false),
        trace_context: AtomicBool::new(false),
        leases: Mutex::new(Vec::new()),
    });
    let port = authority.clone() as Arc<dyn capyctl_controller::LifecyclePort>;
    let router = capyctl_router::serve_router(RouterDeps {
        controller: port.clone(),
        forwards: Arc::new(capyctl_router::forwarders::LiveForwarders::new(port)),
        limits: QueueLimits {
            max_requests_per_deployment: 4,
            max_buffered_bytes_total: 1 << 20,
        },
        api_key: key.map(str::to_owned),
        inflight: Arc::new(capyctl_router::admission::InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
        tracing,
    });
    Stack {
        authority,
        seen,
        router,
    }
}

fn traced() -> (Tracing, Capture) {
    capture(1.0, CallerMode::Parent, Service::Server)
}

fn chat_body(stream: bool) -> Value {
    json!({"model": ROUTE, "stream": stream, "messages": [{"role": "user", "content": "hi"}]})
}

async fn send(
    router: &axum::Router,
    path: &str,
    body: &Value,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, String) {
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(
            request
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

/// The spans settle on their own tasks; wait until `name` is recorded.
async fn recorded(capture: &Capture, name: &str, count: usize) {
    for _ in 0..200 {
        if capture.named(name).len() >= count {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "{name} was recorded fewer than {count} times: {:?}",
        capture.spans()
    );
}

fn engine_headers(seen: &Seen) -> Vec<HeaderMap> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|(h, _)| h.clone())
        .collect()
}

// T48 (§1): with tracing off, nothing is read and nothing is sent: requests
// with and without a caller `traceparent` reach the engine with the same
// headers, none of them a trace header, even on a lane whose engine traces.
#[tokio::test]
async fn off_sends_no_trace_header_and_requests_are_identical() {
    let stack = stack(DeploymentApi::Chat, false, Tracing::off(), None).await;
    stack.authority.engine_trace.store(true, Ordering::SeqCst);
    let plain = send(
        &stack.router,
        "/v1/chat/completions",
        &chat_body(false),
        &[],
    )
    .await;
    let traced = send(
        &stack.router,
        "/v1/chat/completions",
        &chat_body(false),
        &[("traceparent", CALLER), ("tracestate", "vendor=1")],
    )
    .await;
    assert_eq!(plain.0, StatusCode::OK, "{}", plain.2);
    assert_eq!(traced.0, StatusCode::OK, "{}", traced.2);
    let seen = engine_headers(&stack.seen);
    assert_eq!(seen.len(), 2);
    let names = |h: &HeaderMap| {
        let mut names: Vec<String> = h.keys().map(|k| k.as_str().to_owned()).collect();
        names.sort();
        names
    };
    assert_eq!(names(&seen[0]), names(&seen[1]));
    for headers in &seen {
        assert!(!headers.contains_key("traceparent"), "{headers:?}");
        assert!(!headers.contains_key("tracestate"), "{headers:?}");
    }
}

// T48 T37 (§2): a request that fails authentication gets no span.
#[tokio::test]
async fn an_unauthenticated_request_records_nothing() {
    let (tracing, capture) = traced();
    let stack = stack(DeploymentApi::Chat, false, tracing, Some("the-key")).await;
    let (status, _, _) = send(
        &stack.router,
        "/v1/chat/completions",
        &chat_body(false),
        &[("traceparent", CALLER)],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(capture.spans().is_empty());
}

// T48 (§§3, 6, 10): a collected and a streamed chat request with a caller
// context produce §3's spans in the caller's trace, `capyctl.request_id` is
// the answer's `X-CapyCTL-Request-Id`, and an engine launched with engine
// tracing gets `traceparent` with the forward span as its parent and the
// caller's `tracestate`.
#[tokio::test]
async fn chat_spans_join_the_callers_trace_and_reach_a_tracing_engine() {
    for stream in [false, true] {
        let (tracing, capture) = traced();
        let stack = stack(DeploymentApi::Chat, false, tracing, None).await;
        stack.authority.engine_trace.store(true, Ordering::SeqCst);
        let (status, headers, text) = send(
            &stack.router,
            "/v1/chat/completions",
            &chat_body(stream),
            &[("traceparent", CALLER), ("tracestate", "vendor=1")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        recorded(&capture, "capyctl.settle", 1).await;
        recorded(&capture, "capyctl.request", 1).await;
        let request = capture.named("capyctl.request").remove(0);
        assert_eq!(request.kind, Kind::Server);
        assert_eq!(request.cx.trace_id, CALLER_TRACE);
        assert_eq!(request.parent, Some(CALLER_SPAN));
        assert_eq!(
            request.attr("capyctl.request_id"),
            headers.get(CAPYCTL_HEADER).map(|v| v.to_str().unwrap())
        );
        assert_eq!(request.attr("capyctl.endpoint"), Some("chat"));
        assert_eq!(request.attr("capyctl.outcome"), Some("completed"));
        assert_eq!(request.attr("capyctl.deployment"), Some(DEPLOYMENT));
        assert_eq!(request.attr("http.response.status_code"), Some("200"));
        assert_eq!(request.attr("http.route"), Some("/v1/chat/completions"));
        for name in ["capyctl.admission", "capyctl.dispatch", "capyctl.settle"] {
            let span = capture.named(name).remove(0);
            assert_eq!(span.kind, Kind::Internal, "{name}");
            assert_eq!(span.parent, Some(request.cx.span_id), "{name}");
            assert_eq!(span.cx.trace_id, CALLER_TRACE, "{name}");
        }
        let forward = capture.named("capyctl.forward").remove(0);
        assert_eq!(forward.kind, Kind::Client);
        assert_eq!(forward.parent, Some(request.cx.span_id));
        assert_eq!(forward.events, vec!["first_progress".to_owned()]);
        assert_eq!(forward.attr("capyctl.attempt"), Some("1"));
        assert!(capture.named("capyctl.queue_wait").is_empty());
        assert!(capture.named("capyctl.activation_wait").is_empty());
        let engine = engine_headers(&stack.seen).remove(0);
        let sent = SpanCx::parse_traceparent(engine["traceparent"].to_str().unwrap()).unwrap();
        assert_eq!(sent.trace_id, CALLER_TRACE);
        assert_eq!(sent.span_id, forward.cx.span_id);
        assert_eq!(engine["tracestate"], "vendor=1");
        assert!(
            capture.disallowed_keys().is_empty(),
            "{:?}",
            capture.spans()
        );
    }
}

// T48 (§6): an engine launched without engine tracing gets no trace header,
// and neither does a host that did not declare `trace_context`; one that did
// gets the forward span as parent.
#[tokio::test]
async fn trace_headers_reach_only_capable_lanes() {
    let (tracing, capture) = traced();
    let local = stack(DeploymentApi::Chat, false, tracing.clone(), None).await;
    let remote = stack(DeploymentApi::Chat, true, tracing, None).await;
    // The engine of a remote launch traces; its host did not declare
    // `trace_context`, so the router sends it nothing.
    remote.authority.engine_trace.store(true, Ordering::SeqCst);
    for stack in [&local, &remote] {
        let (status, _, text) = send(
            &stack.router,
            "/v1/chat/completions",
            &chat_body(false),
            &[("traceparent", CALLER)],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let engine = engine_headers(&stack.seen).remove(0);
        assert!(!engine.contains_key("traceparent"), "{engine:?}");
    }
    remote.authority.trace_context.store(true, Ordering::SeqCst);
    recorded(&capture, "capyctl.forward", 2).await;
    capture.clear();
    let (status, _, text) = send(
        &remote.router,
        "/v1/chat/completions",
        &chat_body(false),
        &[("traceparent", CALLER)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    recorded(&capture, "capyctl.forward", 1).await;
    let forward = capture.named("capyctl.forward").remove(0);
    let ingress = engine_headers(&remote.seen).remove(1);
    let sent = SpanCx::parse_traceparent(ingress["traceparent"].to_str().unwrap()).unwrap();
    assert_eq!(sent.span_id, forward.cx.span_id);
    assert!(!ingress.contains_key("tracestate"));
}

// T48 T19 (§2): a malformed `traceparent` starts a new trace; the request is
// answered as usual.
#[tokio::test]
async fn a_malformed_traceparent_starts_a_new_trace() {
    let (tracing, capture) = traced();
    let stack = stack(DeploymentApi::Chat, false, tracing, None).await;
    let (status, _, text) = send(
        &stack.router,
        "/v1/chat/completions",
        &chat_body(false),
        &[
            (
                "traceparent",
                "00-4BF92F3577B34DA6A3CE929D0E0E4736-00F067AA0BA902B7-01",
            ),
            ("tracestate", "vendor=1"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    recorded(&capture, "capyctl.request", 1).await;
    let request = capture.named("capyctl.request").remove(0);
    assert_eq!(request.parent, None);
    assert_ne!(request.cx.trace_id, CALLER_TRACE);
}

// T48 (§§3, 6): a decision is traced as a chat request is, and reaches a
// tracing engine with `traceparent`.
#[tokio::test]
async fn a_decision_is_traced() {
    let (tracing, capture) = traced();
    let stack = stack(DeploymentApi::Decisions, false, tracing, None).await;
    stack.authority.engine_trace.store(true, Ordering::SeqCst);
    let body = json!({"model": ROUTE, "state": "hi",
        "questions": {"angry": {"type": "noul", "instructions": "Is the customer angry?"}}});
    let (status, _, text) = send(&stack.router, "/v1/systemone", &body, &[]).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    recorded(&capture, "capyctl.settle", 1).await;
    let request = capture.named("capyctl.request").remove(0);
    assert_eq!(request.attr("capyctl.endpoint"), Some("decisions"));
    assert_eq!(request.attr("http.route"), Some("/v1/systemone"));
    let forward = capture.named("capyctl.forward").remove(0);
    let engine = engine_headers(&stack.seen).remove(0);
    let sent = SpanCx::parse_traceparent(engine["traceparent"].to_str().unwrap()).unwrap();
    assert_eq!(sent.span_id, forward.cx.span_id);
}

// T48 T21 (§5): requests whose messages, system prompt, tool arguments,
// `cache_salt`, `model` string, `X-Request-Id`, API key and an engine
// error body each carry a sentinel export no byte of any sentinel, and every
// key is allow-listed.
#[tokio::test]
async fn no_sentinel_reaches_a_span() {
    let (tracing, capture) = traced();
    let stack = stack(DeploymentApi::Chat, false, tracing, Some("SENTINEL-KEY")).await;
    let auth = ("authorization", "Bearer SENTINEL-KEY");
    let content = json!({"model": ROUTE, "stream": false, "cache_salt": "SENTINEL-SALT",
    "messages": [
        {"role": "system", "content": "SENTINEL-SYSTEM"},
        {"role": "user", "content": "SENTINEL-PROMPT"},
        {"role": "assistant", "content": null, "tool_calls": [{"id": "t1", "type": "function",
            "function": {"name": "f", "arguments": "{\"x\":\"SENTINEL-ARGS\"}"}}]},
        {"role": "tool", "tool_call_id": "t1", "content": "SENTINEL-TOOL"},
    ]});
    let (status, _, text) = send(
        &stack.router,
        "/v1/chat/completions",
        &content,
        &[auth, (CALLER_HEADER, "SENTINEL-CALLER-ID")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let unknown =
        json!({"model": "SENTINEL-MODEL", "messages": [{"role": "user", "content": "x"}]});
    let (status, _, _) = send(&stack.router, "/v1/chat/completions", &unknown, &[auth]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let quoted = json!({"model": ROUTE, "stream": false,
        "messages": [{"role": "user", "content": "QUOTE-ME SENTINEL-ENGINE-ERROR"}]});
    let (status, _, text) = send(&stack.router, "/v1/chat/completions", &quoted, &[auth]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(
        text.contains("SENTINEL-ENGINE-ERROR"),
        "the engine's error reaches the client"
    );
    recorded(&capture, "capyctl.request", 3).await;
    recorded(&capture, "capyctl.settle", 2).await;
    let encoded = capture.encoded();
    assert!(!contains(&encoded, b"SENTINEL"));
    assert!(capture.disallowed_keys().is_empty());
    let refused = capture
        .named("capyctl.request")
        .into_iter()
        .find(|s| s.attr("capyctl.code") == Some("unknown_model"))
        .expect("the unknown model's refusal");
    assert_eq!(refused.attr("capyctl.outcome"), Some("refused"));
    assert_eq!(refused.attr("capyctl.deployment"), None);
    assert_eq!(refused.error.as_deref(), Some("unknown_model"));
}

//! T48 (ADR 0037 §§1–6, 9, 10): trace context at the router. Through the
//! real router and engine forwarder to an engine double that records what
//! reached it, with spans recorded by the test exporter. CPU tests, not
//! qualification.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use capyctl_config::effective::DeploymentApi;
use capyctl_controller::load_table::{EngineGauges, LoadView};
use capyctl_controller::{
    LeaseEnd, LeaseRefused, LifecycleFault, RequestLease, RuntimeEndpoint, ServingInstance,
};
use capyctl_router::admission::InFlight;
use capyctl_router::queue::WaitLimits;
use capyctl_router::request_id::{CALLER_HEADER, CAPYCTL_HEADER};
use capyctl_router::{QueueLimits, RouterDeps};
use capyctl_trace::testing::{capture, contains, Capture, Recorded};
use capyctl_trace::{CallerMode, Kind, Outcome, Service, SpanCx, Tracing};
use futures::StreamExt;
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

/// One deployment with one instance, local or behind a host's ingress; READY
/// unless a test parks it, and then READY only once its activation is
/// released.
struct Authority {
    api: DeploymentApi,
    endpoint: String,
    remote: bool,
    engine_trace: AtomicBool,
    trace_context: AtomicBool,
    leases: Mutex<Vec<Option<LeaseEnd>>>,
    ready: AtomicBool,
    activations: AtomicUsize,
    release: tokio::sync::Semaphore,
    /// When set, a second instance (generation [`DEAD_GENERATION`]) whose
    /// engine refuses connections ranks first.
    dead: Mutex<Option<String>>,
}

/// The generation of the instance whose engine refuses connections.
const DEAD_GENERATION: i64 = 2;

fn lease_id(index: usize) -> String {
    format!("01JLEASE{index:018}")
}

impl Authority {
    fn row(&self) -> capyctl_store::deployments::DeploymentRow {
        let observed = if self.ready.load(Ordering::SeqCst) {
            capyctl_domain::LifecycleState::Ready
        } else {
            capyctl_domain::LifecycleState::Parked
        };
        capyctl_store::deployments::DeploymentRow {
            id: DEPLOYMENT.into(),
            name: ROUTE.into(),
            kind: "sglang".into(),
            route_model_id: Some(ROUTE.into()),
            desired_state: capyctl_domain::LifecycleState::Ready,
            observed_state: observed,
            schema_version: 1,
            current_generation: GENERATION,
        }
    }

    fn runtime(&self, endpoint: String) -> RuntimeEndpoint {
        RuntimeEndpoint {
            endpoint,
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
        Ok((deployment == DEPLOYMENT).then(|| self.runtime(self.endpoint.clone())))
    }
    fn instance_endpoint(
        &self,
        deployment: &str,
        generation: i64,
    ) -> Result<Option<RuntimeEndpoint>, LifecycleFault> {
        if deployment != DEPLOYMENT {
            return Ok(None);
        }
        Ok(match generation {
            GENERATION => Some(self.runtime(self.endpoint.clone())),
            DEAD_GENERATION => self.dead.lock().unwrap().clone().map(|e| self.runtime(e)),
            _ => None,
        })
    }
    fn serving_instances(
        &self,
        _deployment: &str,
    ) -> Result<Option<Vec<ServingInstance>>, LifecycleFault> {
        let instance = |index: u32, generation: i64, host: &str| ServingInstance {
            instance_index: index,
            generation,
            host_id: Some(host.into()),
            remote_host: self.remote.then(|| host.into()),
            launch_command_id: Some(format!("launch-{generation}")),
            dispatch_open: true,
            host_live: true,
            host_unresponsive: false,
            engine_exited: false,
            load: None,
            group: false,
            local_tensor_parallel: 1,
        };
        let mut live = instance(0, GENERATION, "host-a");
        if self.dead.lock().unwrap().is_none() {
            return Ok(Some(vec![live]));
        }
        // A busy engine on the live instance, so the dead one ranks first.
        live.load = Some(LoadView {
            deployment_id: DEPLOYMENT.into(),
            generation: GENERATION,
            host_id: "host-a".into(),
            owned_handle: format!("launch-{GENERATION}"),
            sampled_at_ms: 0,
            age_ms: 500,
            fresh: true,
            ingress_in_flight: 0,
            engine: Some(EngineGauges {
                running: 3,
                waiting: 0,
                kv_usage_ppm: 0,
            }),
            max_running: None,
        });
        Ok(Some(vec![live, instance(1, DEAD_GENERATION, "host-b")]))
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
        Ok(Some(RequestLease::unrecorded(&lease_id(leases.len() - 1))))
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
    async fn activate_for_request(&self, deployment: &str) -> Result<(), LifecycleFault> {
        assert_eq!(deployment, DEPLOYMENT);
        self.activations.fetch_add(1, Ordering::SeqCst);
        self.release.acquire().await.unwrap().forget();
        self.ready.store(true, Ordering::SeqCst);
        Ok(())
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
/// `/v1/systemone`. A chat request carrying `SLOW` is answered after 400 ms;
/// one carrying `HANG` gets a first chunk and then nothing, and one carrying
/// `SILENT` gets nothing at all, with the stream left open.
async fn engine() -> (String, Seen) {
    let seen: Seen = Arc::default();
    let recorded = seen.clone();
    let app = axum::Router::new().fallback(
        move |uri: axum::http::Uri, headers: HeaderMap, body: String| {
            let recorded = recorded.clone();
            async move {
                recorded.lock().unwrap().push((headers, body.clone()));
                if body.contains("SLOW") {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                if body.contains("HANG") || body.contains("SILENT") {
                    let first = if body.contains("HANG") {
                        chunk(json!({"role": "assistant", "content": "hi"}), Value::Null)
                    } else {
                        String::new()
                    };
                    let open =
                        futures::stream::once(
                            async move { Ok::<_, std::convert::Infallible>(first) },
                        )
                        .chain(futures::stream::pending());
                    return axum::response::Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from_stream(open))
                        .unwrap();
                }
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
    inflight: Arc<InFlight>,
}

async fn stack(api: DeploymentApi, remote: bool, tracing: Tracing, key: Option<&str>) -> Stack {
    stack_bounded(api, remote, tracing, key, 4).await
}

/// As [`stack`], with `in_flight` requests per deployment at most.
async fn stack_bounded(
    api: DeploymentApi,
    remote: bool,
    tracing: Tracing,
    key: Option<&str>,
    in_flight: usize,
) -> Stack {
    let (endpoint, seen) = engine().await;
    let authority = Arc::new(Authority {
        api,
        endpoint,
        remote,
        engine_trace: AtomicBool::new(false),
        trace_context: AtomicBool::new(false),
        leases: Mutex::new(Vec::new()),
        ready: AtomicBool::new(true),
        activations: AtomicUsize::new(0),
        release: tokio::sync::Semaphore::new(0),
        dead: Mutex::new(None),
    });
    let port = authority.clone() as Arc<dyn capyctl_controller::LifecyclePort>;
    let inflight = Arc::new(InFlight::default());
    let router = capyctl_router::serve_router(RouterDeps {
        controller: port.clone(),
        forwards: Arc::new(capyctl_router::forwarders::LiveForwarders::new(port)),
        limits: QueueLimits {
            max_requests_per_deployment: in_flight,
            max_buffered_bytes_total: 1 << 20,
        },
        api_key: key.map(str::to_owned),
        inflight: inflight.clone(),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
        tracing,
    });
    Stack {
        authority,
        seen,
        router,
        inflight,
    }
}

impl Stack {
    /// Change the router's waiting limits.
    fn limits(&self, change: impl FnOnce(&mut WaitLimits)) {
        let mut limits = self.inflight.waiting.limits();
        change(&mut limits);
        self.inflight.waiting.set_limits(limits);
    }
}

fn traced() -> (Tracing, Capture) {
    capture(1.0, CallerMode::Parent, Service::Server)
}

fn chat_body(stream: bool) -> Value {
    chat_saying(stream, "hi")
}

fn chat_saying(stream: bool, content: &str) -> Value {
    json!({"model": ROUTE, "stream": stream, "messages": [{"role": "user", "content": content}]})
}

fn post(
    path: &str,
    body: &Value,
    headers: &[(&str, &str)],
) -> axum::http::Request<axum::body::Body> {
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

async fn send(
    router: &axum::Router,
    path: &str,
    body: &Value,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, String) {
    let response = router
        .clone()
        .oneshot(post(path, body, headers))
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

/// Wait until `ready` holds (the router works on its own tasks).
async fn until(what: &str, ready: impl Fn() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting until {what}");
}

/// How the `index`th lease was closed, once it is.
async fn closed(authority: &Authority, index: usize) -> LeaseEnd {
    until("the lease is closed", || {
        matches!(authority.leases.lock().unwrap().get(index), Some(Some(_)))
    })
    .await;
    authority.leases.lock().unwrap()[index].unwrap()
}

/// An address nothing listens on: a connection to it is refused.
async fn dead_endpoint() -> String {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    drop(socket);
    base
}

/// Where `span` is in the order the spans ended.
fn ended_at(capture: &Capture, span: &Recorded) -> usize {
    capture
        .spans()
        .iter()
        .position(|s| s.cx.span_id == span.cx.span_id)
        .unwrap()
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

// T48 T38 (ADR 0037 §3): a refusal before acceptance on the first instance
// fails over; the second attempt adds a `capyctl.dispatch` and a
// `capyctl.forward` under the same `capyctl.request` and request ID, with
// `capyctl.attempt` 1 and 2.
#[tokio::test]
async fn a_failover_adds_a_dispatch_and_a_forward_under_the_same_request() {
    let (tracing, capture) = traced();
    let stack = stack(DeploymentApi::Chat, false, tracing, None).await;
    *stack.authority.dead.lock().unwrap() = Some(dead_endpoint().await);
    let (status, headers, text) = send(
        &stack.router,
        "/v1/chat/completions",
        &chat_body(false),
        &[("traceparent", CALLER)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    recorded(&capture, "capyctl.settle", 2).await;
    recorded(&capture, "capyctl.request", 1).await;
    assert_eq!(
        *stack.authority.leases.lock().unwrap(),
        vec![Some(LeaseEnd::NotAccepted), Some(LeaseEnd::Completed)]
    );
    assert_eq!(stack.seen.lock().unwrap().len(), 1, "only the live engine");
    let requests = capture.named("capyctl.request");
    assert_eq!(requests.len(), 1, "{:?}", capture.spans());
    let request = &requests[0];
    assert_eq!(request.attr("capyctl.outcome"), Some("completed"));
    assert_eq!(
        request.attr("capyctl.request_id"),
        headers.get(CAPYCTL_HEADER).map(|v| v.to_str().unwrap())
    );
    for name in ["capyctl.dispatch", "capyctl.forward", "capyctl.settle"] {
        let mut spans = capture.named(name);
        spans.sort_by_key(|s| s.attr("capyctl.attempt").map(str::to_owned));
        let attempts: Vec<_> = spans.iter().map(|s| s.attr("capyctl.attempt")).collect();
        assert_eq!(attempts, vec![Some("1"), Some("2")], "{name}");
        for span in &spans {
            assert_eq!(span.parent, Some(request.cx.span_id), "{name}");
            assert_eq!(span.cx.trace_id, CALLER_TRACE, "{name}");
        }
        if name != "capyctl.settle" {
            assert_eq!(spans[0].attr("capyctl.generation"), Some("2"), "{name}");
            assert_eq!(spans[1].attr("capyctl.generation"), Some("3"), "{name}");
        }
    }
    let mut forwards = capture.named("capyctl.forward");
    forwards.sort_by_key(|s| s.attr("capyctl.attempt").map(str::to_owned));
    assert_eq!(forwards[0].attr("capyctl.outcome"), Some("refused"));
    assert_eq!(forwards[0].error.as_deref(), Some("unavailable"));
    assert!(forwards[0].events.is_empty());
    assert_eq!(forwards[1].attr("capyctl.outcome"), Some("completed"));
    assert_eq!(forwards[1].error, None);
    assert_eq!(forwards[1].events, vec!["first_progress".to_owned()]);
}

// T48 T19 (ADR 0037 §3): a request that finds the in-flight bound reached
// waits for a slot; its `capyctl.queue_wait` is a child of its request span
// and ends `completed` when the slot is granted. The request that held the
// slot records none.
#[tokio::test]
async fn a_request_waiting_for_a_slot_records_its_queue_wait() {
    let (tracing, capture) = traced();
    let stack = stack_bounded(DeploymentApi::Chat, false, tracing, None, 1).await;
    let router = stack.router.clone();
    let holder = tokio::spawn(async move {
        send(
            &router,
            "/v1/chat/completions",
            &chat_saying(false, "SLOW"),
            &[],
        )
        .await
    });
    until("the engine has the first request", || {
        stack.seen.lock().unwrap().len() == 1
    })
    .await;
    let router = stack.router.clone();
    let waiter =
        tokio::spawn(
            async move { send(&router, "/v1/chat/completions", &chat_body(false), &[]).await },
        );
    until("the second request waits", || {
        stack.inflight.waiting.waiting(DEPLOYMENT) == 1
    })
    .await;
    assert!(capture.named("capyctl.queue_wait").is_empty(), "still open");
    let (status, _, text) = holder.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{text}");
    let (status, headers, text) = waiter.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{text}");
    recorded(&capture, "capyctl.request", 2).await;
    let waits = capture.named("capyctl.queue_wait");
    assert_eq!(waits.len(), 1, "{:?}", capture.spans());
    let wait = &waits[0];
    assert_eq!(wait.kind, Kind::Internal);
    assert_eq!(wait.attr("capyctl.outcome"), Some("completed"));
    assert_eq!(wait.error, None);
    let request = capture
        .named("capyctl.request")
        .into_iter()
        .find(|r| r.cx.span_id == wait.parent.unwrap())
        .expect("the queue wait's parent is a request span");
    assert_eq!(request.cx.trace_id, wait.cx.trace_id);
    assert_eq!(
        request.attr("capyctl.request_id"),
        headers.get(CAPYCTL_HEADER).map(|v| v.to_str().unwrap()),
        "the request that waited"
    );
}

// T48 T19 (ADR 0037 §3): a request still waiting for a slot at its deadline
// is refused `queue_full`; its `capyctl.queue_wait` ends failed with that
// code, under the request span, which is refused with it.
#[tokio::test]
async fn a_queue_full_refusal_fails_the_queue_wait() {
    let (tracing, capture) = traced();
    let stack = stack_bounded(DeploymentApi::Chat, false, tracing, None, 1).await;
    let router = stack.router.clone();
    let holder = tokio::spawn(async move {
        send(
            &router,
            "/v1/chat/completions",
            &chat_saying(false, "SLOW"),
            &[],
        )
        .await
    });
    until("the engine has the first request", || {
        stack.seen.lock().unwrap().len() == 1
    })
    .await;
    // The first request's bounds are already set; only the second request's
    // deadline is short.
    stack.limits(|l| l.deadline = Duration::from_millis(150));
    let (status, _, text) = send(
        &stack.router,
        "/v1/chat/completions",
        &chat_body(false),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{text}");
    recorded(&capture, "capyctl.queue_wait", 1).await;
    recorded(&capture, "capyctl.request", 1).await;
    let wait = capture.named("capyctl.queue_wait").remove(0);
    assert_eq!(wait.error.as_deref(), Some("queue_full"));
    assert_eq!(wait.attr("capyctl.code"), Some("queue_full"));
    let request = capture.named("capyctl.request").remove(0);
    assert_eq!(wait.parent, Some(request.cx.span_id));
    assert_eq!(request.attr("capyctl.code"), Some("queue_full"));
    assert_eq!(request.attr("capyctl.outcome"), Some("refused"));
    let (status, _, text) = holder.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{text}");
}

/// ADR 0037 §3: a cut request's `capyctl.settle` is held while its
/// cancellation is unsettled and exported only once
/// [`Tracing::settled`] names its lease, with the request span as parent,
/// after the request span ended.
async fn settles_after_the_cut(
    stack: &Stack,
    tracing: &Tracing,
    capture: &Capture,
    cause: &str,
    answered: bool,
) {
    assert_eq!(
        closed(&stack.authority, 0).await,
        LeaseEnd::Cancelling { answered }
    );
    recorded(capture, "capyctl.request", 1).await;
    recorded(capture, "capyctl.forward", 1).await;
    let request = capture.named("capyctl.request").remove(0);
    assert_eq!(request.attr("capyctl.outcome"), Some("cancelling"));
    assert_eq!(request.attr("capyctl.cancel_cause"), Some(cause));
    let forward = capture.named("capyctl.forward").remove(0);
    assert_eq!(forward.attr("capyctl.outcome"), Some("cancelling"));
    assert_eq!(forward.attr("capyctl.cancel_cause"), Some(cause));
    assert!(
        capture.named("capyctl.settle").is_empty(),
        "held until the cancellation settles: {:?}",
        capture.spans()
    );
    // Another lease's settlement ends nothing.
    tracing.settled(&lease_id(1), Outcome::Completed);
    assert!(capture.named("capyctl.settle").is_empty());
    tracing.settled(&lease_id(0), Outcome::Completed);
    let settles = capture.named("capyctl.settle");
    assert_eq!(settles.len(), 1, "{:?}", capture.spans());
    let settle = &settles[0];
    assert_eq!(settle.kind, Kind::Internal);
    assert_eq!(settle.parent, Some(request.cx.span_id));
    assert_eq!(settle.cx.trace_id, request.cx.trace_id);
    assert_eq!(settle.attr("capyctl.outcome"), Some("completed"));
    assert_eq!(settle.attr("capyctl.attempt"), Some("1"));
    assert!(ended_at(capture, &request) < ended_at(capture, settle));
    // Settled once: a second settlement records nothing more.
    tracing.settled(&lease_id(0), Outcome::Completed);
    assert_eq!(capture.named("capyctl.settle").len(), 1);
}

// T48 T17 (ADR 0037 §3): a stream whose client hangs up is cut; its
// `capyctl.settle` is exported only when the cancellation settles.
#[tokio::test]
async fn a_hang_up_settles_later_under_the_request() {
    let (tracing, capture) = traced();
    let stack = stack(DeploymentApi::Chat, false, tracing.clone(), None).await;
    let response = stack
        .router
        .clone()
        .oneshot(post(
            "/v1/chat/completions",
            &chat_saying(true, "HANG"),
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let mut read = String::new();
    while !read.contains("hi") {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("the first chunk reaches the client")
            .expect("the stream is open")
            .unwrap();
        read.push_str(&String::from_utf8_lossy(&frame));
    }
    drop(body);
    settles_after_the_cut(&stack, &tracing, &capture, "client_hangup", true).await;
}

// T48 T17 (ADR 0037 §3): a stream with no output by its deadline is cut;
// its `capyctl.settle` is exported only when the cancellation settles.
#[tokio::test]
async fn a_deadline_cut_settles_later_under_the_request() {
    let (tracing, capture) = traced();
    let stack = stack(DeploymentApi::Chat, false, tracing.clone(), None).await;
    stack.limits(|l| l.deadline = Duration::from_millis(300));
    tokio::time::timeout(
        Duration::from_secs(10),
        send(
            &stack.router,
            "/v1/chat/completions",
            &chat_saying(true, "SILENT"),
            &[],
        ),
    )
    .await
    .expect("the cut answers the client");
    settles_after_the_cut(&stack, &tracing, &capture, "deadline", false).await;
}

// T48 T17 (ADR 0037 §3): a stream that stops producing past the idle bound
// is cut; its `capyctl.settle` is exported only when the cancellation
// settles.
#[tokio::test]
async fn an_idle_cut_settles_later_under_the_request() {
    let (tracing, capture) = traced();
    let stack = stack(DeploymentApi::Chat, false, tracing.clone(), None).await;
    stack.limits(|l| l.stream_idle = Duration::from_millis(300));
    tokio::time::timeout(
        Duration::from_secs(10),
        send(
            &stack.router,
            "/v1/chat/completions",
            &chat_saying(true, "HANG"),
            &[],
        ),
    )
    .await
    .expect("the cut answers the client");
    settles_after_the_cut(&stack, &tracing, &capture, "stream_idle", true).await;
}

/// Park the stack's deployment, send one request per entry of `callers` (a
/// caller `traceparent` or none) so that the first starts the activation and
/// the others join it while it runs, then release it. Every request is
/// answered.
async fn through_one_activation(stack: &Stack, callers: &[Option<String>]) {
    stack.authority.ready.store(false, Ordering::SeqCst);
    let mut requests = Vec::new();
    for (joined, caller) in callers.iter().enumerate() {
        let router = stack.router.clone();
        let caller = caller.clone();
        requests.push(tokio::spawn(async move {
            let headers: Vec<(&str, &str)> = caller
                .as_deref()
                .map(|c| ("traceparent", c))
                .into_iter()
                .collect();
            send(&router, "/v1/chat/completions", &chat_body(false), &headers).await
        }));
        until("the request waits for the activation", || {
            stack.inflight.waiting.waiting(DEPLOYMENT) == joined + 1
                && stack.authority.activations.load(Ordering::SeqCst) == 1
        })
        .await;
    }
    stack.authority.release.add_permits(1);
    for request in requests {
        let (status, _, text) = request.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{text}");
    }
    assert_eq!(stack.authority.activations.load(Ordering::SeqCst), 1);
}

// T48 (ADR 0037 §§4, 9): at sample ratio 0 an activation is still recorded,
// as the root of its own trace; the unsampled requests that waited for it
// record nothing.
#[tokio::test]
async fn an_activation_is_recorded_at_ratio_zero_and_its_requests_are_not() {
    let (tracing, capture) = capture(0.0, CallerMode::Parent, Service::Server);
    let stack = stack(DeploymentApi::Chat, false, tracing, None).await;
    through_one_activation(&stack, &[None, None, None]).await;
    recorded(&capture, "capyctl.activation", 1).await;
    let spans = capture.spans();
    assert_eq!(spans.len(), 1, "only the activation: {spans:?}");
    let activation = &spans[0];
    assert_eq!(activation.name, "capyctl.activation");
    assert_eq!(activation.parent, None);
    assert!(activation.cx.sampled());
    assert_eq!(activation.attr("capyctl.outcome"), Some("completed"));
}

// T48 (ADR 0037 §§4, 9): at sample ratio 0, requests whose callers sampled
// them are recorded; each one's `capyctl.activation_wait` is a child of its
// request span and links to the one `capyctl.activation` root, in its own
// trace, with `capyctl.activation.started` true for the request that
// started it and false for those that joined.
#[tokio::test]
async fn joined_requests_link_to_the_one_activation_at_ratio_zero() {
    let (tracing, capture) = capture(0.0, CallerMode::Parent, Service::Server);
    let stack = stack(DeploymentApi::Chat, false, tracing, None).await;
    let caller = |n: u64| format!("00-4bf92f3577b34da6a3ce929d0e0e4736-{n:016x}-01");
    through_one_activation(&stack, &[Some(caller(1)), Some(caller(2)), Some(caller(3))]).await;
    recorded(&capture, "capyctl.activation", 1).await;
    recorded(&capture, "capyctl.request", 3).await;
    recorded(&capture, "capyctl.activation_wait", 3).await;
    let activations = capture.named("capyctl.activation");
    assert_eq!(activations.len(), 1, "{:?}", capture.spans());
    let activation = &activations[0];
    assert_eq!(activation.parent, None);
    assert_ne!(activation.cx.trace_id, CALLER_TRACE, "its own trace");
    let requests = capture.named("capyctl.request");
    for wait in capture.named("capyctl.activation_wait") {
        let request = requests
            .iter()
            .find(|r| Some(r.cx.span_id) == wait.parent)
            .expect("each wait is a child of its request span");
        assert_eq!(wait.cx.trace_id, CALLER_TRACE);
        assert_eq!(wait.attr("capyctl.outcome"), Some("completed"));
        let links: Vec<_> = wait.links.iter().map(|l| (l.trace_id, l.span_id)).collect();
        assert_eq!(
            links,
            vec![(activation.cx.trace_id, activation.cx.span_id)],
            "linked to the one activation"
        );
        let starter = request.parent == Some(1u64.to_be_bytes());
        assert_eq!(
            wait.attr("capyctl.activation.started"),
            Some(if starter { "true" } else { "false" }),
            "{request:?}"
        );
    }
}

//! T47 (ADR 0036 §§3-4, 9): `POST /v1/embeddings` is validated, routed,
//! admitted, leased and settled like a decision, through the real engine
//! forwarder to an engine double; its answer is streamed through to the
//! client, checked as it passes, and closed with `capyctl.metrics` and its
//! brace only when it passed; the endpoints that do not answer a deployment's
//! API are refused before activation; a cut request is read to its end and
//! settles on its answer. CPU tests, not qualification.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use capyctl_config::effective::DeploymentApi;
use capyctl_controller::{LeaseEnd, LeaseRefused, LifecycleFault, RequestLease};
use capyctl_router::{QueueLimits, RouterDeps};
use serde_json::{json, Value};
use tower::ServiceExt;

const DEPLOYMENT: &str = "dep-embed";
const ROUTE: &str = "embed";
const SERVED: &str = "served-embed";
const GENERATION: i64 = 7;

/// One deployment serving `api` on `engine`, parked until a request activates
/// it, whose one instance runs at `endpoint`. Every lease it grants is
/// recorded with its end, and every cut lease settled on its answer.
struct Authority {
    api: DeploymentApi,
    engine: &'static str,
    /// ADR 0036 §5: the native dimension the instance's reference recorded.
    dimension: Option<usize>,
    ready: AtomicBool,
    activations: AtomicUsize,
    endpoint: String,
    leases: Mutex<Vec<Option<LeaseEnd>>>,
    /// ADR 0036 §4: leases settled on their answer after a cut.
    settled: Mutex<Vec<String>>,
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
            name: "embed".into(),
            kind: self.engine.into(),
            route_model_id: Some(ROUTE.into()),
            desired_state: capyctl_domain::LifecycleState::Ready,
            observed_state: observed,
            schema_version: 1,
            current_generation: 1,
        }
    }
    fn ends(&self) -> Vec<Option<LeaseEnd>> {
        self.leases.lock().unwrap().clone()
    }
    fn settled(&self) -> Vec<String> {
        self.settled.lock().unwrap().clone()
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
    fn deployment_api(&self, deployment: &str) -> Result<DeploymentApi, LifecycleFault> {
        assert_eq!(deployment, DEPLOYMENT);
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
    ) -> Result<Option<capyctl_controller::RuntimeEndpoint>, LifecycleFault> {
        Ok(
            (deployment == DEPLOYMENT).then(|| capyctl_controller::RuntimeEndpoint {
                endpoint: self.endpoint.clone(),
                served_model: SERVED.into(),
                engine_key: Some("launch-key".into()),
                incarnation: "1".into(),
                engine: Some(self.engine.into()),
                deployment_id: DEPLOYMENT.into(),
                revision: 1,
                request_abort: false,
            }),
        )
    }
    fn serving_instances(
        &self,
        _deployment: &str,
    ) -> Result<Option<Vec<capyctl_controller::ServingInstance>>, LifecycleFault> {
        let ready = self.ready.load(Ordering::SeqCst);
        Ok(Some(
            ready
                .then_some(capyctl_controller::ServingInstance {
                    instance_index: 0,
                    generation: GENERATION,
                    host_id: None,
                    remote_host: None,
                    launch_command_id: None,
                    dispatch_open: true,
                    host_live: true,
                    host_unresponsive: false,
                    engine_exited: false,
                    load: None,
                    group: false,
                    local_tensor_parallel: 1,
                })
                .into_iter()
                .collect(),
        ))
    }
    fn embedding_dimension(
        &self,
        deployment: &str,
        generation: i64,
    ) -> Result<Option<usize>, LifecycleFault> {
        assert_eq!((deployment, generation), (DEPLOYMENT, GENERATION));
        Ok(self.dimension)
    }
    async fn open_request_lease(
        &self,
        _d: &str,
        _max: usize,
    ) -> Result<Option<RequestLease>, LeaseRefused> {
        assert!(
            self.ready.load(Ordering::SeqCst),
            "SPEC §10: nothing is dispatched before the deployment is READY"
        );
        let mut leases = self.leases.lock().unwrap();
        leases.push(None);
        Ok(Some(RequestLease::unrecorded(
            &(leases.len() - 1).to_string(),
        )))
    }
    async fn close_request_lease(
        &self,
        lease: RequestLease,
        end: LeaseEnd,
    ) -> Result<(), LeaseRefused> {
        let index: usize = lease.id().parse().unwrap();
        let mut leases = self.leases.lock().unwrap();
        assert!(leases[index].is_none(), "a lease is closed once");
        leases[index] = Some(end);
        Ok(())
    }
    async fn settle_answered_cut(&self, lease_id: &str) -> Result<bool, LeaseRefused> {
        let index: usize = lease_id.parse().unwrap();
        assert!(
            matches!(
                self.leases.lock().unwrap()[index],
                Some(LeaseEnd::Cancelling { .. })
            ),
            "only a cancelling lease settles on its answer"
        );
        self.settled.lock().unwrap().push(lease_id.to_owned());
        Ok(true)
    }
    async fn acknowledge_cancelled_request(&self, _lease: &str) -> Result<bool, LeaseRefused> {
        panic!("ADR 0036 §4: an embeddings request is never aborted")
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
        self.ready.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// How the engine double answers every request.
#[derive(Clone)]
enum Reply {
    /// A status, content type and body, whole.
    Answer(u16, &'static str, String),
    /// HTTP 200 JSON: `head`, then nothing until the gate opens, then `rest`
    /// (or, when `rest` is `None`, a transfer that breaks).
    Paused {
        head: String,
        rest: Option<String>,
        gate: Arc<tokio::sync::Notify>,
    },
}

/// What reached the engine (path and body), and how its answers ended:
/// `finished` counts answers sent to their end, `dropped` answers whose
/// connection closed before they were.
#[derive(Clone, Default)]
struct Seen {
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    finished: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

/// Counts an answer body as dropped unless it was sent to its end.
struct Watch {
    seen: Seen,
    ended: bool,
}

impl Drop for Watch {
    fn drop(&mut self) {
        let counter = if self.ended {
            &self.seen.finished
        } else {
            &self.seen.dropped
        };
        counter.fetch_add(1, Ordering::SeqCst);
    }
}

async fn engine(reply: Reply) -> (String, Seen) {
    let seen = Seen::default();
    let recorded = seen.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
        let reply = reply.clone();
        let seen = recorded.clone();
        async move {
            seen.requests.lock().unwrap().push((
                uri.path().to_owned(),
                serde_json::from_slice(&body).unwrap_or(Value::Null),
            ));
            let (status, media, body) = match reply {
                Reply::Answer(status, media, body) => (status, media, axum::body::Body::from(body)),
                Reply::Paused { head, rest, gate } => {
                    let stream = async_stream::stream! {
                        let mut watch = Watch { seen, ended: false };
                        if !head.is_empty() {
                            yield Ok::<_, std::io::Error>(axum::body::Bytes::from(head));
                        }
                        gate.notified().await;
                        match rest {
                            Some(rest) => {
                                yield Ok(axum::body::Bytes::from(rest));
                                watch.ended = true;
                            }
                            None => yield Err(std::io::Error::other("the engine went away")),
                        }
                    };
                    (
                        200,
                        "application/json",
                        axum::body::Body::from_stream(stream),
                    )
                }
            };
            axum::response::Response::builder()
                .status(status)
                .header("content-type", media)
                .body(body)
                .unwrap()
        }
    });
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (base, seen)
}

/// A vLLM-shaped answer for two inputs, `model` after `data` (the engine's
/// name, which the router replaces), split where `split` says.
fn answer() -> String {
    r#"{"id":"embd-1","object":"list","created":5,"data":[{"index":0,"object":"embedding","embedding":[0.5,-0.25,0.125]},{"index":1,"object":"embedding","embedding":[1e-2,3,0]}],"model":"/srv/model","usage":{"prompt_tokens":9,"total_tokens":9}}"#.to_owned()
}

/// `answer()` cut after its first entry, and the rest.
fn halves() -> (String, String) {
    let whole = answer();
    let at = whole.find(r#",{"index":1"#).unwrap();
    (whole[..at].to_owned(), whole[at..].to_owned())
}

fn embed_body() -> Value {
    json!({"model": ROUTE, "input": ["first text", "second text"]})
}

fn authority(api: DeploymentApi, engine: &'static str, endpoint: &str) -> Arc<Authority> {
    Arc::new(Authority {
        api,
        engine,
        dimension: None,
        ready: AtomicBool::new(false),
        activations: AtomicUsize::new(0),
        endpoint: endpoint.into(),
        leases: Mutex::new(Vec::new()),
        settled: Mutex::new(Vec::new()),
    })
}

fn stack_on(authority: Arc<Authority>) -> (Arc<Authority>, RouterDeps, axum::Router) {
    let port = authority.clone() as Arc<dyn capyctl_controller::LifecyclePort>;
    let deps = RouterDeps {
        controller: port.clone(),
        forwards: Arc::new(capyctl_router::forwarders::LiveForwarders::new(port)),
        limits: QueueLimits {
            max_requests_per_deployment: 4,
            max_buffered_bytes_total: 1 << 20,
        },
        api_key: None,
        inflight: Arc::new(capyctl_router::admission::InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    };
    let router = capyctl_router::serve_router(deps.clone());
    (authority, deps, router)
}

fn stack(api: DeploymentApi, endpoint: &str) -> (Arc<Authority>, RouterDeps, axum::Router) {
    stack_on(authority(api, "vllm", endpoint))
}

/// Status, headers and raw body of one request through `router`.
async fn send_raw(
    router: &axum::Router,
    path: &str,
    body: Vec<u8>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(if body.is_empty() { "GET" } else { "POST" })
                .uri(path)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, headers, bytes.to_vec())
}

async fn send(router: &axum::Router, path: &str, body: Vec<u8>) -> (StatusCode, Value) {
    let (status, _, bytes) = send_raw(router, path, body).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn embed(router: &axum::Router, body: &Value) -> (StatusCode, Value) {
    send(router, "/v1/embeddings", body.to_string().into_bytes()).await
}

/// Poll `ready` until it holds, within 5 s.
async fn until(what: &str, ready: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(tokio::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Serve `router` on a loopback socket; its address.
async fn serve(router: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    address
}

// T47 (ADR 0036 §§3-4, 9): an embeddings request wakes its deployment, is
// leased and forwarded as one JSON body with only `model` replaced by the
// served name, and its answer comes back with the public name and nothing
// else rewritten, with no `Content-Length`; `capyctl.metrics` holds the
// router's total, time to response and input count and the engine's prompt
// tokens, each labelled, and nothing about generated text; its completion
// closes the lease and frees the slot.
#[tokio::test]
async fn an_embedding_is_admitted_forwarded_streamed_and_settled() {
    let (endpoint, seen) = engine(Reply::Answer(200, "application/json", answer())).await;
    let (authority, deps, router) = stack(DeploymentApi::Embeddings, &endpoint);
    let sent = embed_body();
    let (status, headers, bytes) =
        send_raw(&router, "/v1/embeddings", sent.to_string().into_bytes()).await;
    let body: Value = serde_json::from_slice(&bytes).expect("a whole JSON document");
    assert_eq!(status, 200, "{body}");
    assert!(headers.get("content-length").is_none(), "{headers:?}");
    assert_eq!(authority.activations.load(Ordering::SeqCst), 1);
    let mut expected = sent.clone();
    expected["model"] = json!(SERVED);
    assert_eq!(
        seen.requests.lock().unwrap().as_slice(),
        [("/v1/embeddings".to_owned(), expected)]
    );
    let engine: Value = serde_json::from_str(&answer()).unwrap();
    assert_eq!(body["model"], ROUTE);
    for key in ["id", "object", "created", "data", "usage"] {
        assert_eq!(body[key], engine[key], "{key}");
    }
    let metrics = &body["capyctl"]["metrics"];
    assert_eq!(
        metrics["input_tokens"],
        json!({"value": 9, "source": "engine"})
    );
    assert_eq!(metrics["inputs"], json!({"value": 2, "source": "router"}));
    for figure in ["total_ms", "time_to_response_ms"] {
        assert_eq!(metrics[figure]["source"], "router", "{body}");
        assert!(metrics[figure]["value"].as_f64().unwrap() >= 0.0, "{body}");
    }
    for figure in [
        "ttft_ms",
        "decode_tokens_per_second",
        "queue_ms",
        "cached_tokens",
    ] {
        assert!(metrics.get(figure).is_none(), "{body}");
    }
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed)]);
    until("the slot is released", || {
        deps.inflight.current(DEPLOYMENT) == 0
    })
    .await;
    // SPEC §17, ADR 0036 §9: recorded in the latency series as a decision
    // is, but with no time to first content or to the last chunk.
    let mut series: Vec<_> = deps
        .inflight
        .latency
        .snapshot(Some(DEPLOYMENT))
        .into_iter()
        .map(|view| view.series)
        .collect();
    series.sort_unstable();
    assert_eq!(
        series,
        [
            "router_activation_wait",
            "router_lease_grant",
            "router_pre_forward",
            "router_queue_wait",
            "router_selection",
            "router_time_to_first_byte",
            "router_total",
            "router_upstream_first_byte",
        ]
    );

    let (status, _) = embed(&router, &sent).await;
    assert_eq!(status, 200);
    assert_eq!(authority.activations.load(Ordering::SeqCst), 1);
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed); 2]);
}

// T47 (ADR 0036 §3, SPEC §10): a request outside the embeddings contract is a
// 400 decided before any activation, lease or engine.
#[tokio::test]
async fn embedding_requests_outside_the_contract_are_refused_before_activation() {
    let (endpoint, seen) = engine(Reply::Answer(200, "application/json", answer())).await;
    let (authority, _deps, router) = stack(DeploymentApi::Embeddings, &endpoint);
    let with = |field: &str, value: Value| {
        let mut body = embed_body();
        body[field] = value;
        body.to_string().into_bytes()
    };
    let many: Vec<String> = (0..2049).map(|i| format!("text {i}")).collect();
    for body in [
        b"{not json".to_vec(),
        with("stream", json!(true)),
        with("cache_salt", json!("tenant")),
        with("messages", json!([])),
        with("truncate_prompt_tokens", json!(8)),
        with("encoding_format", json!("bytes")),
        with("input", json!("")),
        with("input", json!([])),
        with("input", json!(["a", ""])),
        with("input", json!(["a", [1, 2]])),
        with("input", json!([-1])),
        with("input", json!([2147483648u64])),
        with("input", json!(many)),
        with("input", json!([{"type": "image_url"}])),
        with("dimensions", json!(0)),
    ] {
        let (status, answer) = send(&router, "/v1/embeddings", body.clone()).await;
        assert_eq!(status, 400, "{}: {answer}", String::from_utf8_lossy(&body));
        assert_eq!(answer["code"], "invalid_request", "{answer}");
    }
    assert_eq!(authority.activations.load(Ordering::SeqCst), 0);
    assert!(authority.ends().is_empty());
    assert!(seen.requests.lock().unwrap().is_empty());
}

// T47 T43 (ADR 0036 §3): an embeddings deployment refuses chat completions
// and decision requests, and chat and decisions deployments refuse
// embeddings, each with `api_mismatch` naming the endpoint to use, before
// anything activates, is leased or is sent.
#[tokio::test]
async fn the_mismatched_endpoints_are_refused_before_activation_every_way() {
    let (endpoint, seen) = engine(Reply::Answer(200, "application/json", answer())).await;
    let (embeddings, _deps, router) = stack(DeploymentApi::Embeddings, &endpoint);
    let chat = |stream: bool| json!({"model": ROUTE, "stream": stream, "messages": [{"role": "user", "content": "hi"}]});
    let decision = json!({"model": ROUTE, "state": "s",
        "questions": {"q": {"type": "noul", "instructions": "x?"}}});
    for (path, body) in [
        ("/v1/chat/completions", chat(false)),
        ("/v1/chat/completions", chat(true)),
        ("/v1/systemone", decision),
    ] {
        let (status, answer) = send(&router, path, body.to_string().into_bytes()).await;
        assert_eq!(status, 400, "{path}: {answer}");
        assert_eq!(answer["code"], "api_mismatch");
        let message = answer["message"].as_str().unwrap();
        assert!(message.contains("POST /v1/embeddings"), "{answer}");
        assert!(message.contains("embeddings API"), "{answer}");
    }
    assert_eq!(embeddings.activations.load(Ordering::SeqCst), 0);
    assert!(embeddings.ends().is_empty());

    for (api, path) in [
        (DeploymentApi::Chat, "/v1/chat/completions"),
        (DeploymentApi::Decisions, "/v1/systemone"),
    ] {
        let (other, _deps, router) = stack(api, &endpoint);
        let (status, answer) = embed(&router, &embed_body()).await;
        assert_eq!(status, 400, "{answer}");
        assert_eq!(answer["code"], "api_mismatch");
        assert!(
            answer["message"]
                .as_str()
                .unwrap()
                .contains(&format!("POST {path}")),
            "{answer}"
        );
        assert_eq!(other.activations.load(Ordering::SeqCst), 0);
        assert!(other.ends().is_empty());
    }
    assert!(seen.requests.lock().unwrap().is_empty());
}

// T47 (ADR 0036 §3, owner decision 2026-10-27, question 4): on llama.cpp a
// `dimensions` other than the instance's recorded native dimension is
// refused with `invalid_request` saying llama.cpp returns full vectors, and
// nothing reaches the engine; the native one is forwarded. vLLM judges
// `dimensions` itself, and an instance with no recorded dimension yet is not
// refused.
#[tokio::test]
async fn llamacpp_refuses_a_dimension_other_than_its_native_one() {
    let (endpoint, seen) = engine(Reply::Answer(200, "application/json", answer())).await;
    let llamacpp = Arc::new(Authority {
        dimension: Some(3),
        ..Arc::into_inner(authority(DeploymentApi::Embeddings, "llamacpp", &endpoint)).unwrap()
    });
    let (authority_, deps, router) = stack_on(llamacpp);
    let mut body = embed_body();
    body["dimensions"] = json!(4);
    let (status, refused) = embed(&router, &body).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["code"], "invalid_request");
    let message = refused["message"].as_str().unwrap();
    assert!(message.contains("dimensions must be 3"), "{refused}");
    assert!(
        message.contains("llama.cpp returns full vectors"),
        "{refused}"
    );
    assert!(seen.requests.lock().unwrap().is_empty());
    assert_eq!(authority_.ends(), vec![Some(LeaseEnd::NotAccepted)]);
    assert_eq!(deps.inflight.current(DEPLOYMENT), 0);
    body["dimensions"] = json!(3);
    let (status, answered) = embed(&router, &body).await;
    assert_eq!(status, 200, "{answered}");
    assert_eq!(seen.requests.lock().unwrap().len(), 1);

    // Not llama.cpp, or no dimension recorded yet: forwarded as asked (the
    // engine answers 3 values per vector, so `dimensions: 4` fails its
    // checks; what matters here is that it was sent).
    for (engine_name, dimension) in [("vllm", Some(3)), ("llamacpp", None)] {
        let (endpoint, seen) = engine(Reply::Answer(200, "application/json", answer())).await;
        let other = Arc::new(Authority {
            dimension,
            ..Arc::into_inner(authority(DeploymentApi::Embeddings, engine_name, &endpoint)).unwrap()
        });
        let (_other, _deps, router) = stack_on(other);
        body["dimensions"] = json!(4);
        let _ = send_raw(&router, "/v1/embeddings", body.to_string().into_bytes()).await;
        assert_eq!(
            seen.requests.lock().unwrap().len(),
            1,
            "{engine_name} {dimension:?}: forwarded"
        );
    }
}

// T47 (ADR 0036 §4): the answer reaches the client as it arrives: with the
// engine paused mid-answer, the client already holds HTTP 200 and the first
// entry, with `model` not yet written; the rest, the figures and the brace
// follow once the engine finishes.
#[tokio::test]
async fn the_answer_reaches_the_client_before_the_engine_finished() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let (head, rest) = halves();
    let (endpoint, seen) = engine(Reply::Paused {
        head,
        rest: Some(rest),
        gate: gate.clone(),
    })
    .await;
    let (authority, _deps, router) = stack(DeploymentApi::Embeddings, &endpoint);
    let address = serve(router).await;
    let mut response = reqwest::Client::new()
        .post(format!("http://{address}/v1/embeddings"))
        .json(&embed_body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let first = tokio::time::timeout(Duration::from_secs(5), response.chunk())
        .await
        .expect("the first entry arrives while the engine is paused")
        .unwrap()
        .unwrap();
    let first = String::from_utf8(first.to_vec()).unwrap();
    assert!(first.contains(r#""index":0"#), "{first}");
    assert_eq!(seen.finished.load(Ordering::SeqCst), 0);
    assert!(authority.ends().iter().all(Option::is_none));
    gate.notify_one();
    let mut whole = first;
    while let Some(chunk) = response.chunk().await.unwrap() {
        whole.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    let body: Value = serde_json::from_str(&whole).expect("a whole document");
    assert_eq!(body["model"], ROUTE);
    assert_eq!(body["data"][1]["index"], 1);
    assert_eq!(body["capyctl"]["metrics"]["inputs"]["value"], 2);
    until("the lease closes completed", || {
        authority.ends() == vec![Some(LeaseEnd::Completed)]
    })
    .await;
}

// T47 T17 (ADR 0036 §4): an answer that fails a check after part of it was
// relayed (a repeated index, ragged vectors, invalid JSON, an answer cut
// short, the engine closing the connection) ends without `capyctl.metrics` or
// the closing brace, so no JSON parser accepts it; the request is uncertain
// and never replayed.
#[tokio::test]
async fn a_failed_answer_ends_without_the_brace_and_leaves_the_lease_uncertain() {
    let (head, rest) = halves();
    for rest in [
        Some(rest.replace(r#"{"index":1"#, r#"{"index":0"#)),
        Some(rest.replace("[1e-2,3,0]", "[1e-2,3]")),
        Some(rest.replace("[1e-2,3,0]", "[1e-2,3,]")),
        Some(rest[..rest.len() - 10].to_owned()),
        None,
    ] {
        // The first entry reaches the client before the rest is sent.
        let gate = Arc::new(tokio::sync::Notify::new());
        let opener = gate.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            opener.notify_one();
        });
        let reply = Reply::Paused {
            head: head.clone(),
            rest: rest.clone(),
            gate,
        };
        let (endpoint, seen) = engine(reply).await;
        let (authority, deps, router) = stack(DeploymentApi::Embeddings, &endpoint);
        let (status, _, bytes) = send_raw(
            &router,
            "/v1/embeddings",
            embed_body().to_string().into_bytes(),
        )
        .await;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert_eq!(status, 200, "{text}");
        assert!(text.starts_with('{'), "{text}");
        assert!(serde_json::from_slice::<Value>(&bytes).is_err(), "{text}");
        assert!(!text.contains("capyctl"), "{text}");
        until("the lease is retained uncertain", || {
            authority.ends() == vec![Some(LeaseEnd::Uncertain)]
        })
        .await;
        assert_eq!(seen.requests.lock().unwrap().len(), 1, "never replayed");
        until("the in-memory slot is released", || {
            deps.inflight.current(DEPLOYMENT) == 0
        })
        .await;
        assert!(authority.settled().is_empty());
    }
}

// T47 (ADR 0036 §4, SPEC §10): an engine's complete invalid-request answer
// (an input over the model's length) is relayed with its status as
// `engine_rejected` and closes the lease completed; an answer that never
// started is a 500 `engine_error` before any byte.
#[tokio::test]
async fn an_engine_rejection_is_relayed_as_engine_rejected() {
    let (endpoint, _) = engine(Reply::Answer(
        400,
        "application/json",
        r#"{"object":"error","message":"This model's maximum context length is 512 tokens","type":"BadRequestError","code":400}"#.into(),
    ))
    .await;
    let (authority, deps, router) = stack(DeploymentApi::Embeddings, &endpoint);
    let (status, refused) = embed(&router, &embed_body()).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["code"], "engine_rejected");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("maximum context length"),
        "{refused}"
    );
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed)]);
    until("the slot is released", || {
        deps.inflight.current(DEPLOYMENT) == 0
    })
    .await;

    let (endpoint, _) = engine(Reply::Answer(200, "text/plain", answer())).await;
    let (authority, _deps, router) = stack(DeploymentApi::Embeddings, &endpoint);
    let (status, answer) = embed(&router, &embed_body()).await;
    assert_eq!(status, 500, "{answer}");
    assert_eq!(answer["code"], "engine_error");
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Uncertain)]);
}

// T47 T17 (ADR 0036 §4, owner decision 2026-10-27, question 7): a client
// that hangs up mid-answer does not close the engine connection: the lease
// is cancelling at once, the answer is read and checked to its end, and the
// lease settles on it only after the engine finished, never before.
#[tokio::test]
async fn a_hang_up_mid_answer_settles_on_the_answer_after_the_engine_finished() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let (head, rest) = halves();
    let (endpoint, seen) = engine(Reply::Paused {
        head,
        rest: Some(rest),
        gate: gate.clone(),
    })
    .await;
    let (authority, deps, router) = stack(DeploymentApi::Embeddings, &endpoint);
    let address = serve(router).await;
    let mut response = reqwest::Client::new()
        .post(format!("http://{address}/v1/embeddings"))
        .json(&embed_body())
        .send()
        .await
        .unwrap();
    let first = response.chunk().await.unwrap().unwrap();
    assert!(!first.is_empty());
    drop(response);
    until("the lease is cancelling on the hang-up", || {
        authority.ends() == vec![Some(LeaseEnd::Cancelling { answered: true })]
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        authority.settled().is_empty(),
        "not before the engine finished"
    );
    assert_eq!(
        seen.dropped.load(Ordering::SeqCst),
        0,
        "the engine connection stays open"
    );
    until("the in-memory slot is released", || {
        deps.inflight.current(DEPLOYMENT) == 0
    })
    .await;
    gate.notify_one();
    until("the lease settles on the answer", || {
        authority.settled() == vec!["0".to_owned()]
    })
    .await;
    assert_eq!(seen.finished.load(Ordering::SeqCst), 1);
    assert_eq!(seen.dropped.load(Ordering::SeqCst), 0);
}

// T47 T17 T19 (ADR 0036 §4): a request cut by its deadline before the engine
// sent anything is answered 500 `engine_error` at the deadline, with its
// lease cancelling and unanswered; the engine connection stays open, and the
// lease settles on the answer once the engine finishes it.
#[tokio::test]
async fn a_deadline_cut_is_read_to_its_end_and_settles_on_the_answer() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let (endpoint, seen) = engine(Reply::Paused {
        head: String::new(),
        rest: Some(answer()),
        gate: gate.clone(),
    })
    .await;
    let (authority, deps, router) = stack(DeploymentApi::Embeddings, &endpoint);
    authority.ready.store(true, Ordering::SeqCst);
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            deadline: Duration::from_millis(300),
            stream_idle: Duration::from_secs(600),
            ..Default::default()
        });
    let (status, answer) =
        tokio::time::timeout(Duration::from_secs(10), embed(&router, &embed_body()))
            .await
            .expect("cut at the request deadline");
    assert_eq!(status, 500, "{answer}");
    assert_eq!(answer["code"], "engine_error");
    assert_eq!(
        authority.ends(),
        vec![Some(LeaseEnd::Cancelling { answered: false })]
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        authority.settled().is_empty(),
        "not before the engine finished"
    );
    assert_eq!(
        seen.dropped.load(Ordering::SeqCst),
        0,
        "the engine connection stays open"
    );
    gate.notify_one();
    until("the lease settles on the answer", || {
        authority.settled() == vec!["0".to_owned()]
    })
    .await;
    assert_eq!(seen.finished.load(Ordering::SeqCst), 1);
}

// T47 T17 (ADR 0036 §4): a cut request whose answer then breaks never
// completes, so it keeps today's rule: its lease stays cancelling, held to
// the instance's verified stop, and is never settled on an answer.
#[tokio::test]
async fn a_cut_answer_that_breaks_is_held_to_the_verified_stop() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let (head, _) = halves();
    let (endpoint, seen) = engine(Reply::Paused {
        head,
        rest: None,
        gate: gate.clone(),
    })
    .await;
    let (authority, deps, router) = stack(DeploymentApi::Embeddings, &endpoint);
    let address = serve(router).await;
    let mut response = reqwest::Client::new()
        .post(format!("http://{address}/v1/embeddings"))
        .json(&embed_body())
        .send()
        .await
        .unwrap();
    response.chunk().await.unwrap().unwrap();
    drop(response);
    until("the lease is cancelling on the hang-up", || {
        authority.ends() == vec![Some(LeaseEnd::Cancelling { answered: true })]
    })
    .await;
    gate.notify_one();
    until("the engine's answer broke", || {
        seen.dropped.load(Ordering::SeqCst) == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        authority.settled().is_empty(),
        "never settled on a broken answer"
    );
    assert_eq!(
        authority.ends(),
        vec![Some(LeaseEnd::Cancelling { answered: true })]
    );
    assert_eq!(deps.inflight.current(DEPLOYMENT), 0);
}

// T47 (ADR 0036 §3): `/v1/models` names an embeddings deployment's API
// without waking it.
#[tokio::test]
async fn models_name_the_embeddings_api_without_waking() {
    let (authority, _deps, router) = stack(DeploymentApi::Embeddings, "http://127.0.0.1:9");
    let (status, listed) = send(&router, "/v1/models", Vec::new()).await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(listed["data"][0]["id"], ROUTE);
    assert_eq!(
        listed["data"][0]["capyctl"]["api"], "embeddings",
        "{listed}"
    );
    assert_eq!(authority.activations.load(Ordering::SeqCst), 0);
}

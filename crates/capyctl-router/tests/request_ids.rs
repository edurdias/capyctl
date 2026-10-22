//! SPEC §10 (owner decision 2026-10-19): request IDs. A caller's
//! `X-Request-Id` has one fixed format, is echoed on every answer (a stream's
//! included) and never reaches the engine; every answer carries
//! `X-CapyCTL-Request-Id`, the request's first lease ID, or a fresh ULID when
//! no lease was granted. Through the real engine forwarder to an engine
//! double that records what reached it. CPU tests, not qualification.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use capyctl_config::effective::DeploymentApi;
use capyctl_controller::{LeaseEnd, LeaseRefused, LifecycleFault, RequestLease};
use capyctl_router::request_id::{CALLER_HEADER, CAPYCTL_HEADER};
use capyctl_router::{QueueLimits, RouterDeps};
use serde_json::{json, Value};
use tower::ServiceExt;

const DEPLOYMENT: &str = "dep-ids";
const ROUTE: &str = "ids";
const SERVED: &str = "served-ids";

/// One READY deployment serving `api`, whose engine runs at `endpoint`. It
/// names its leases `lease-<n>` and records how each ended; with `refuse`
/// set it grants none.
struct Authority {
    api: DeploymentApi,
    endpoint: String,
    refuse: AtomicBool,
    leases: Mutex<Vec<Option<LeaseEnd>>>,
}

impl Authority {
    fn row(&self) -> capyctl_store::deployments::DeploymentRow {
        capyctl_store::deployments::DeploymentRow {
            id: DEPLOYMENT.into(),
            name: ROUTE.into(),
            kind: "llamacpp".into(),
            route_model_id: Some(ROUTE.into()),
            desired_state: capyctl_domain::LifecycleState::Ready,
            observed_state: capyctl_domain::LifecycleState::Ready,
            schema_version: 1,
            current_generation: 1,
        }
    }
    fn ends(&self) -> Vec<Option<LeaseEnd>> {
        self.leases.lock().unwrap().clone()
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
    ) -> Result<Option<capyctl_controller::RuntimeEndpoint>, LifecycleFault> {
        Ok(
            (deployment == DEPLOYMENT).then(|| capyctl_controller::RuntimeEndpoint {
                endpoint: self.endpoint.clone(),
                served_model: SERVED.into(),
                engine_key: Some("launch-key".into()),
                incarnation: "1".into(),
                engine: Some("llamacpp".into()),
                request_abort: false,
            }),
        )
    }
    async fn open_request_lease(
        &self,
        _d: &str,
        _max: usize,
    ) -> Result<Option<RequestLease>, LeaseRefused> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(LeaseRefused::Full);
        }
        let mut leases = self.leases.lock().unwrap();
        leases.push(None);
        Ok(Some(RequestLease::unrecorded(&format!(
            "lease-{}",
            leases.len() - 1
        ))))
    }
    async fn close_request_lease(
        &self,
        lease: RequestLease,
        end: LeaseEnd,
    ) -> Result<(), LeaseRefused> {
        let index: usize = lease.id().strip_prefix("lease-").unwrap().parse().unwrap();
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

/// What reached the engine: the headers of every request.
type Seen = Arc<Mutex<Vec<HeaderMap>>>;

fn chunk(delta: Value, finish: Value) -> String {
    let chunk = json!({"id": "c1", "object": "chat.completion.chunk", "created": 1,
        "model": SERVED, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
    format!("data: {chunk}\n\n")
}

/// An engine double that records the headers of every request, answers chat
/// with a short stream, and answers a decision with `decision` (a status and
/// a JSON body). The engine sends its own `X-Request-Id`, which must never
/// reach the client.
async fn engine(decision: (u16, Value)) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let recorded = seen.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, headers: HeaderMap| {
        let recorded = recorded.clone();
        let decision = decision.clone();
        async move {
            recorded.lock().unwrap().push(headers);
            let (status, content_type, body) = if uri.path() == "/v1/systemone" {
                (decision.0, "application/json", decision.1.to_string())
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
                .header(CALLER_HEADER, "engine-own-id")
                .header(CAPYCTL_HEADER, "engine-own-id")
                .body(axum::body::Body::from(body))
                .unwrap()
        }
    });
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (base, seen)
}

fn answer() -> Value {
    json!({
        "model": SERVED,
        "answers": {"angry": {"type": "noul", "noul": 0.6226}},
        "usage": {"input_tokens": 87, "output_tokens": 0},
    })
}

fn decision_body() -> Value {
    json!({"model": ROUTE, "state": "hi",
           "questions": {"angry": {"type": "noul", "instructions": "Is the customer angry?"}}})
}

fn chat_body(stream: bool) -> Value {
    json!({"model": ROUTE, "stream": stream, "messages": [{"role": "user", "content": "hi"}]})
}

async fn stack(api: DeploymentApi, decision: (u16, Value)) -> (Arc<Authority>, Seen, axum::Router) {
    let (endpoint, seen) = engine(decision).await;
    let authority = Arc::new(Authority {
        api,
        endpoint,
        refuse: AtomicBool::new(false),
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
        api_key: None,
        inflight: Arc::new(capyctl_router::admission::InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    });
    (authority, seen, router)
}

/// Send `body` to `path` with `caller` as `X-Request-Id` (raw bytes); the
/// answer's status, headers and body text.
async fn send(
    router: &axum::Router,
    path: &str,
    body: &Value,
    caller: Option<&[u8]>,
) -> (StatusCode, HeaderMap, String) {
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(caller) = caller {
        request = request.header(
            CALLER_HEADER,
            axum::http::HeaderValue::from_bytes(caller).unwrap(),
        );
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

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).map(|v| v.to_str().unwrap())
}

/// T21: nothing CapyCTL consumes reaches the engine.
fn assert_engine_saw_no_ids(seen: &Seen) {
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    for headers in seen.iter() {
        assert!(!headers.contains_key(CALLER_HEADER), "{headers:?}");
        assert!(!headers.contains_key(CAPYCTL_HEADER), "{headers:?}");
    }
}

// T17 T21 (SPEC §10, owner decision 2026-10-19): a collected chat answer, a
// stream and a decision each echo the caller's ID in their response headers
// and carry the request's lease ID as CapyCTL's; the engine sees neither,
// and the engine's own headers never reach the client.
#[tokio::test]
async fn answers_echo_the_caller_id_and_name_the_lease_and_the_engine_sees_neither() {
    let (authority, seen, router) = stack(DeploymentApi::Chat, (200, answer())).await;
    for (n, stream) in [false, true].into_iter().enumerate() {
        let (status, headers, text) = send(
            &router,
            "/v1/chat/completions",
            &chat_body(stream),
            Some(b"caller-42"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(header(&headers, CALLER_HEADER), Some("caller-42"));
        assert_eq!(
            header(&headers, CAPYCTL_HEADER),
            Some(format!("lease-{n}").as_str())
        );
        if stream {
            assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
        } else {
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap()["choices"][0]["message"]["content"],
                "hi"
            );
        }
    }
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed); 2]);
    assert_engine_saw_no_ids(&seen);

    // T43: a decision the same way; without a caller ID only CapyCTL's.
    let (authority, seen, router) = stack(DeploymentApi::Decisions, (200, answer())).await;
    let (status, headers, text) =
        send(&router, "/v1/systemone", &decision_body(), Some(b"d-1")).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(header(&headers, CALLER_HEADER), Some("d-1"));
    assert_eq!(header(&headers, CAPYCTL_HEADER), Some("lease-0"));
    let (status, headers, _) = send(&router, "/v1/systemone", &decision_body(), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&headers, CALLER_HEADER), None);
    assert_eq!(header(&headers, CAPYCTL_HEADER), Some("lease-1"));
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed); 2]);
    assert_engine_saw_no_ids(&seen);
}

// T17 T43 (SPEC §10): an error answered after a lease was granted (the
// engine's rejection) carries both IDs, the lease's as CapyCTL's.
#[tokio::test]
async fn an_error_after_a_lease_names_the_lease() {
    let rejection = json!({"error": {"code": 400, "message": "instructions must be provided",
                                     "type": "invalid_request_error"}});
    let (authority, seen, router) = stack(DeploymentApi::Decisions, (400, rejection)).await;
    let (status, headers, text) =
        send(&router, "/v1/systemone", &decision_body(), Some(b"r-1")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(text.contains("engine_rejected"), "{text}");
    assert_eq!(header(&headers, CALLER_HEADER), Some("r-1"));
    assert_eq!(header(&headers, CAPYCTL_HEADER), Some("lease-0"));
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed)]);
    assert_engine_saw_no_ids(&seen);
}

// T19 (SPEC §10): a request refused before any lease (an unknown model, an
// endpoint the deployment does not answer, a refused lease) still carries a
// CapyCTL ID, a fresh ULID per request, and the caller's.
#[tokio::test]
async fn a_refusal_before_any_lease_gets_a_fresh_id() {
    let (authority, seen, router) = stack(DeploymentApi::Chat, (200, answer())).await;
    let mut unknown = chat_body(false);
    unknown["model"] = json!("nobody");
    let mut ids = Vec::new();
    for (path, body, status) in [
        ("/v1/chat/completions", unknown, StatusCode::NOT_FOUND),
        ("/v1/systemone", decision_body(), StatusCode::BAD_REQUEST),
    ] {
        let (answered, headers, text) = send(&router, path, &body, Some(b"x")).await;
        assert_eq!(answered, status, "{text}");
        assert_eq!(header(&headers, CALLER_HEADER), Some("x"));
        let id = header(&headers, CAPYCTL_HEADER).unwrap().to_owned();
        assert!(ulid::Ulid::from_string(&id).is_ok(), "{id}");
        ids.push(id);
    }
    authority.refuse.store(true, Ordering::SeqCst);
    let (answered, headers, text) = send(
        &router,
        "/v1/chat/completions",
        &chat_body(true),
        Some(b"x"),
    )
    .await;
    assert_eq!(answered, StatusCode::TOO_MANY_REQUESTS, "{text}");
    assert!(headers.contains_key("retry-after"));
    let id = header(&headers, CAPYCTL_HEADER).unwrap().to_owned();
    assert!(ulid::Ulid::from_string(&id).is_ok(), "{id}");
    ids.push(id);
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "one fresh ID per request");
    assert!(authority.ends().is_empty());
    assert!(seen.lock().unwrap().is_empty());
}

// T19 (SPEC §10): a malformed `X-Request-Id` is a 400 `invalid_request`
// naming the header, decided before anything is leased or sent; it is not
// echoed, and the answer still carries a CapyCTL ID.
#[tokio::test]
async fn a_malformed_caller_id_is_refused_before_anything_is_touched() {
    let (authority, seen, router) = stack(DeploymentApi::Chat, (200, answer())).await;
    for bad in [&b""[..], b"two words", &[b'a'; 129], b"tab\there"] {
        for (path, body) in [
            ("/v1/chat/completions", chat_body(false)),
            ("/v1/chat/completions", chat_body(true)),
            ("/v1/systemone", decision_body()),
        ] {
            let (status, headers, text) = send(&router, path, &body, Some(bad)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
            let answer: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(answer["code"], "invalid_request", "{answer}");
            assert!(
                answer["message"].as_str().unwrap().contains("X-Request-Id"),
                "{answer}"
            );
            assert_eq!(header(&headers, CALLER_HEADER), None);
            let id = header(&headers, CAPYCTL_HEADER).unwrap();
            assert!(ulid::Ulid::from_string(id).is_ok(), "{id}");
        }
    }
    let (status, _, _) = send(
        &router,
        "/v1/chat/completions",
        &chat_body(false),
        Some(&[b'~'; 128]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "128 printable bytes are accepted");
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed)]);
    assert_engine_saw_no_ids(&seen);
}

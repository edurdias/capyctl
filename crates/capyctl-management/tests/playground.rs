//! ADR 0040 §12: composed browser authorization forwards into the real inference
//! router. Engine doubles supply engine bytes, never router envelopes.
use axum::{
    body::{to_bytes, Body, Bytes},
    extract::connect_info::MockConnectInfo,
    http::{HeaderMap, Method, Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    Router,
};
use capyctl_config::effective::DeploymentApi;
use capyctl_controller::{Controller, LeaseEnd, LeaseRefused, LifecycleFault, RequestLease};
use capyctl_domain::{LifecycleAction, LifecycleState};
use capyctl_management::{
    compose::{self, ManagementPeer},
    playground,
    ui_sessions::{Principal, SessionVerifier},
    ManagementCredentials,
};
use capyctl_router::{forwarders::StaticForwarders, QueueLimits, RouterDeps};
use capyctl_store::Store;
use capyctl_testkit::{FakeEngine, HeldEngine};
use futures::StreamExt;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;

const ADMIN: &str = "management-credential-012345678901234567890";
const KEY: &str = "inference-credential-0123456789012345678901";
const HOST: &str = "box.lan:7443";
const CHAT: &str = "/management/v1/playground/chat/completions";
const DECISION: &str = "/management/v1/playground/systemone";
const PROMPT: &str = "PLAYGROUND_PRIVATE_PROMPT_7d3bd682";
const ANSWER: &str = "PLAYGROUND_PRIVATE_ANSWER_c6a17f95";

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

struct Management {
    router: Router,
    _dir: tempfile::TempDir,
}

fn management(inference: Router) -> Management {
    let dir = private_dir();
    let verifier = SessionVerifier::open(
        ManagementCredentials::from_trusted_resolver(ADMIN, KEY).unwrap(),
        Store::open(&dir.path().join("sessions.sqlite3")).unwrap(),
    )
    .unwrap();
    Management {
        router: compose::compose(verifier, None, [playground::router(inference)]),
        _dir: dir,
    }
}

fn from(router: &Router, peer: &str) -> Router {
    router.clone().layer(MockConnectInfo(ManagementPeer {
        peer: peer.parse().unwrap(),
        local: Some("192.0.2.1:7443".parse().unwrap()),
    }))
}

struct Session {
    id: String,
    cookie: String,
}

async fn sign_in(router: &Router) -> Session {
    let response = from(router, "127.0.0.1:50000")
        .oneshot(
            Request::post("/management/v1/ui/sign-in-links")
                .header("authorization", format!("Bearer {ADMIN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let link: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    let response = from(router, "198.51.100.7:50000")
        .oneshot(
            Request::post("/management/v1/ui/sessions")
                .header("host", HOST)
                .header("origin", format!("http://{HOST}"))
                .header("x-capyctl-ui", "1")
                .header("content-type", "application/json")
                .body(Body::from(json!({"token": link["token"]}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let value: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    Session {
        id: value["session"]["id"].as_str().unwrap().into(),
        cookie,
    }
}

fn browser(path: &str, session: &Session, body: impl Into<Body>) -> Request<Body> {
    Request::post(path)
        .header("host", HOST)
        .header("origin", format!("http://{HOST}"))
        .header("x-capyctl-ui", "1")
        .header("cookie", &session.cookie)
        .header("content-type", "application/json")
        .header("x-request-id", "playground-request")
        .body(body.into())
        .unwrap()
}

fn chat(stream: bool) -> Value {
    json!({"model": "chat", "stream": stream, "messages": [{"role": "user", "content": PROMPT}]})
}

fn decision() -> Value {
    json!({"model": "chat", "state": PROMPT, "questions": {
        "route": {"type": "choice", "instructions": "Choose a team", "criteria": {"billing": "Payments", "shipping": "Delivery"}}
    }})
}

fn deps(
    controller: Arc<dyn capyctl_controller::LifecyclePort>,
    forward: Arc<dyn capyctl_adapters::ChatForward>,
) -> RouterDeps {
    RouterDeps {
        controller,
        // One engine double for this fixture's sole deployment. StaticForwarders
        // addresses multiple entries by deployment id, not engine family.
        forwards: Arc::new(StaticForwarders(HashMap::from([("only".into(), forward)]))),
        limits: QueueLimits {
            max_requests_per_deployment: 4,
            max_buffered_bytes_total: 4 << 20,
        },
        api_key: None,
        inflight: Arc::new(capyctl_router::admission::InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    }
}

async fn local() -> (tempfile::TempDir, Arc<Controller>, String, RouterDeps) {
    let dir = private_dir();
    let store = Arc::new(Mutex::new(
        Store::open(&dir.path().join("state.sqlite3")).unwrap(),
    ));
    let fake = Arc::new(FakeEngine::new());
    let controller = Arc::new(Controller::new(
        store,
        fake.clone(),
        Arc::new(capyctl_testkit::FakeLauncher::new()),
    ));
    let id = controller
        .submit_deploy(capyctl_controller::DeployRequest {
            name: "chat".into(),
            kind: "fake".into(),
            manifest: b"name: chat\nkind: fake\n".to_vec(),
            route_model_id: Some("chat".into()),
        })
        .await
        .unwrap();
    let op = controller
        .request_transition(&id, LifecycleAction::Start)
        .await
        .unwrap();
    assert_eq!(
        controller.wait_terminal(&op).await.unwrap(),
        LifecycleState::Ready
    );
    let deps = deps(controller.clone(), fake);
    (dir, controller, id, deps)
}

async fn api(router: &Router, path: &str, body: String) -> Response {
    router
        .clone()
        .oneshot(
            Request::post(path)
                .header("authorization", format!("Bearer {KEY}"))
                .header("content-type", "application/json")
                .header("x-request-id", "playground-request")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn bytes(response: Response) -> (StatusCode, Bytes) {
    (
        response.status(),
        to_bytes(response.into_body(), 16 << 20).await.unwrap(),
    )
}

async fn chunks(response: Response) -> Vec<Bytes> {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    response
        .into_body()
        .into_data_stream()
        .map(|chunk| chunk.unwrap())
        .collect()
        .await
}

#[derive(Clone)]
struct BrowserExtension;
type Captured = Arc<Mutex<Vec<Bytes>>>;

// ADR 0040 §12: inspect the supplied service boundary, before inference can
// strip headers itself. Capture its actual output to prove lossless streaming,
// including the per-request metrics comment (whose timings vary between calls).
fn observed(router: Router, captured: Captured) -> Router {
    router.layer(middleware::from_fn(
        move |request: Request<Body>, next: Next| {
            let captured = captured.clone();
            async move {
                assert_eq!(request.method(), Method::POST);
                assert!(matches!(
                    request.uri().path(),
                    "/v1/chat/completions" | "/v1/systemone"
                ));
                assert!(request.uri().query().is_none());
                let mut names: Vec<_> =
                    request.headers().keys().map(|name| name.as_str()).collect();
                names.sort_unstable();
                assert_eq!(names, ["content-type", "x-request-id"]);
                assert_eq!(request.headers()["content-type"], "application/json");
                assert_eq!(request.headers()["x-request-id"], "playground-request");
                assert!(request.extensions().get::<BrowserExtension>().is_none());
                assert!(request.extensions().get::<Principal>().is_none());
                assert!(request
                    .extensions()
                    .get::<MockConnectInfo<ManagementPeer>>()
                    .is_none());
                assert!(request
                    .extensions()
                    .get::<axum::extract::ConnectInfo<ManagementPeer>>()
                    .is_none());
                let response = next.run(request).await;
                response.map(|body| {
                    Body::from_stream(body.into_data_stream().inspect(move |chunk| {
                        if let Ok(chunk) = chunk {
                            captured.lock().unwrap().push(chunk.clone());
                        }
                    }))
                })
            }
        },
    ))
}

// ADR 0040 §12: both endpoints are protected by composed authorization; the
// inference listener still needs its own API key, never the browser cookie.
#[tokio::test]
async fn both_playground_routes_require_composed_authorization() {
    let (_dir, _controller, _id, mut deps) = local().await;
    let management = management(capyctl_router::serve_router(deps.clone()));
    for path in [CHAT, DECISION] {
        let response = from(&management.router, "198.51.100.7:50000")
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(chat(true).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let session = sign_in(&management.router).await;
    deps.api_key = Some(KEY.into());
    let response = capyctl_router::serve_router(deps)
        .oneshot(browser(
            "/v1/chat/completions",
            &session,
            chat(true).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ADR 0040 §12: no header or extension from the browser crosses the supplied
// service boundary. Actual data frames equal keyed inference, and every byte
// of this request's router response, metrics included, reaches the browser.
#[tokio::test]
async fn session_chat_streams_actual_inference_chunks_with_only_allowed_headers() {
    let (_dir, _controller, _id, mut deps) = local().await;
    let captured: Captured = Arc::default();
    let management = management(observed(
        capyctl_router::serve_router(deps.clone()),
        captured.clone(),
    ));
    let session = sign_in(&management.router).await;
    let mut request = browser(
        &format!("{CHAT}?browser=query"),
        &session,
        chat(true).to_string(),
    );
    // Session requests omit Authorization; composition gives it precedence.
    request
        .headers_mut()
        .insert("accept", "text/event-stream".parse().unwrap());
    request
        .headers_mut()
        .insert("x-extra-browser-header", "private".parse().unwrap());
    request.extensions_mut().insert(BrowserExtension);
    let forwarded = chunks(
        from(&management.router, "198.51.100.7:50000")
            .oneshot(request)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(forwarded, *captured.lock().unwrap());
    deps.api_key = Some(KEY.into());
    let direct = chunks(
        api(
            &capyctl_router::serve_router(deps),
            "/v1/chat/completions",
            chat(true).to_string(),
        )
        .await,
    )
    .await;
    let data = |chunks: &[Bytes]| {
        let joined: Vec<u8> = chunks
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect();
        String::from_utf8(joined)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("data:"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let frames = data(&forwarded);
    assert_eq!(frames, data(&direct));
    assert!(frames.iter().any(|frame| frame.contains("hel")));
    assert!(frames.iter().any(|frame| frame.contains("lo")));
    assert_eq!(frames.last().unwrap(), "data: [DONE]");
}

// ADR 0040 §12: a browser-shaped request authenticated by the local management
// bearer must not hand that bearer to inference or its HTTP engine.
#[tokio::test]
async fn browser_authorization_is_removed_before_inference_and_the_http_engine() {
    let engine = engine().await;
    let captured = Arc::default();
    let management = management(observed(
        capyctl_router::serve_router(live(DeploymentApi::Chat, &engine.endpoint)),
        captured,
    ));
    let session = sign_in(&management.router).await;
    let mut request = browser(CHAT, &session, chat(true).to_string());
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {ADMIN}").parse().unwrap());
    request.extensions_mut().insert(BrowserExtension);
    let response = from(&management.router, "127.0.0.1:50000")
        .oneshot(request)
        .await
        .unwrap();
    let (_, body) = bytes(response).await;
    assert!(String::from_utf8_lossy(&body).contains(ANSWER));
    let seen = engine.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "/v1/chat/completions");
    assert!(!seen[0].1.contains_key("cookie"));
    assert!(!seen[0].1.contains_key("x-capyctl-ui"));
    assert_eq!(seen[0].1["authorization"], "Bearer launch-key");
}

// ADR 0040 §12; AE7: this is the real controller's Park -> request-driven
// activation -> Ready path with the existing Fake engine, not a canned reply.
// AE7: the signed-in request wakes a genuinely parked deployment.
#[tokio::test]
async fn a_parked_deployment_wakes_and_answers() {
    let (_dir, controller, id, deps) = local().await;
    let op = controller
        .request_transition(&id, LifecycleAction::Park)
        .await
        .unwrap();
    assert_eq!(
        controller.wait_terminal(&op).await.unwrap(),
        LifecycleState::Parked
    );
    let management = management(capyctl_router::serve_router(deps));
    let session = sign_in(&management.router).await;
    let response = from(&management.router, "198.51.100.7:50000")
        .oneshot(browser(CHAT, &session, chat(false).to_string()))
        .await
        .unwrap();
    let (status, body) = bytes(response).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let answer: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(answer["choices"][0]["message"]["content"], "ok");
    use capyctl_controller::LifecyclePort as _;
    assert_eq!(
        controller
            .get_deployment(&id)
            .unwrap()
            .unwrap()
            .observed_state,
        LifecycleState::Ready
    );
}

// ADR 0040 §12: forwarding must not replace the inference router's envelope.
#[tokio::test]
async fn stopped_deployment_envelope_is_unchanged() {
    let (_dir, controller, id, mut deps) = local().await;
    let op = controller
        .request_transition(&id, LifecycleAction::Stop)
        .await
        .unwrap();
    controller.wait_terminal(&op).await.unwrap();
    let management = management(capyctl_router::serve_router(deps.clone()));
    let session = sign_in(&management.router).await;
    let forwarded = bytes(
        from(&management.router, "198.51.100.7:50000")
            .oneshot(browser(CHAT, &session, chat(false).to_string()))
            .await
            .unwrap(),
    )
    .await;
    deps.api_key = Some(KEY.into());
    let direct = bytes(
        api(
            &capyctl_router::serve_router(deps),
            "/v1/chat/completions",
            chat(false).to_string(),
        )
        .await,
    )
    .await;
    assert_eq!(forwarded, direct);
    assert_eq!(forwarded.0, StatusCode::CONFLICT);
    let answer: Value = serde_json::from_slice(&forwarded.1).unwrap();
    assert_eq!(answer["code"], "deployment_stopped");
    assert!(answer["message"]
        .as_str()
        .unwrap()
        .contains(&format!("capyctl start deployment {id}")));
}

// ADR 0040 §12: axum's default 2 MiB must not silently become a second limit.
#[tokio::test]
async fn playground_uses_the_inference_routers_body_limit_and_refusal() {
    let (_dir, _controller, _id, mut deps) = local().await;
    let management = management(capyctl_router::serve_router(deps.clone()));
    let session = sign_in(&management.router).await;
    let mut body = chat(false);
    body["messages"][0]["content"] = json!("x".repeat(3 << 20));
    let response = from(&management.router, "198.51.100.7:50000")
        .oneshot(browser(CHAT, &session, body.to_string()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 16 << 20).await.unwrap();
    body["messages"][0]["content"] = json!("x".repeat(5 << 20));
    let forwarded = bytes(
        from(&management.router, "198.51.100.7:50000")
            .oneshot(browser(CHAT, &session, body.to_string()))
            .await
            .unwrap(),
    )
    .await;
    deps.api_key = Some(KEY.into());
    let direct = bytes(
        api(
            &capyctl_router::serve_router(deps),
            "/v1/chat/completions",
            body.to_string(),
        )
        .await,
    )
    .await;
    assert_eq!(forwarded, direct);
    assert_eq!(forwarded.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        serde_json::from_slice::<Value>(&forwarded.1).unwrap()["code"],
        "request_too_large"
    );
}

struct Engine {
    endpoint: String,
    seen: Arc<Mutex<Vec<(String, HeaderMap, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn engine() -> Engine {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let router = Router::new().fallback(move |uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
        let recorded = recorded.clone();
        async move {
            recorded.lock().unwrap().push((uri.path().into(), headers, serde_json::from_slice(&body).unwrap()));
            if uri.path() == "/v1/systemone" {
                Response::builder().header("content-type", "application/json").body(Body::from(json!({
                    "model": "served-name", "answers": {"route": {"type": "choice", "choice": "billing", "probabilities": {"billing": 0.9, "shipping": 0.1}, "confidence": 0.8}},
                    "usage": {"input_tokens": 12, "output_tokens": 0}
                }).to_string())).unwrap()
            } else {
                let chunk = json!({"id":"chat-1","object":"chat.completion.chunk","created":1,"model":"served-name","choices":[{"index":0,"delta":{"content":ANSWER},"finish_reason":null}]}).to_string();
                Response::builder().header("content-type", "text/event-stream").body(Body::from(format!("data: {chunk}\n\ndata: [DONE]\n\n"))).unwrap()
            }
        }
    });
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(socket, router).await.unwrap() });
    Engine {
        endpoint,
        seen,
        task,
    }
}

// The existing decisions.rs lifecycle fixture's READY deployment, with all
// actual request validation, dispatch, response envelopes and SSE in the router.
struct Authority {
    api: DeploymentApi,
    endpoint: String,
    leases: Mutex<Vec<Option<LeaseEnd>>>,
}
impl Authority {
    fn row(&self) -> capyctl_store::deployments::DeploymentRow {
        capyctl_store::deployments::DeploymentRow {
            id: "dep-chat".into(),
            name: "chat".into(),
            kind: "llamacpp".into(),
            route_model_id: Some("chat".into()),
            desired_state: LifecycleState::Ready,
            observed_state: LifecycleState::Ready,
            schema_version: 1,
            current_generation: 1,
        }
    }
}
fn unsupported() -> LifecycleFault {
    LifecycleFault::Blocked("not exercised by playground fixture".into())
}
#[async_trait::async_trait]
impl capyctl_controller::LifecyclePort for Authority {
    fn find_deployment_by_route(
        &self,
        route: &str,
    ) -> Result<Option<capyctl_store::deployments::DeploymentRow>, LifecycleFault> {
        Ok((route == "chat").then(|| self.row()))
    }
    fn list_enabled_routes(
        &self,
    ) -> Result<Vec<capyctl_store::deployments::PublicRoute>, LifecycleFault> {
        Ok(vec![capyctl_store::deployments::PublicRoute {
            id: "chat".into(),
            api: self.api,
        }])
    }
    fn deployment_api(&self, _: &str) -> Result<DeploymentApi, LifecycleFault> {
        Ok(self.api)
    }
    fn get_deployment(
        &self,
        id: &str,
    ) -> Result<Option<capyctl_store::deployments::DeploymentRow>, LifecycleFault> {
        Ok((id == "dep-chat").then(|| self.row()))
    }
    fn latest_operation(
        &self,
        _: &str,
    ) -> Result<Option<capyctl_store::deployments::OperationRow>, LifecycleFault> {
        Ok(None)
    }
    fn ready_deployments_excluding(&self, _: &str) -> Result<Vec<String>, LifecycleFault> {
        Ok(Vec::new())
    }
    fn runtime_endpoint(
        &self,
        _: &str,
    ) -> Result<Option<capyctl_controller::RuntimeEndpoint>, LifecycleFault> {
        Ok(Some(capyctl_controller::RuntimeEndpoint {
            endpoint: self.endpoint.clone(),
            served_model: "served-name".into(),
            engine_key: Some("launch-key".into()),
            incarnation: "1".into(),
            engine: Some("llamacpp".into()),
            deployment_id: "dep-chat".into(),
            revision: 1,
            request_abort: false,
        }))
    }
    async fn open_request_lease(
        &self,
        _: &str,
        _: usize,
    ) -> Result<Option<RequestLease>, LeaseRefused> {
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
        self.leases.lock().unwrap()[lease.id().parse::<usize>().unwrap()] = Some(end);
        Ok(())
    }
    fn clear_suspension(&self, _: &str) -> Result<(), LifecycleFault> {
        Err(unsupported())
    }
    fn journal(
        &self,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: &str,
    ) -> Result<(), LifecycleFault> {
        Err(unsupported())
    }
    async fn observe_adapter(
        &self,
        _: &str,
    ) -> Result<capyctl_adapters::traits::WorkObservation, LifecycleFault> {
        Err(unsupported())
    }
    async fn idle_stop(
        &self,
        _: &str,
    ) -> Result<capyctl_controller::OperationHandle, LifecycleFault> {
        Err(unsupported())
    }
    async fn wait_terminal(
        &self,
        _: &capyctl_controller::OperationHandle,
    ) -> Result<LifecycleState, LifecycleFault> {
        Err(unsupported())
    }
    async fn auto_activate(
        &self,
        _: &str,
    ) -> Result<capyctl_controller::OperationHandle, LifecycleFault> {
        Err(unsupported())
    }
    async fn request_transition(
        &self,
        _: &str,
        _: LifecycleAction,
    ) -> Result<capyctl_controller::OperationHandle, LifecycleFault> {
        Err(unsupported())
    }
}

fn live(api: DeploymentApi, endpoint: &str) -> RouterDeps {
    let authority = Arc::new(Authority {
        api,
        endpoint: endpoint.into(),
        leases: Mutex::default(),
    });
    let mut deps = deps(authority.clone(), Arc::new(FakeEngine::new()));
    deps.forwards = Arc::new(capyctl_router::forwarders::LiveForwarders::new(authority));
    deps
}

// ADR 0040 §12: probabilities and API mismatch are produced by real inference;
// inspect headers at the actual HTTP engine as well as at the supplied service.
#[tokio::test]
async fn decisions_return_probabilities_chat_mismatch_and_never_browser_headers() {
    let engine = engine().await;
    let mut deps = live(DeploymentApi::Decisions, &engine.endpoint);
    let captured = Arc::default();
    let management = management(observed(
        capyctl_router::serve_router(deps.clone()),
        captured,
    ));
    let session = sign_in(&management.router).await;
    let response = from(&management.router, "198.51.100.7:50000")
        .oneshot(browser(DECISION, &session, decision().to_string()))
        .await
        .unwrap();
    let (status, body) = bytes(response).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let answer: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(answer["model"], "chat");
    assert_eq!(
        answer["answers"]["route"]["probabilities"],
        json!({"billing": 0.9, "shipping": 0.1})
    );
    let forwarded = bytes(
        from(&management.router, "198.51.100.7:50000")
            .oneshot(browser(CHAT, &session, chat(false).to_string()))
            .await
            .unwrap(),
    )
    .await;
    deps.api_key = Some(KEY.into());
    let direct = bytes(
        api(
            &capyctl_router::serve_router(deps),
            "/v1/chat/completions",
            chat(false).to_string(),
        )
        .await,
    )
    .await;
    assert_eq!(forwarded, direct);
    assert_eq!(forwarded.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_slice::<Value>(&forwarded.1).unwrap()["code"],
        "api_mismatch"
    );
    let seen = engine.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "/v1/systemone");
    assert_eq!(seen[0].2["state"], PROMPT);
    assert!(!seen[0].1.contains_key("cookie"));
    assert!(!seen[0].1.contains_key("x-capyctl-ui"));
    assert_eq!(seen[0].1["authorization"], "Bearer launch-key");
}

// ADR 0040 §12; KTD7: revocation through the management API ends an already
// streaming router response, and drops the real HTTP forward to the engine.
#[tokio::test]
async fn revoking_a_session_midstream_ends_the_forward_within_one_second() {
    let engine = HeldEngine::opening().await;
    let management = management(capyctl_router::serve_router(live(
        DeploymentApi::Chat,
        &engine.endpoint(),
    )));
    let session = sign_in(&management.router).await;
    let response = from(&management.router, "198.51.100.7:50000")
        .oneshot(browser(CHAT, &session, chat(true).to_string()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&first).contains("assistant"));
    let start = tokio::time::Instant::now();
    let response = from(&management.router, "127.0.0.1:50000")
        .oneshot(
            Request::post(format!("/management/v1/ui/sessions/{}/revoke", session.id))
                .header("authorization", format!("Bearer {ADMIN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::timeout_at(start + Duration::from_secs(1), async {
        while let Some(chunk) = stream.next().await {
            chunk.unwrap();
        }
        assert!(engine.closed_within(1, Duration::from_secs(1)).await);
    })
    .await
    .expect("revocation ends the stream and engine forward in under one second");
}

// ADR 0040 §12: use a real management HTTP connection, not just dropping a
// mocked response. Both before the first token and after the opening chunk,
// the browser's disconnection reaches the actual engine connection.
#[tokio::test]
async fn browser_disconnect_closes_the_real_engine_forward() {
    for opening in [false, true] {
        let engine = if opening {
            HeldEngine::opening().await
        } else {
            HeldEngine::start().await
        };
        let management = management(capyctl_router::serve_router(live(
            DeploymentApi::Chat,
            &engine.endpoint(),
        )));
        let session = sign_in(&management.router).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = from(&management.router, "198.51.100.7:50000");
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (opened, opening_seen) = tokio::sync::oneshot::channel();
        let client = tokio::spawn(async move {
            let response = reqwest::Client::new()
                .post(format!("http://{address}{CHAT}"))
                .header("host", HOST)
                .header("origin", format!("http://{HOST}"))
                .header("x-capyctl-ui", "1")
                .header("cookie", session.cookie)
                .json(&chat(true))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let mut stream = response.bytes_stream();
            if opening {
                let chunk = stream.next().await.unwrap().unwrap();
                assert!(String::from_utf8_lossy(&chunk).contains("assistant"));
                opened.send(()).unwrap();
            }
            futures::future::pending::<()>().await;
            drop(stream);
        });
        assert!(engine.arrived_within(1, Duration::from_secs(5)).await);
        if opening {
            tokio::time::timeout(Duration::from_secs(5), opening_seen)
                .await
                .expect("browser receives the first chunk")
                .unwrap();
        }
        client.abort();
        let _ = client.await;
        assert!(
            engine.closed_within(1, Duration::from_secs(5)).await,
            "engine stayed connected after browser disconnect (opening={opening})"
        );
        server.abort();
    }
}

struct Observations(Vec<capyctl_domain::resources::MemoryObservation>);
impl capyctl_controller::coordinator::ServiceObservation for Observations {
    fn observe(&self, _: String) -> capyctl_controller::coordinator::ObservationFuture {
        let values = self.0.clone();
        Box::pin(async move { Ok(values) })
    }
}

// ADR 0040 §12: the very store the real coordinator uses for lifecycle and
// inference leases is inspected, including its journal and live WAL.
// The CLI role-log assertion belongs in the role-process fixture, not an
// unused file here: this library fixture does not install a role log sink.
#[tokio::test]
async fn playground_bodies_do_not_enter_the_actual_owned_store_or_journal() {
    use capyctl_controller::{coordinator::CoordinatorOptions, OwnedCoordinatorState};
    let source = capyctl_testkit::fixture::owned_source().await;
    let dir = private_dir();
    let path = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let worker = capyctl_testkit::spawn_fake_coordinator(
        owner.clone(),
        Arc::new(Observations(source.observations.clone())),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions {
            poll_interval: Duration::from_millis(10),
            ..Default::default()
        },
    )
    .unwrap();
    drop(worker.start(&source.fence, 10000).unwrap());
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if owner
                .lock()
                .unwrap()
                .store()
                .runtime_binding(&source.fence.deployment_id)
                .unwrap()
                .is_some_and(|binding| binding.state == "live")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned coordinator reaches Ready");
    let engine = engine().await;
    let forward = capyctl_adapters::forward::engine_forwarder(
        engine.endpoint.parse().unwrap(),
        "served-name".into(),
        Some("launch-key".into()),
        capyctl_adapters::forward::CacheSalt::Refused,
    );
    let deps = deps(
        Arc::new(capyctl_controller::CoordinatorLifecycle::new(
            worker.commands(),
        )),
        forward,
    );
    let inflight = deps.inflight.clone();
    let management = management(capyctl_router::serve_router(deps));
    let session = sign_in(&management.router).await;
    let mut body = chat(true);
    body["model"] = json!("ordinary");
    let response = from(&management.router, "198.51.100.7:50000")
        .oneshot(browser(CHAT, &session, body.to_string()))
        .await
        .unwrap();
    let (status, body) = bytes(response).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains(ANSWER));
    assert_eq!(
        engine.seen.lock().unwrap()[0].2["messages"][0]["content"],
        PROMPT
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while inflight.current(&source.fence.deployment_id) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("real inference lease closes before inspecting persistence");
    let sql = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1",
            [&source.fence.deployment_id],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1,
        "the inspected store really owns this inference request's lease",
    );
    let journal: Vec<String> = sql
        .prepare("SELECT evidence FROM journal_entries")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for evidence in journal {
        assert!(!evidence.contains(PROMPT));
        assert!(!evidence.contains(ANSWER));
    }
    // Include live WAL bytes; a checkpoint-only scan would miss recent writes.
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            let bytes = std::fs::read(entry.path()).unwrap();
            for sentinel in [PROMPT, ANSWER] {
                assert!(
                    !bytes
                        .windows(sentinel.len())
                        .any(|window| window == sentinel.as_bytes()),
                    "{} leaked into {}",
                    sentinel,
                    entry.path().display()
                );
            }
        }
    }
    worker.shutdown().await.unwrap();
}

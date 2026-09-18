//! Router core (F1 design §5): /v1/models never wakes; chat completions
//! admit against bounds and dispatch only to READY deployments; auth is
//! API-key; queue limits return structured errors.


use tower::ServiceExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mllm_adapters::fake::FakeEngine;
use mllm_controller::Controller;
use mllm_router::forwarders::StaticForwarders;
use mllm_router::{QueueLimits, RouterDeps};
use mllm_store::Store;

async fn app() -> (
    axum::Router,
    Arc<Mutex<Store>>,
    Arc<Controller>,
    mllm_store::Store,
    mllm_router::RouterDeps,
) {
    let shared = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let fake = Arc::new(FakeEngine::new());
    let adapter = fake.clone() as Arc<dyn mllm_adapters::ChatForward>;
    let controller = Arc::new(Controller::new(
        shared.clone(),
        fake.clone() as Arc<dyn mllm_adapters::EngineAdapter>,
        Arc::new(mllm_adapters::fake::FakeLauncher::new()),
    ));
    let deps = RouterDeps {
        controller: controller.clone(),
        forwards: Arc::new(StaticForwarders(HashMap::from([(
            "fake".to_string(),
            adapter,
        )]))),
        limits: QueueLimits { max_requests_per_deployment: 2, max_buffered_bytes_total: 1024 },
        api_key: Some("test-key".into()),
        inflight: Arc::new(mllm_router::admission::InFlight::default()),
        activation_join: Arc::new(mllm_router::WakeJoin::new()),
    };
    let file_store = Store::open_in_memory().unwrap();
    (mllm_router::serve_router(deps.clone()), shared, controller, file_store, deps)
}

async fn deploy_ready(
    _router: &axum::Router,
    controller: &Controller,
    name: &str,
) -> String {
    let id = controller
        .submit_deploy(mllm_controller::DeployRequest {
            name: name.into(),
            kind: "fake".into(),
            manifest: format!("name: {name}\nkind: fake\n").into_bytes(),
            route_model_id: Some(name.into()),
        })
        .await
        .unwrap();
    let op = controller
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await
        .unwrap();
    controller.wait_terminal(&op).await.unwrap();
    id
}

#[tokio::test]
async fn models_lists_enabled_never_wakes() {
    let (router, store, controller, _fs, _deps) = app().await;
    let _ = deploy_ready(&router, &controller, "m1").await;
    let _ = store;
    let res = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/models")
                .header("Authorization", "Bearer test-key")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["data"][0]["id"], "m1");
    // Never wakes: observed state unchanged (Ready, since it was ready — but
    // a STOPPED deployment stays STOPPED).
}

#[tokio::test]
async fn models_does_not_activate_stopped_deployment() {
    let (router, store, controller, _fs, _deps) = app().await;
    let id = controller
        .submit_deploy(mllm_controller::DeployRequest {
            name: "stopped-m".into(),
            kind: "fake".into(),
            manifest: b"name: stopped-m\n".to_vec(),
            route_model_id: Some("stopped-m".into()),
        })
        .await
        .unwrap();
    let res = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/models")
                .header("Authorization", "Bearer test-key")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    // The stopped deployment is still listed (enabled route), NOT activated.
    let row = store.lock().unwrap().get_deployment(&id).unwrap().unwrap();
    assert_eq!(row.observed_state, mllm_domain::LifecycleState::Stopped);
}

#[tokio::test]
async fn unauthenticated_requests_rejected() {
    let (router, _s, _c, _f, _deps) = app().await;
    let res = router
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/models")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn chat_dispatches_to_ready_deployment() {
    let (router, _s, controller, _f, _deps) = app().await;
    deploy_ready(&router, &controller, "ready-m").await;
    let res = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("Authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"model": "ready-m", "messages": [{"role": "user", "content": "hi"}]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["choices"][0]["message"]["content"].is_string());
}

#[tokio::test]
async fn queue_bounds_return_structured_error() {
    let (router, _s, controller, _f, _deps) = app().await;
    deploy_ready(&router, &controller, "bounded-m").await;
    // max_buffered_bytes_total = 1024; a body larger than that is rejected.
    let big = "x".repeat(4096);
    let res = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("Authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"model": "bounded-m", "messages": [{"role": "user", "content": big}]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 413);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["code"], "queue_full");
}

#[tokio::test]
async fn concurrent_resolves_join_one_wake_at_router_tier() {
    // T15 at the router tier (auto-activation join): two simultaneous
    // requests to the SAME non-READY deployment must produce exactly one
    // Start operation — never a double-spawn or duplicate wake.
    let (_router, store, controller, _fs, deps) = app().await;
    let id = controller
        .submit_deploy(mllm_controller::DeployRequest {
            name: "wake-router-m".into(),
            kind: "fake".into(),
            manifest: b"name: wake-router-m\n".to_vec(),
            route_model_id: Some("wake-router-m".into()),
        })
        .await
        .unwrap();
    let (r1, r2) = tokio::join!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            mllm_router::chat::resolve(&deps, "wake-router-m"),
        ),
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            mllm_router::chat::resolve(&deps, "wake-router-m"),
        ),
    );
    // Both resolve successfully (the timeout proves no hang).
    let (d1, d2) = (r1.unwrap().unwrap(), r2.unwrap().unwrap());
    assert_eq!(d1, id);
    assert_eq!(d2, id);
    let starts = store
        .lock()
        .unwrap()
        .operations_of_kind(&id, "start")
        .unwrap()
        .len();
    assert_eq!(starts, 1, "exactly one Start operation (T15, router tier)");
}
// ---------------------------------------------------------------------------
// The router reaches the engine the lifecycle authority says is running.
// ---------------------------------------------------------------------------

/// A lifecycle authority that serves one READY deployment and reports one runtime,
/// which the test rotates under it. Every other capability is refused rather than
/// approximated: a stub that answered them would be asserting behaviour this test
/// does not exercise.
struct StubAuthority {
    deployment: String,
    route: String,
    runtime: Mutex<mllm_controller::RuntimeEndpoint>,
}

impl StubAuthority {
    fn refuse(what: &str) -> mllm_controller::LifecycleFault {
        mllm_controller::LifecycleFault::Blocked(format!("the stub authority cannot {what}"))
    }

    fn row(&self) -> mllm_store::deployments::DeploymentRow {
        mllm_store::deployments::DeploymentRow {
            id: self.deployment.clone(),
            name: "stub".into(),
            kind: "vllm".into(),
            route_model_id: Some(self.route.clone()),
            desired_state: mllm_domain::LifecycleState::Ready,
            observed_state: mllm_domain::LifecycleState::Ready,
            schema_version: 1,
            current_generation: 1,
        }
    }
}

#[async_trait::async_trait]
impl mllm_controller::LifecyclePort for StubAuthority {
    fn find_deployment_by_route(
        &self,
        route: &str,
    ) -> Result<Option<mllm_store::deployments::DeploymentRow>, mllm_controller::LifecycleFault>
    {
        Ok((route == self.route).then(|| self.row()))
    }
    fn list_enabled_route_ids(&self) -> Result<Vec<String>, mllm_controller::LifecycleFault> {
        Ok(vec![self.route.clone()])
    }
    fn get_deployment(
        &self,
        id: &str,
    ) -> Result<Option<mllm_store::deployments::DeploymentRow>, mllm_controller::LifecycleFault>
    {
        Ok((id == self.deployment).then(|| self.row()))
    }
    fn latest_operation(
        &self,
        _deployment_id: &str,
    ) -> Result<Option<mllm_store::deployments::OperationRow>, mllm_controller::LifecycleFault>
    {
        Ok(None)
    }
    fn ready_deployments_excluding(
        &self,
        _deployment: &str,
    ) -> Result<Vec<String>, mllm_controller::LifecycleFault> {
        Ok(Vec::new())
    }
    fn runtime_endpoint(
        &self,
        deployment: &str,
    ) -> Result<Option<mllm_controller::RuntimeEndpoint>, mllm_controller::LifecycleFault> {
        if deployment != self.deployment {
            return Ok(None);
        }
        Ok(Some(self.runtime.lock().unwrap().clone()))
    }
    fn clear_suspension(&self, _d: &str) -> Result<(), mllm_controller::LifecycleFault> {
        Err(Self::refuse("clear a suspension"))
    }
    fn journal(
        &self,
        _host_id: Option<&str>,
        _operation_id: Option<&str>,
        _state: Option<&str>,
        _evidence: &str,
    ) -> Result<(), mllm_controller::LifecycleFault> {
        Err(Self::refuse("journal"))
    }
    async fn observe_adapter(
        &self,
        _deployment: &str,
    ) -> Result<mllm_adapters::traits::WorkObservation, mllm_controller::LifecycleFault> {
        Err(Self::refuse("observe engine work"))
    }
    async fn idle_stop(
        &self,
        _deployment: &str,
    ) -> Result<mllm_controller::OperationHandle, mllm_controller::LifecycleFault> {
        Err(Self::refuse("stop a deployment"))
    }
    async fn wait_terminal(
        &self,
        _handle: &mllm_controller::OperationHandle,
    ) -> Result<mllm_domain::LifecycleState, mllm_controller::LifecycleFault> {
        Err(Self::refuse("await an operation"))
    }
    async fn auto_activate(
        &self,
        _deployment: &str,
    ) -> Result<mllm_controller::OperationHandle, mllm_controller::LifecycleFault> {
        Err(Self::refuse("activate a deployment"))
    }
    async fn request_transition(
        &self,
        _deployment: &str,
        _action: mllm_domain::LifecycleAction,
    ) -> Result<mllm_controller::OperationHandle, mllm_controller::LifecycleFault> {
        Err(Self::refuse("request a transition"))
    }
}

/// What the stub engine will accept: the key of the launch it belongs to, and the
/// name that launch was told to serve.
#[derive(Clone)]
struct EngineExpects {
    key: String,
    served_model: String,
}

/// One SSE completion, in the shape the forwarder's parser requires.
fn engine_sse(model: &str) -> String {
    format!(
        "data: {}\r\n\r\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chat-1",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": model,
            "choices": [{"index":0,"delta":{"content":"hello"},"finish_reason":"stop"}],
        })
    )
}

/// An engine that answers only a caller presenting its own launch's key and
/// addressing it by the name it serves. Anything else is refused, which is how a
/// stale forwarder becomes visible as a failure rather than a silent success.
async fn stub_engine(expects: Arc<Mutex<EngineExpects>>) -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(
            move |headers: axum::http::HeaderMap,
                  axum::Json(body): axum::Json<serde_json::Value>| {
                let expects = expects.clone();
                async move {
                    let expected = expects.lock().unwrap().clone();
                    let presented = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    if presented != format!("Bearer {}", expected.key)
                        || body["model"] != expected.served_model
                    {
                        return (
                            axum::http::StatusCode::UNAUTHORIZED,
                            [("content-type", "application/json")],
                            "{\"error\":\"wrong key or model\"}".to_string(),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        [("content-type", "text/event-stream")],
                        engine_sse(&expected.served_model),
                    )
                }
            },
        ),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    base
}

async fn chat_once(router: &axum::Router, alias: &str) -> (axum::http::StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "model": alias,
                        "messages": [{"role":"user","content":"hi"}],
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

/// Spec §3: the router resolves endpoint and key per deployment at request time. T19
///
/// The forwarding table this replaces was built once at boot and keyed by engine
/// family. A leased port and a per-launch key make that table wrong the moment a
/// deployment restarts, and wrong in the worst way: it keeps presenting a retired
/// credential to whatever now holds the port.
#[tokio::test]
async fn the_router_forwards_to_the_deployments_live_endpoint_with_its_key() {
    let expects = Arc::new(Mutex::new(EngineExpects {
        key: "k3y".into(),
        served_model: "served-name".into(),
    }));
    let endpoint = stub_engine(expects.clone()).await;
    let authority = Arc::new(StubAuthority {
        deployment: "dep-1".into(),
        route: "public-alias".into(),
        runtime: Mutex::new(mllm_controller::RuntimeEndpoint {
            endpoint: endpoint.clone(),
            served_model: "served-name".into(),
            engine_key: Some("k3y".into()),
            incarnation: "1".into(),
        }),
    });
    let port = authority.clone() as Arc<dyn mllm_controller::LifecyclePort>;
    let router = mllm_router::serve_router(RouterDeps {
        controller: port.clone(),
        forwards: Arc::new(mllm_router::forwarders::LiveForwarders::new(port)),
        limits: QueueLimits {
            max_requests_per_deployment: 4,
            max_buffered_bytes_total: 64 * 1024,
        },
        api_key: None,
        inflight: Arc::new(mllm_router::admission::InFlight::default()),
        activation_join: Arc::new(mllm_router::WakeJoin::new()),
    });

    let (status, body) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 200, "the first launch's key reaches its engine: {body}");
    assert_eq!(body["choices"][0]["message"]["content"], "hello");
    // The client asked for the alias and is answered in its own terms; the served
    // name is what went upstream and never leaks back.
    assert_eq!(body["model"], "public-alias");

    // A restart: new incarnation, new key. Nothing about the deployment changed.
    expects.lock().unwrap().key = "k4y".into();
    {
        let mut runtime = authority.runtime.lock().unwrap();
        runtime.engine_key = Some("k4y".into());
        runtime.incarnation = "2".into();
    }

    let (status, body) = chat_once(&router, "public-alias").await;
    assert_eq!(
        status, 200,
        "the second launch's key is used, not the one cached for the first: {body}"
    );
    assert_eq!(body["choices"][0]["message"]["content"], "hello");
}

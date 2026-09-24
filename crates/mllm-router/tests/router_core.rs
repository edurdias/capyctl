//! Router core (F1 design §5): /v1/models never wakes; chat completions
//! admit against bounds and dispatch only to READY deployments; auth is
//! API-key; queue limits return structured errors.


use futures::StreamExt;
use tower::ServiceExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mllm_testkit::FakeEngine;
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
        Arc::new(mllm_testkit::FakeLauncher::new()),
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

/// SPEC §9.1 / §13.3, ADR 0012: default-on deep parking never makes an engine
/// control path public. An authenticated client still cannot reach one
/// through the router.
// T21
#[tokio::test]
async fn the_router_never_forwards_engine_control_paths() {
    let (router, _s, _c, _f, _deps) = app().await;
    for path in [
        "/sleep",
        "/wake_up",
        "/collective_rpc",
        "/is_sleeping",
        "/v1/sleep",
        "/v1/collective_rpc",
    ] {
        let res = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("Authorization", "Bearer test-key")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(!res.status().is_success(), "{path}: {}", res.status());
    }
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
    // SPEC §10: an oversized body is a body-size refusal (413), not a full
    // queue; retrying the same body can never succeed.
    assert_eq!(res.status(), 413);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["code"], "request_too_large");
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
    /// SPEC §10: every lease opened, with how it was closed (`None` while open).
    leases: Mutex<Vec<(String, Option<mllm_controller::LeaseEnd>)>>,
    /// When set, the ledger refuses the next grants this way.
    refuse_leases: Mutex<Option<mllm_controller::LeaseRefused>>,
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
    async fn open_request_lease(
        &self,
        _deployment: &str,
        _max_per_deployment: usize,
    ) -> Result<Option<mllm_controller::RequestLease>, mllm_controller::LeaseRefused> {
        if let Some(refused) = self.refuse_leases.lock().unwrap().clone() {
            return Err(refused);
        }
        let mut leases = self.leases.lock().unwrap();
        let id = format!("lease-{}", leases.len());
        leases.push((id.clone(), None));
        Ok(Some(mllm_controller::RequestLease::unrecorded(&id)))
    }
    async fn close_request_lease(
        &self,
        lease: mllm_controller::RequestLease,
        end: mllm_controller::LeaseEnd,
    ) -> Result<(), mllm_controller::LeaseRefused> {
        let mut leases = self.leases.lock().unwrap();
        let entry = leases
            .iter_mut()
            .find(|(id, _)| id == lease.id())
            .expect("a lease this authority granted");
        assert!(entry.1.is_none(), "a lease is closed once");
        entry.1 = Some(end);
        Ok(())
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
        leases: Mutex::new(Vec::new()),
        refuse_leases: Mutex::new(None),
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

/// An engine that answers every chat request with one scripted status, body and
/// content type, counting how many requests reached it.
async fn scripted_engine(
    status: u16,
    content_type: &'static str,
    body: String,
) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = hits.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let body = body.clone();
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    [("content-type", content_type)],
                    body,
                )
            }
        }),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (base, hits)
}

fn stub_router(endpoint: &str) -> (Arc<StubAuthority>, RouterDeps) {
    let authority = Arc::new(StubAuthority {
        deployment: "dep-1".into(),
        route: "public-alias".into(),
        runtime: Mutex::new(mllm_controller::RuntimeEndpoint {
            endpoint: endpoint.into(),
            served_model: "served-name".into(),
            engine_key: None,
            incarnation: "1".into(),
        }),
        leases: Mutex::new(Vec::new()),
        refuse_leases: Mutex::new(None),
    });
    let port = authority.clone() as Arc<dyn mllm_controller::LifecyclePort>;
    let deps = RouterDeps {
        controller: port.clone(),
        forwards: Arc::new(mllm_router::forwarders::LiveForwarders::new(port)),
        limits: QueueLimits {
            max_requests_per_deployment: 4,
            max_buffered_bytes_total: 64 * 1024,
        },
        api_key: None,
        inflight: Arc::new(mllm_router::admission::InFlight::default()),
        activation_join: Arc::new(mllm_router::WakeJoin::new()),
    };
    (authority, deps)
}

async fn chat_stream(router: &axum::Router, alias: &str) -> (axum::http::StatusCode, String) {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "model": alias, "stream": true,
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
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn ends(authority: &StubAuthority) -> Vec<Option<mllm_controller::LeaseEnd>> {
    authority.leases.lock().unwrap().iter().map(|(_, end)| *end).collect()
}

// T17 T19 (SPEC §10, owner decision 2026-09-22): every dispatch holds a durable
// lease from before the engine sees it; completion closes it.
#[tokio::test]
async fn a_completed_dispatch_opens_and_closes_one_lease() {
    let (endpoint, hits) =
        scripted_engine(200, "text/event-stream", engine_sse("served-name")).await;
    let (authority, deps) = stub_router(&endpoint);
    let router = mllm_router::serve_router(deps.clone());
    let (status, body) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ends(&authority), vec![Some(mllm_controller::LeaseEnd::Completed)]);
    let (status, text) = chat_stream(&router, "public-alias").await;
    assert_eq!(status, 200);
    assert!(text.contains("[DONE]"), "{text}");
    assert_eq!(
        ends(&authority),
        vec![Some(mllm_controller::LeaseEnd::Completed); 2],
        "the stream held its lease until the backend ended"
    );
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(deps.inflight.current("dep-1"), 0);
}

// T17 T38: a host ingress that is shutting down refuses before forwarding. The
// router answers 503 shutting_down, retryable, never 500, and closes the lease
// on that evidence.
#[tokio::test]
async fn a_shutting_down_refusal_is_a_retryable_503_that_closes_the_lease() {
    let refusal = serde_json::json!({"error":{"code":"shutting_down","message":"restarting","retryable":true}});
    let (endpoint, _) = scripted_engine(503, "application/json", refusal.to_string()).await;
    let (authority, deps) = stub_router(&endpoint);
    let router = mllm_router::serve_router(deps.clone());
    let (status, body) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["code"], "shutting_down");
    assert_eq!(body["retryable"], true);
    assert_eq!(ends(&authority), vec![Some(mllm_controller::LeaseEnd::NotAccepted)]);
    assert_eq!(deps.inflight.current("dep-1"), 0, "nothing was accepted");
    let (status, text) = chat_stream(&router, "public-alias").await;
    assert_eq!(status, 200);
    assert!(text.contains("shutting_down") && !text.contains("[DONE]"), "{text}");
    assert_eq!(ends(&authority)[1], Some(mllm_controller::LeaseEnd::NotAccepted));
}

// T17 T19 (SPEC §10): an engine failure after the request may have been
// accepted keeps the durable lease charged as uncertain. The per-process slot is
// released, so uncertain ends never shrink the deployment's in-flight bound for
// the life of the router: more requests than the bound still get through.
#[tokio::test]
async fn an_unverified_failure_keeps_the_lease_charged() {
    let (endpoint, _) = scripted_engine(503, "application/json", "{}".into()).await;
    let (authority, deps) = stub_router(&endpoint);
    let router = mllm_router::serve_router(deps.clone());
    let bound = deps.limits.max_requests_per_deployment;
    for _ in 0..bound + 2 {
        let (status, body) = chat_once(&router, "public-alias").await;
        assert_eq!(status, 500, "never a stale 429 from leaked slots: {body}");
    }
    let (status, text) = chat_stream(&router, "public-alias").await;
    assert_eq!(status, 200);
    assert!(!text.contains("[DONE]"), "{text}");
    assert_eq!(
        ends(&authority),
        vec![Some(mllm_controller::LeaseEnd::Uncertain); bound + 3]
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while deps.inflight.current("dep-1") != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the in-memory slots are released");
}

// T18: when the ledger refuses because the gate closed (a host draining, a
// readiness re-proof), nothing reaches the engine and the client gets a
// retryable 503, not a 500.
#[tokio::test]
async fn a_closed_gate_refuses_before_the_engine_with_a_retryable_503() {
    let (endpoint, hits) =
        scripted_engine(200, "text/event-stream", engine_sse("served-name")).await;
    let (authority, deps) = stub_router(&endpoint);
    *authority.refuse_leases.lock().unwrap() = Some(mllm_controller::LeaseRefused::Closed);
    let router = mllm_router::serve_router(deps.clone());
    let (status, body) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["retryable"], true);
    let (status, _) = chat_stream(&router, "public-alias").await;
    assert_eq!(status, 503);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(deps.inflight.current("dep-1"), 0);
    *authority.refuse_leases.lock().unwrap() = Some(mllm_controller::LeaseRefused::Full);
    // T19, SPEC §10 bounded queues: a full queue is "too many requests, retry",
    // 429 with Retry-After; 413 is reserved for body size.
    let (status, body) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 429, "{body}");
    assert_eq!(body["code"], "queue_full");
    assert_eq!(body["retryable"], true);
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"model": "public-alias", "stream": true,
                                       "messages": [{"role":"user","content":"hi"}]})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert!(response.headers().contains_key(axum::http::header::RETRY_AFTER));
}

/// Send one raw chat body (as bytes) with optional headers.
async fn post_raw(
    router: &axum::Router,
    body: Vec<u8>,
    authorization: Option<&str>,
) -> (axum::http::StatusCode, serde_json::Value) {
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(value) = authorization {
        request = request.header("Authorization", value);
    }
    let response = router
        .clone()
        .oneshot(request.body(axum::body::Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

fn chat_body(alias: &str, extra: serde_json::Value) -> Vec<u8> {
    let mut body = serde_json::json!({"model": alias, "messages": [{"role":"user","content":"hi"}]});
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        body[key] = value;
    }
    body.to_string().into_bytes()
}

// T19 T21 (SPEC §10, §14): a request that can never be forwarded is a 400 decided
// before any accounting, lease or engine: malformed JSON, an engine-internal
// field, or a parameter the relay cannot answer faithfully (n > 1, legacy
// functions). SPEC §10 preserves tool calls, so `tools` is forwarded and the
// engine's tool calls come back.
#[tokio::test]
async fn unforwardable_requests_are_400_before_any_engine_and_tools_are_forwarded() {
    let tool_sse = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"id":"chat-1","object":"chat.completion.chunk","created":1,
            "model":"served-name","choices":[{"index":0,"delta":{"role":"assistant",
            "tool_calls":[{"index":0,"id":"call_1","type":"function",
            "function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":"tool_calls"}]})
    );
    let (endpoint, hits) = scripted_engine(200, "text/event-stream", tool_sse).await;
    let (authority, deps) = stub_router(&endpoint);
    let router = mllm_router::serve_router(deps.clone());
    for (body, code) in [
        (b"{not json".to_vec(), "invalid_request"),
        (chat_body("public-alias", serde_json::json!({"rid": "x"})), "invalid_request"),
        (chat_body("public-alias", serde_json::json!({"n": 2})), "unsupported_parameter"),
        (chat_body("public-alias", serde_json::json!({"functions": []})), "unsupported_parameter"),
        (chat_body("public-alias", serde_json::json!({"n": 3, "stream": true})), "unsupported_parameter"),
    ] {
        let (status, answer) = post_raw(&router, body, None).await;
        assert_eq!(status, 400, "{answer}");
        assert_eq!(answer["code"], code, "{answer}");
    }
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(ends(&authority).is_empty(), "no lease was opened");
    assert_eq!(deps.inflight.current("dep-1"), 0);
    let tools = serde_json::json!({"tools": [{"type":"function","function":{"name":"lookup"}}],
                                   "tool_choice": "auto"});
    let (status, answer) = post_raw(&router, chat_body("public-alias", tools), None).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(answer["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "lookup");
    assert_eq!(ends(&authority), vec![Some(mllm_controller::LeaseEnd::Completed)]);
}

// T19 (SPEC §10: limit body size): the router's configured bound is the body
// limit, not axum's implicit 2 MiB default; past it the refusal is the
// structured 413.
#[tokio::test]
async fn the_configured_body_bound_is_the_request_limit() {
    let (endpoint, hits) =
        scripted_engine(200, "text/event-stream", engine_sse("served-name")).await;
    let (_authority, mut deps) = stub_router(&endpoint);
    deps.limits.max_buffered_bytes_total = 4 << 20;
    let router = mllm_router::serve_router(deps.clone());
    let large = serde_json::json!({"messages": [{"role":"user","content":"x".repeat(3 << 20)}]});
    let mut body = large.clone();
    body["model"] = serde_json::json!("public-alias");
    let (status, answer) = post_raw(&router, body.to_string().into_bytes(), None).await;
    assert_eq!(status, 200, "a 3 MiB body under a 4 MiB bound is served: {answer}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    body["messages"][0]["content"] = serde_json::json!("x".repeat(5 << 20));
    let (status, answer) = post_raw(&router, body.to_string().into_bytes(), None).await;
    assert_eq!(status, 413);
    assert_eq!(answer["code"], "request_too_large");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
}

// T37 (SPEC §13.3): the API key check accepts the Bearer scheme in any case and
// refuses a wrong, longer or missing key.
#[tokio::test]
async fn the_api_key_scheme_is_case_insensitive_and_the_key_exact() {
    let (router, _s, _c, _f, _deps) = app().await;
    for (header, expected) in [
        (Some("Bearer test-key"), 200),
        (Some("bearer test-key"), 200),
        (Some("BEARER test-key"), 200),
        (Some("Bearer test-kex"), 401),
        (Some("Bearer test-key2"), 401),
        (Some("Basic test-key"), 401),
        (Some("Bearertest-key"), 401),
        (None, 401),
    ] {
        let mut request = axum::http::Request::builder().uri("/v1/models");
        if let Some(value) = header {
            request = request.header("Authorization", value);
        }
        let response = router
            .clone()
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{header:?}");
    }
}

/// An engine that records each request's first message and answers only when
/// a permit is released, one per request.
async fn gated_engine() -> (String, Arc<tokio::sync::Semaphore>, Arc<Mutex<Vec<String>>>) {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let order = Arc::new(Mutex::new(Vec::new()));
    let (held, seen) = (gate.clone(), order.clone());
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let (gate, seen) = (held.clone(), seen.clone());
            async move {
                seen.lock()
                    .unwrap()
                    .push(body["messages"][0]["content"].as_str().unwrap_or_default().to_owned());
                gate.acquire().await.unwrap().forget();
                (
                    axum::http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    engine_sse("served-name"),
                )
            }
        }),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (base, gate, order)
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

// T19 (SPEC §10: bounded waiting, oldest first): at the in-flight bound a
// request waits for a slot instead of an instant 429, slots go out in arrival
// order, a fresh arrival queues behind requests already waiting, and only a
// request whose deadline passes while waiting is refused, retryably.
#[tokio::test]
async fn at_the_in_flight_bound_requests_wait_in_arrival_order() {
    let (endpoint, gate, order) = gated_engine().await;
    let (_authority, mut deps) = stub_router(&endpoint);
    deps.limits.max_requests_per_deployment = 1;
    let router = mllm_router::serve_router(deps.clone());
    let send = |content: &'static str| {
        let router = router.clone();
        tokio::spawn(async move {
            let body = serde_json::json!({"model":"public-alias",
                "messages":[{"role":"user","content":content}]});
            post_raw(&router, body.to_string().into_bytes(), None).await
        })
    };
    let first = send("r1");
    until("r1 at the engine", || order.lock().unwrap().len() == 1).await;
    let second = send("r2");
    until("r2 waiting", || deps.inflight.slot_waiters("dep-1") == 1).await;
    let third = send("r3");
    until("r3 waiting", || deps.inflight.slot_waiters("dep-1") == 2).await;
    for _ in 0..3 {
        gate.add_permits(1);
    }
    for request in [first, second, third] {
        let (status, answer) = request.await.unwrap();
        assert_eq!(status, 200, "{answer}");
    }
    assert_eq!(*order.lock().unwrap(), vec!["r1", "r2", "r3"]);

    // A waiter whose deadline passes is refused retryably, after waiting. The
    // holder was admitted under the default deadline, so only the waiter's
    // wait is short.
    let holder = send("r4");
    until("r4 at the engine", || order.lock().unwrap().len() == 4).await;
    deps.inflight.waiting.set_limits(mllm_router::queue::WaitLimits {
        deadline: std::time::Duration::from_millis(300),
        ..Default::default()
    });
    let started = std::time::Instant::now();
    let (status, answer) = send("r5").await.unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_millis(250), "waited first");
    assert_eq!(status, 429, "{answer}");
    assert_eq!(answer["retryable"], true);
    assert_eq!(deps.inflight.slot_waiters("dep-1"), 0, "it left the queue");
    assert_eq!(deps.inflight.waiting.totals(), (0, 0));
    gate.add_permits(1);
    assert_eq!(holder.await.unwrap().0, 200);
}

// T19 T17 (SPEC §10): a non-streaming request is bounded like a stream — its
// first backend event by the request deadline, later ones by the idle bound —
// not by a fixed 300 s / 60 s cap. A backend that stalls after its first event
// is cut at the idle bound and its lease stays uncertain.
#[tokio::test]
async fn a_non_streaming_request_is_bounded_by_the_stream_idle_bound() {
    let first = format!(
        "data: {}\n\n",
        serde_json::json!({"id":"chat-1","object":"chat.completion.chunk","created":1,
            "model":"served-name","choices":[{"index":0,"delta":{"content":"he"},"finish_reason":null}]})
    );
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let first = first.clone();
            async move {
                let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(first) })
                    .chain(futures::stream::pending());
                ([("content-type", "text/event-stream")], axum::body::Body::from_stream(stream))
            }
        }),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    let (authority, deps) = stub_router(&endpoint);
    deps.inflight.waiting.set_limits(mllm_router::queue::WaitLimits {
        stream_idle: std::time::Duration::from_millis(300),
        ..Default::default()
    });
    let router = mllm_router::serve_router(deps.clone());
    let (status, answer) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        chat_once(&router, "public-alias"),
    )
    .await
    .expect("cut at the idle bound, not a fixed 60 s read timeout");
    assert_eq!(status, 500, "{answer}");
    assert_eq!(answer["code"], "engine_error");
    assert_eq!(ends(&authority), vec![Some(mllm_controller::LeaseEnd::Uncertain)]);
}

// T15 (SPEC §10 step 2): a detached activation whose task panics answers its
// waiters and frees its slot, so the next request starts a fresh activation
// instead of joining a claim nobody completes.
#[tokio::test]
async fn a_panicking_activation_answers_its_waiters_and_frees_its_slot() {
    let join: Arc<mllm_router::WakeJoin<String>> = Arc::new(mllm_router::WakeJoin::new());
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        join.join_detached(
            "dep",
            || async {
                if true {
                    panic!("activation task panicked");
                }
                Ok(0)
            },
            "aborted".to_owned(),
        ),
    )
    .await
    .expect("a waiter is answered");
    assert_eq!(outcome, Err("aborted".to_owned()));
    let again = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        join.join_detached("dep", || async { Ok(7) }, "aborted".to_owned()),
    )
    .await
    .expect("a fresh wake runs");
    assert_eq!(again, Ok(7));
}

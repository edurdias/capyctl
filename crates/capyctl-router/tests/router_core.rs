//! Router core (F1 design §5): /v1/models never wakes; chat completions
//! admit against bounds and dispatch only to READY deployments; auth is
//! API-key; queue limits return structured errors.

use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

use capyctl_controller::Controller;
use capyctl_router::forwarders::StaticForwarders;
use capyctl_router::{QueueLimits, RouterDeps};
use capyctl_store::Store;
use capyctl_testkit::FakeEngine;

async fn app() -> (
    axum::Router,
    Arc<Mutex<Store>>,
    Arc<Controller>,
    capyctl_store::Store,
    capyctl_router::RouterDeps,
) {
    let shared = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let fake = Arc::new(FakeEngine::new());
    let adapter = fake.clone() as Arc<dyn capyctl_adapters::ChatForward>;
    let controller = Arc::new(Controller::new(
        shared.clone(),
        fake.clone() as Arc<dyn capyctl_adapters::EngineAdapter>,
        Arc::new(capyctl_testkit::FakeLauncher::new()),
    ));
    let deps = RouterDeps {
        controller: controller.clone(),
        forwards: Arc::new(StaticForwarders(HashMap::from([(
            "fake".to_string(),
            adapter,
        )]))),
        limits: QueueLimits {
            max_requests_per_deployment: 2,
            max_buffered_bytes_total: 1024,
        },
        api_key: Some("test-key".into()),
        inflight: Arc::new(capyctl_router::admission::InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    };
    let file_store = Store::open_in_memory().unwrap();
    (
        capyctl_router::serve_router(deps.clone()),
        shared,
        controller,
        file_store,
        deps,
    )
}

async fn deploy_ready(_router: &axum::Router, controller: &Controller, name: &str) -> String {
    let id = controller
        .submit_deploy(capyctl_controller::DeployRequest {
            name: name.into(),
            kind: "fake".into(),
            manifest: format!("name: {name}\nkind: fake\n").into_bytes(),
            route_model_id: Some(name.into()),
        })
        .await
        .unwrap();
    let op = controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
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
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["data"][0]["id"], "m1");
    // Never wakes: observed state unchanged (Ready, since it was ready — but
    // a STOPPED deployment stays STOPPED).
}

#[tokio::test]
async fn models_does_not_activate_stopped_deployment() {
    let (router, store, controller, _fs, _deps) = app().await;
    let id = controller
        .submit_deploy(capyctl_controller::DeployRequest {
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
    assert_eq!(row.observed_state, capyctl_domain::LifecycleState::Stopped);
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
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
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
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
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
        .submit_deploy(capyctl_controller::DeployRequest {
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
            capyctl_router::chat::resolve(&deps, "wake-router-m"),
        ),
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            capyctl_router::chat::resolve(&deps, "wake-router-m"),
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
    runtime: Mutex<capyctl_controller::RuntimeEndpoint>,
    /// SPEC §10: every lease opened, with how it was closed (`None` while open).
    leases: Mutex<Vec<(String, Option<capyctl_controller::LeaseEnd>)>>,
    /// When set, the ledger refuses the next grants this way.
    refuse_leases: Mutex<Option<capyctl_controller::LeaseRefused>>,
    /// ADR 0028 §11: the instance it serves, and the group stalls reported.
    instances: InstanceStub,
    /// SPEC §10 (owner decision 2026-10-18): holds lease closes back.
    closes: CloseGate,
    /// ADR 0032: aborts, acknowledgements and restarts asked of it.
    abort: AbortStub,
}

/// ADR 0032 §3–§4, §7: what the stub was asked about cancelled requests,
/// and how it answers.
struct AbortStub {
    /// Whether an abort is reported sent.
    sends: std::sync::atomic::AtomicBool,
    /// Every abort asked: deployment, generation and request ID.
    asked: Mutex<Vec<(String, i64, String)>>,
    /// One permit per abort sent, for an engine double that ends a request
    /// when it is aborted.
    aborted: Arc<tokio::sync::Semaphore>,
    /// Every lease acknowledged by its own final output.
    acks: Mutex<Vec<String>>,
    /// The answer to a restart request, and every deployment asked.
    restart: Mutex<capyctl_controller::HeldRestart>,
    restarts: Mutex<Vec<String>>,
    /// While held, the activation a request waits for after a restart waits
    /// for a permit; every such activation is counted.
    activation_held: std::sync::atomic::AtomicBool,
    activation_permits: tokio::sync::Semaphore,
    activations: std::sync::atomic::AtomicUsize,
}

impl Default for AbortStub {
    fn default() -> Self {
        Self {
            sends: std::sync::atomic::AtomicBool::new(true),
            asked: Mutex::new(Vec::new()),
            aborted: Arc::new(tokio::sync::Semaphore::new(0)),
            acks: Mutex::new(Vec::new()),
            restart: Mutex::new(capyctl_controller::HeldRestart::NotHeld),
            restarts: Mutex::new(Vec::new()),
            activation_held: std::sync::atomic::AtomicBool::new(false),
            activation_permits: tokio::sync::Semaphore::new(0),
            activations: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

/// SPEC §10 (owner decision 2026-10-18): while held, every lease close waits
/// for a permit, so a test sees a request between its answer and its close.
struct CloseGate {
    held: std::sync::atomic::AtomicBool,
    permits: tokio::sync::Semaphore,
}

impl Default for CloseGate {
    fn default() -> Self {
        Self {
            held: std::sync::atomic::AtomicBool::new(false),
            permits: tokio::sync::Semaphore::new(0),
        }
    }
}

/// ADR 0028 §11 (decided 2026-10-06): the one instance a stub reports, when it
/// reports instances at all (`None`: the router dispatches to the deployment
/// whole), the stall timeout it sets, and every group stall reported to it.
#[derive(Default)]
struct InstanceStub {
    serving: Option<capyctl_controller::ServingInstance>,
    stall_timeout: std::time::Duration,
    reports: Mutex<Vec<(String, u32, i64)>>,
}

impl StubAuthority {
    fn refuse(what: &str) -> capyctl_controller::LifecycleFault {
        capyctl_controller::LifecycleFault::Blocked(format!("the stub authority cannot {what}"))
    }

    fn row(&self) -> capyctl_store::deployments::DeploymentRow {
        capyctl_store::deployments::DeploymentRow {
            id: self.deployment.clone(),
            name: "stub".into(),
            kind: "vllm".into(),
            route_model_id: Some(self.route.clone()),
            desired_state: capyctl_domain::LifecycleState::Ready,
            observed_state: capyctl_domain::LifecycleState::Ready,
            schema_version: 1,
            current_generation: 1,
        }
    }

    /// A READY deployment `deployment` (its route the same name) served by
    /// exactly one open instance, a group's head when `group` is set, on
    /// `gpus` GPUs of its host (ADR 0035).
    fn with_instance(
        deployment: &str,
        index: u32,
        generation: i64,
        group: bool,
        gpus: u32,
    ) -> Arc<Self> {
        Arc::new(Self {
            deployment: deployment.into(),
            route: deployment.into(),
            runtime: Mutex::new(capyctl_controller::RuntimeEndpoint {
                endpoint: "http://127.0.0.1:9".into(),
                served_model: "served-name".into(),
                engine_key: None,
                incarnation: generation.to_string(),
                engine: Some("vllm".into()),
                deployment_id: deployment.into(),
                revision: 1,
                request_abort: false,
            }),
            leases: Mutex::new(Vec::new()),
            refuse_leases: Mutex::new(None),
            instances: InstanceStub {
                serving: Some(capyctl_controller::ServingInstance {
                    instance_index: index,
                    generation,
                    host_id: None,
                    remote_host: Some("host-a".into()),
                    launch_command_id: None,
                    dispatch_open: true,
                    host_live: true,
                    host_unresponsive: false,
                    engine_exited: false,
                    load: None,
                    group,
                    local_tensor_parallel: gpus,
                }),
                stall_timeout: std::time::Duration::from_secs(120),
                reports: Mutex::new(Vec::new()),
            },
            closes: CloseGate::default(),
            abort: AbortStub::default(),
        })
    }

    /// ADR 0032: deployment `dep-1` (route `public-alias`) served by one
    /// SGLang instance (instance 0, generation 1) at `endpoint`, whose lane
    /// has the per-request abort when `request_abort`, reached through its
    /// host's ingress when `remote`.
    fn sglang(endpoint: &str, remote: bool, request_abort: bool) -> Arc<Self> {
        Arc::new(Self {
            deployment: "dep-1".into(),
            route: "public-alias".into(),
            runtime: Mutex::new(capyctl_controller::RuntimeEndpoint {
                endpoint: endpoint.into(),
                served_model: "served-name".into(),
                engine_key: None,
                incarnation: "1".into(),
                engine: Some("sglang".into()),
                deployment_id: "dep-1".into(),
                revision: 1,
                request_abort,
            }),
            leases: Mutex::new(Vec::new()),
            refuse_leases: Mutex::new(None),
            instances: InstanceStub {
                serving: Some(capyctl_controller::ServingInstance {
                    instance_index: 0,
                    generation: 1,
                    host_id: None,
                    remote_host: remote.then(|| "host-a".into()),
                    launch_command_id: None,
                    dispatch_open: true,
                    host_live: true,
                    host_unresponsive: false,
                    engine_exited: false,
                    load: None,
                    group: false,
                    local_tensor_parallel: 1,
                }),
                stall_timeout: std::time::Duration::from_secs(120),
                reports: Mutex::new(Vec::new()),
            },
            closes: CloseGate::default(),
            abort: AbortStub::default(),
        })
    }

    /// ADR 0032 §3: every abort asked of this authority, in order.
    fn aborts(&self) -> Vec<(String, i64, String)> {
        self.abort.asked.lock().unwrap().clone()
    }

    /// ADR 0032 §4: every lease acknowledged by its own final output.
    fn acks(&self) -> Vec<String> {
        self.abort.acks.lock().unwrap().clone()
    }

    /// ADR 0032 §7: every deployment a restart was asked for.
    fn restarts(&self) -> Vec<String> {
        self.abort.restarts.lock().unwrap().clone()
    }

    /// ADR 0028 §9: a multi-node group's instance `index` at `generation`,
    /// served at its head.
    fn with_group(deployment: &str, index: u32, generation: i64) -> Arc<Self> {
        Self::with_instance(deployment, index, generation, true, 1)
    }

    /// A single-host instance (instance 0, generation 1).
    fn with_single(deployment: &str) -> Arc<Self> {
        Self::with_instance(deployment, 0, 1, false, 1)
    }

    /// ADR 0035: a single-host instance (instance 0, generation 1) on
    /// `gpus` GPUs of its host.
    fn with_gpus(deployment: &str, gpus: u32) -> Arc<Self> {
        Self::with_instance(deployment, 0, 1, false, gpus)
    }

    /// ADR 0028 §11: the group stall timeout this authority sets.
    fn stall_timeout(self: Arc<Self>, timeout: std::time::Duration) -> Arc<Self> {
        let mut stub = Arc::try_unwrap(self)
            .ok()
            .expect("configured before it is shared");
        stub.instances.stall_timeout = timeout;
        Arc::new(stub)
    }

    /// Every group stall reported to this authority, in order.
    fn stall_reports(&self) -> Vec<(String, u32, i64)> {
        self.instances.reports.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl capyctl_controller::LifecyclePort for StubAuthority {
    fn find_deployment_by_route(
        &self,
        route: &str,
    ) -> Result<Option<capyctl_store::deployments::DeploymentRow>, capyctl_controller::LifecycleFault>
    {
        Ok((route == self.route).then(|| self.row()))
    }
    fn list_enabled_routes(
        &self,
    ) -> Result<Vec<capyctl_store::deployments::PublicRoute>, capyctl_controller::LifecycleFault>
    {
        Ok(vec![capyctl_store::deployments::PublicRoute {
            id: self.route.clone(),
            api: capyctl_config::effective::DeploymentApi::Chat,
        }])
    }
    fn deployment_api(
        &self,
        _deployment: &str,
    ) -> Result<capyctl_config::effective::DeploymentApi, capyctl_controller::LifecycleFault> {
        Ok(capyctl_config::effective::DeploymentApi::Chat)
    }
    fn get_deployment(
        &self,
        id: &str,
    ) -> Result<Option<capyctl_store::deployments::DeploymentRow>, capyctl_controller::LifecycleFault>
    {
        Ok((id == self.deployment).then(|| self.row()))
    }
    fn latest_operation(
        &self,
        _deployment_id: &str,
    ) -> Result<Option<capyctl_store::deployments::OperationRow>, capyctl_controller::LifecycleFault>
    {
        Ok(None)
    }
    fn ready_deployments_excluding(
        &self,
        _deployment: &str,
    ) -> Result<Vec<String>, capyctl_controller::LifecycleFault> {
        Ok(Vec::new())
    }
    fn runtime_endpoint(
        &self,
        deployment: &str,
    ) -> Result<Option<capyctl_controller::RuntimeEndpoint>, capyctl_controller::LifecycleFault>
    {
        if deployment != self.deployment {
            return Ok(None);
        }
        Ok(Some(self.runtime.lock().unwrap().clone()))
    }
    async fn open_request_lease(
        &self,
        _deployment: &str,
        _max_per_deployment: usize,
    ) -> Result<Option<capyctl_controller::RequestLease>, capyctl_controller::LeaseRefused> {
        if let Some(refused) = self.refuse_leases.lock().unwrap().clone() {
            return Err(refused);
        }
        let mut leases = self.leases.lock().unwrap();
        let id = format!("lease-{}", leases.len());
        leases.push((id.clone(), None));
        Ok(Some(capyctl_controller::RequestLease::unrecorded(&id)))
    }
    async fn close_request_lease(
        &self,
        lease: capyctl_controller::RequestLease,
        end: capyctl_controller::LeaseEnd,
    ) -> Result<(), capyctl_controller::LeaseRefused> {
        if self.closes.held.load(std::sync::atomic::Ordering::SeqCst) {
            self.closes.permits.acquire().await.unwrap().forget();
        }
        let mut leases = self.leases.lock().unwrap();
        let entry = leases
            .iter_mut()
            .find(|(id, _)| id == lease.id())
            .expect("a lease this authority granted");
        assert!(entry.1.is_none(), "a lease is closed once");
        entry.1 = Some(end);
        Ok(())
    }
    fn clear_suspension(&self, _d: &str) -> Result<(), capyctl_controller::LifecycleFault> {
        Err(Self::refuse("clear a suspension"))
    }
    fn journal(
        &self,
        _host_id: Option<&str>,
        _operation_id: Option<&str>,
        _state: Option<&str>,
        _evidence: &str,
    ) -> Result<(), capyctl_controller::LifecycleFault> {
        Err(Self::refuse("journal"))
    }
    async fn observe_adapter(
        &self,
        _deployment: &str,
    ) -> Result<capyctl_adapters::traits::WorkObservation, capyctl_controller::LifecycleFault> {
        Err(Self::refuse("observe engine work"))
    }
    async fn idle_stop(
        &self,
        _deployment: &str,
    ) -> Result<capyctl_controller::OperationHandle, capyctl_controller::LifecycleFault> {
        Err(Self::refuse("stop a deployment"))
    }
    async fn wait_terminal(
        &self,
        _handle: &capyctl_controller::OperationHandle,
    ) -> Result<capyctl_domain::LifecycleState, capyctl_controller::LifecycleFault> {
        Err(Self::refuse("await an operation"))
    }
    async fn auto_activate(
        &self,
        _deployment: &str,
    ) -> Result<capyctl_controller::OperationHandle, capyctl_controller::LifecycleFault> {
        Err(Self::refuse("activate a deployment"))
    }
    async fn request_transition(
        &self,
        _deployment: &str,
        _action: capyctl_domain::LifecycleAction,
    ) -> Result<capyctl_controller::OperationHandle, capyctl_controller::LifecycleFault> {
        Err(Self::refuse("request a transition"))
    }
    fn serving_instances(
        &self,
        deployment: &str,
    ) -> Result<Option<Vec<capyctl_controller::ServingInstance>>, capyctl_controller::LifecycleFault>
    {
        Ok(self
            .instances
            .serving
            .clone()
            .filter(|_| deployment == self.deployment)
            .map(|instance| vec![instance]))
    }
    fn group_stall_timeout(&self) -> std::time::Duration {
        self.instances.stall_timeout
    }
    async fn report_group_stall(&self, deployment_id: &str, instance: u32, generation: i64) {
        self.instances
            .reports
            .lock()
            .unwrap()
            .push((deployment_id.into(), instance, generation));
    }
    async fn abort_request(&self, deployment_id: &str, generation: i64, request_id: &str) -> bool {
        self.abort.asked.lock().unwrap().push((
            deployment_id.into(),
            generation,
            request_id.into(),
        ));
        let sent = self.abort.sends.load(std::sync::atomic::Ordering::SeqCst);
        if sent {
            self.abort.aborted.add_permits(1);
        }
        sent
    }
    async fn acknowledge_cancelled_request(
        &self,
        lease_id: &str,
    ) -> Result<bool, capyctl_controller::LeaseRefused> {
        let leases = self.leases.lock().unwrap();
        let (_, end) = leases
            .iter()
            .find(|(id, _)| id == lease_id)
            .expect("a lease this authority granted");
        assert!(
            matches!(end, Some(capyctl_controller::LeaseEnd::Cancelling { .. })),
            "only a cancelling lease is acknowledged: {end:?}"
        );
        self.abort.acks.lock().unwrap().push(lease_id.into());
        Ok(true)
    }
    async fn restart_for_held_requests(
        &self,
        deployment_id: &str,
    ) -> Result<capyctl_controller::HeldRestart, capyctl_controller::LifecycleFault> {
        self.abort
            .restarts
            .lock()
            .unwrap()
            .push(deployment_id.into());
        let answer = *self.abort.restart.lock().unwrap();
        if answer == capyctl_controller::HeldRestart::Restarting {
            // The restart's verified exit settles the held requests, so the
            // bound has room once it is done.
            *self.refuse_leases.lock().unwrap() = None;
        }
        Ok(answer)
    }
    async fn activate_for_request(
        &self,
        deployment: &str,
    ) -> Result<(), capyctl_controller::LifecycleFault> {
        assert_eq!(deployment, self.deployment);
        self.abort
            .activations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .abort
            .activation_held
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.abort
                .activation_permits
                .acquire()
                .await
                .unwrap()
                .forget();
        }
        Ok(())
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

async fn chat_once(
    router: &axum::Router,
    alias: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
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
        runtime: Mutex::new(capyctl_controller::RuntimeEndpoint {
            endpoint: endpoint.clone(),
            served_model: "served-name".into(),
            engine_key: Some("k3y".into()),
            incarnation: "1".into(),
            engine: Some("vllm".into()),
            deployment_id: "dep-1".into(),
            revision: 1,
            request_abort: false,
        }),
        leases: Mutex::new(Vec::new()),
        refuse_leases: Mutex::new(None),
        instances: InstanceStub::default(),
        closes: CloseGate::default(),
        abort: AbortStub::default(),
    });
    let port = authority.clone() as Arc<dyn capyctl_controller::LifecyclePort>;
    let router = capyctl_router::serve_router(RouterDeps {
        controller: port.clone(),
        forwards: Arc::new(capyctl_router::forwarders::LiveForwarders::new(port)),
        limits: QueueLimits {
            max_requests_per_deployment: 4,
            max_buffered_bytes_total: 64 * 1024,
        },
        api_key: None,
        inflight: Arc::new(capyctl_router::admission::InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    });

    let (status, body) = chat_once(&router, "public-alias").await;
    assert_eq!(
        status, 200,
        "the first launch's key reaches its engine: {body}"
    );
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
    stub_router_on(endpoint, Some("vllm"))
}

/// As [`stub_router`], with the engine family the launch's revision froze.
fn stub_router_on(endpoint: &str, engine: Option<&str>) -> (Arc<StubAuthority>, RouterDeps) {
    stub_router_serving(endpoint, engine, "dep-1", 1)
}

/// As [`stub_router_on`], serving `deployment` from a launch that runs
/// `revision` (ADR 0033 §5: the namespace of the effective salt).
fn stub_router_serving(
    endpoint: &str,
    engine: Option<&str>,
    deployment: &str,
    revision: u64,
) -> (Arc<StubAuthority>, RouterDeps) {
    let authority = Arc::new(StubAuthority {
        deployment: deployment.into(),
        route: "public-alias".into(),
        runtime: Mutex::new(capyctl_controller::RuntimeEndpoint {
            endpoint: endpoint.into(),
            served_model: "served-name".into(),
            engine_key: None,
            incarnation: "1".into(),
            engine: engine.map(str::to_owned),
            deployment_id: deployment.into(),
            revision,
            request_abort: false,
        }),
        leases: Mutex::new(Vec::new()),
        refuse_leases: Mutex::new(None),
        instances: InstanceStub::default(),
        closes: CloseGate::default(),
        abort: AbortStub::default(),
    });
    let port = authority.clone() as Arc<dyn capyctl_controller::LifecyclePort>;
    let deps = RouterDeps {
        controller: port.clone(),
        forwards: Arc::new(capyctl_router::forwarders::LiveForwarders::new(port)),
        limits: QueueLimits {
            max_requests_per_deployment: 4,
            max_buffered_bytes_total: 64 * 1024,
        },
        api_key: None,
        inflight: Arc::new(capyctl_router::admission::InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
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

fn ends(authority: &StubAuthority) -> Vec<Option<capyctl_controller::LeaseEnd>> {
    authority
        .leases
        .lock()
        .unwrap()
        .iter()
        .map(|(_, end)| *end)
        .collect()
}

// T17 T19 (SPEC §10, owner decision 2026-09-22): every dispatch holds a durable
// lease from before the engine sees it; completion closes it.
#[tokio::test]
async fn a_completed_dispatch_opens_and_closes_one_lease() {
    let (endpoint, hits) =
        scripted_engine(200, "text/event-stream", engine_sse("served-name")).await;
    let (authority, deps) = stub_router(&endpoint);
    let router = capyctl_router::serve_router(deps.clone());
    let (status, body) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        ends(&authority),
        vec![Some(capyctl_controller::LeaseEnd::Completed)]
    );
    let (status, text) = chat_stream(&router, "public-alias").await;
    assert_eq!(status, 200);
    assert!(text.contains("[DONE]"), "{text}");
    assert_eq!(
        ends(&authority),
        vec![Some(capyctl_controller::LeaseEnd::Completed); 2],
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
    let router = capyctl_router::serve_router(deps.clone());
    let (status, body) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["code"], "shutting_down");
    assert_eq!(body["retryable"], true);
    assert_eq!(
        ends(&authority),
        vec![Some(capyctl_controller::LeaseEnd::NotAccepted)]
    );
    assert_eq!(deps.inflight.current("dep-1"), 0, "nothing was accepted");
    let (status, text) = chat_stream(&router, "public-alias").await;
    assert_eq!(status, 200);
    assert!(
        text.contains("shutting_down") && !text.contains("[DONE]"),
        "{text}"
    );
    assert_eq!(
        ends(&authority)[1],
        Some(capyctl_controller::LeaseEnd::NotAccepted)
    );
}

// T17 T19 (SPEC §10, found live 2026-09-24): an engine rejection of an
// invalid request is its complete answer. The router relays it with the
// engine's status and message as `engine_rejected`, not retryable, never a
// 500, and closes the lease as completed: rejected requests must not pile up
// uncertain leases until the deployment's outstanding bound refuses everyone.
#[tokio::test]
async fn an_engine_rejection_is_relayed_and_closes_the_lease() {
    let rejection = serde_json::json!({"error":{"message":"This model's maximum context length is 16384 tokens.","type":"BadRequestError","code":400}});
    let (endpoint, _) = scripted_engine(400, "application/json", rejection.to_string()).await;
    let (authority, deps) = stub_router(&endpoint);
    let router = capyctl_router::serve_router(deps.clone());
    let bound = deps.limits.max_requests_per_deployment;
    for _ in 0..bound + 2 {
        let (status, body) = chat_once(&router, "public-alias").await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["code"], "engine_rejected");
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("maximum context length"),
            "{body}"
        );
    }
    let (status, text) = chat_stream(&router, "public-alias").await;
    assert_eq!(status, 200);
    assert!(
        text.contains("engine_rejected") && text.contains("maximum context length"),
        "{text}"
    );
    assert!(
        text.contains("\"retryable\":false") && !text.contains("[DONE]"),
        "{text}"
    );
    assert_eq!(
        ends(&authority),
        vec![Some(capyctl_controller::LeaseEnd::Completed); bound + 3]
    );
    assert_eq!(deps.inflight.current("dep-1"), 0);
}

// T17 T19 (SPEC §10): an engine failure after the request may have been
// accepted keeps the durable lease charged as uncertain. The per-process slot is
// released, so uncertain ends never shrink the deployment's in-flight bound for
// the life of the router: more requests than the bound still get through.
#[tokio::test]
async fn an_unverified_failure_keeps_the_lease_charged() {
    let (endpoint, _) = scripted_engine(503, "application/json", "{}".into()).await;
    let (authority, deps) = stub_router(&endpoint);
    let router = capyctl_router::serve_router(deps.clone());
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
        vec![Some(capyctl_controller::LeaseEnd::Uncertain); bound + 3]
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while deps.inflight.current("dep-1") != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the in-memory slots are released");
}

// T17 T19 (SPEC §10, amended 2026-10-17, found live): an engine's complete
// error answer (llama.cpp v0.7.0's 500 for an input over its batch size) is
// relayed as a 502 `engine_error` with the engine's message, not retryable,
// collected and streamed alike, and never sent to another instance. The
// lease is cancelling, not uncertain, so the engine's quiescence settles it:
// amended 2026-10-21, the complete answer is the engine's answer, so the
// cancellation counts as answered.
#[tokio::test]
async fn an_engine_error_answer_is_relayed_and_leaves_the_lease_cancelling() {
    let error = serde_json::json!({"error":{"code":500,
        "message":"input (8195 tokens) is too large to process. increase the physical batch size",
        "type":"server_error"}});
    let (endpoint, hits) = scripted_engine(500, "application/json", error.to_string()).await;
    let (authority, deps) = stub_router(&endpoint);
    let router = capyctl_router::serve_router(deps.clone());
    let bound = deps.limits.max_requests_per_deployment;
    for _ in 0..bound + 2 {
        let (status, body) = chat_once(&router, "public-alias").await;
        assert_eq!(status, 502, "{body}");
        assert_eq!(body["code"], "engine_error");
        assert_eq!(body["retryable"], false);
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("too large to process"),
            "{body}"
        );
    }
    let (status, text) = chat_stream(&router, "public-alias").await;
    assert_eq!(status, 200);
    assert!(
        text.contains("engine_error") && text.contains("too large to process"),
        "{text}"
    );
    assert!(
        text.contains("\"retryable\":false") && !text.contains("[DONE]"),
        "{text}"
    );
    assert_eq!(
        ends(&authority),
        vec![Some(capyctl_controller::LeaseEnd::Cancelling { answered: true }); bound + 3]
    );
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), bound + 3);
    assert_eq!(deps.inflight.current("dep-1"), 0);
}

// T18: when the ledger refuses because the gate closed (a host draining, a
// readiness re-proof), nothing reaches the engine and the client gets a
// retryable 503, not a 500.
#[tokio::test]
async fn a_closed_gate_refuses_before_the_engine_with_a_retryable_503() {
    let (endpoint, hits) =
        scripted_engine(200, "text/event-stream", engine_sse("served-name")).await;
    let (authority, deps) = stub_router(&endpoint);
    *authority.refuse_leases.lock().unwrap() = Some(capyctl_controller::LeaseRefused::Closed);
    let router = capyctl_router::serve_router(deps.clone());
    let (status, body) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["retryable"], true);
    let (status, _) = chat_stream(&router, "public-alias").await;
    assert_eq!(status, 503);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(deps.inflight.current("dep-1"), 0);
    *authority.refuse_leases.lock().unwrap() = Some(capyctl_controller::LeaseRefused::Full);
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
    assert!(response
        .headers()
        .contains_key(axum::http::header::RETRY_AFTER));
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
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn chat_body(alias: &str, extra: serde_json::Value) -> Vec<u8> {
    let mut body =
        serde_json::json!({"model": alias, "messages": [{"role":"user","content":"hi"}]});
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
    let router = capyctl_router::serve_router(deps.clone());
    for (body, code) in [
        (b"{not json".to_vec(), "invalid_request"),
        (
            chat_body("public-alias", serde_json::json!({"rid": "x"})),
            "invalid_request",
        ),
        (
            chat_body("public-alias", serde_json::json!({"n": 2})),
            "unsupported_parameter",
        ),
        (
            chat_body("public-alias", serde_json::json!({"functions": []})),
            "unsupported_parameter",
        ),
        (
            chat_body("public-alias", serde_json::json!({"n": 3, "stream": true})),
            "unsupported_parameter",
        ),
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
    assert_eq!(
        answer["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "lookup"
    );
    assert_eq!(
        ends(&authority),
        vec![Some(capyctl_controller::LeaseEnd::Completed)]
    );
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
    let router = capyctl_router::serve_router(deps.clone());
    let large = serde_json::json!({"messages": [{"role":"user","content":"x".repeat(3 << 20)}]});
    let mut body = large.clone();
    body["model"] = serde_json::json!("public-alias");
    let (status, answer) = post_raw(&router, body.to_string().into_bytes(), None).await;
    assert_eq!(
        status, 200,
        "a 3 MiB body under a 4 MiB bound is served: {answer}"
    );
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
                seen.lock().unwrap().push(
                    body["messages"][0]["content"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                );
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
    let router = capyctl_router::serve_router(deps.clone());
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
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            deadline: std::time::Duration::from_millis(300),
            ..Default::default()
        });
    let started = std::time::Instant::now();
    let (status, answer) = send("r5").await.unwrap();
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(250),
        "waited first"
    );
    assert_eq!(status, 429, "{answer}");
    assert_eq!(answer["retryable"], true);
    assert_eq!(deps.inflight.slot_waiters("dep-1"), 0, "it left the queue");
    assert_eq!(deps.inflight.waiting.totals(), (0, 0));
    gate.add_permits(1);
    assert_eq!(holder.await.unwrap().0, 200);
}

// T19, SPEC §10 (owner decision 2026-10-08): with `max_pending_per_deployment`
// at 0 no request waits. One beyond the in-flight bound is refused at once with
// the bounded queue's answer, 429 `queue_full` and `Retry-After`, never sent to
// the engine, and the request holding the slot is untouched.
#[tokio::test]
async fn with_no_waiting_a_request_beyond_the_in_flight_bound_is_refused_at_once() {
    let (endpoint, gate, order) = gated_engine().await;
    let (_authority, mut deps) = stub_router(&endpoint);
    deps.limits.max_requests_per_deployment = 1;
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            max_pending_per_deployment: 0,
            ..Default::default()
        });
    let router = capyctl_router::serve_router(deps.clone());
    let holder = {
        let router = router.clone();
        tokio::spawn(async move {
            let body = serde_json::json!({"model":"public-alias",
                "messages":[{"role":"user","content":"r1"}]});
            post_raw(&router, body.to_string().into_bytes(), None).await
        })
    };
    until("r1 at the engine", || order.lock().unwrap().len() == 1).await;
    let started = std::time::Instant::now();
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"model": "public-alias",
                        "messages": [{"role":"user","content":"r2"}]})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    // The default request deadline is 600 s: an answer this fast did not wait.
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(response.status(), 429);
    assert_eq!(
        response.headers()[axum::http::header::RETRY_AFTER],
        "1",
        "the existing retry hint"
    );
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["code"], "queue_full", "{body}");
    assert_eq!(body["retryable"], true);
    assert_eq!(deps.inflight.slot_waiters("dep-1"), 0);
    assert_eq!(deps.inflight.waiting.totals(), (0, 0));
    assert_eq!(*order.lock().unwrap(), vec!["r1"], "never sent");
    gate.add_permits(1);
    assert_eq!(holder.await.unwrap().0, 200);
}

// T19 T17 (SPEC §10): a non-streaming request is bounded like a stream — its
// first backend event by the request deadline, later ones by the idle bound —
// not by a fixed 300 s / 60 s cap. A backend that stalls after its first event
// is cut at the idle bound; the cut closed the engine connection, so its lease
// is cancelling until the engine reports quiescence (found live 2026-10-02).
// Amended 2026-10-21: the engine produced output first, so the cancellation
// counts as answered.
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
                (
                    [("content-type", "text/event-stream")],
                    axum::body::Body::from_stream(stream),
                )
            }
        }),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    let (authority, deps) = stub_router(&endpoint);
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            stream_idle: std::time::Duration::from_millis(300),
            ..Default::default()
        });
    let router = capyctl_router::serve_router(deps.clone());
    let (status, answer) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        chat_once(&router, "public-alias"),
    )
    .await
    .expect("cut at the idle bound, not a fixed 60 s read timeout");
    assert_eq!(status, 500, "{answer}");
    assert_eq!(answer["code"], "engine_error");
    assert_eq!(
        ends(&authority),
        vec![Some(capyctl_controller::LeaseEnd::Cancelling {
            answered: true
        })]
    );
}

// T17 T38 (SPEC §10, amended 2026-10-21): a collected request cut at its
// deadline before the engine answered anything (headers and a chunk that only
// opens the reply are not an answer) is cancelling as unanswered, so no idle
// reading settles it; only the instance's verified stop does.
#[tokio::test]
async fn a_collected_request_cut_before_its_answer_is_unanswered() {
    let opening = format!(
        "data: {}\n\n",
        serde_json::json!({"id":"chat-1","object":"chat.completion.chunk","created":1,
            "model":"served-name","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]})
    );
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let opening = opening.clone();
            async move {
                let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(opening) })
                    .chain(futures::stream::pending());
                (
                    [("content-type", "text/event-stream")],
                    axum::body::Body::from_stream(stream),
                )
            }
        }),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    let (authority, deps) = stub_router(&endpoint);
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            deadline: std::time::Duration::from_millis(300),
            ..Default::default()
        });
    let router = capyctl_router::serve_router(deps.clone());
    let (status, answer) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        chat_once(&router, "public-alias"),
    )
    .await
    .expect("cut at the request deadline");
    assert_eq!(status, 500, "{answer}");
    assert_eq!(
        ends(&authority),
        vec![Some(capyctl_controller::LeaseEnd::Cancelling {
            answered: false
        })]
    );
}

/// An engine that answers every chat request with an SSE stream: `opening`
/// first, then, when `content`, a content chunk every 20 ms forever.
async fn endless_engine(content: bool) -> String {
    let chunk = |delta: serde_json::Value| {
        format!(
            "data: {}\n\n",
            serde_json::json!({"id":"chat-1","object":"chat.completion.chunk","created":1,
                "model":"served-name","choices":[{"index":0,"delta":delta,"finish_reason":null}]})
        )
    };
    let opening = chunk(serde_json::json!({"role":"assistant","content":""}));
    let token = chunk(serde_json::json!({"content":"he"}));
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let (opening, token) = (opening.clone(), token.clone());
            async move {
                let tokens = futures::stream::unfold(token, |token| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    Some((Ok::<_, std::io::Error>(token.clone()), token))
                });
                let opening = futures::stream::once(async move { Ok(opening) });
                let body = if content {
                    axum::body::Body::from_stream(opening.chain(tokens))
                } else {
                    axum::body::Body::from_stream(opening.chain(futures::stream::pending()))
                };
                ([("content-type", "text/event-stream")], body)
            }
        }),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    endpoint
}

// T17 T38 (SPEC §10, amended 2026-10-21): a stream the router cuts at its
// request deadline before the engine produced any output (only response
// headers and a chunk that only opens the reply) is cancelling as
// unanswered: no idle reading settles it, only the verified stop.
#[tokio::test]
async fn a_stream_cut_before_any_engine_output_is_unanswered() {
    let endpoint = endless_engine(false).await;
    let (authority, deps) = stub_router(&endpoint);
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            deadline: std::time::Duration::from_millis(300),
            ..Default::default()
        });
    let router = capyctl_router::serve_router(deps.clone());
    let (status, text) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        chat_stream(&router, "public-alias"),
    )
    .await
    .expect("cut at the request deadline");
    assert_eq!(status, 200);
    assert!(!text.contains("[DONE]"), "{text}");
    eventually(|| {
        ends(&authority)
            == vec![Some(capyctl_controller::LeaseEnd::Cancelling {
                answered: false,
            })]
    })
    .await;
}

// T17 (SPEC §10, amended 2026-10-21): a client that hangs up after the
// engine's first output leaves a cancellation the engine answered, which its
// idle reading settles as before.
#[tokio::test]
async fn a_hang_up_after_an_engine_chunk_is_answered() {
    let endpoint = endless_engine(true).await;
    let (authority, deps) = stub_router(&endpoint);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = capyctl_router::serve_router(deps.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{address}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "public-alias", "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut body = response.bytes_stream();
    let mut seen = String::new();
    while !seen.contains("\"he\"") {
        let bytes = body.next().await.unwrap().unwrap();
        seen.push_str(&String::from_utf8_lossy(&bytes));
    }
    // The client hangs up mid-stream.
    drop(body);
    drop(client);
    eventually(|| {
        ends(&authority)
            == vec![Some(capyctl_controller::LeaseEnd::Cancelling {
                answered: true,
            })]
    })
    .await;
}

// T17 T38 (SPEC §10, amended 2026-10-21, owner decision for 0.1.7): a client
// that disconnects in the middle of a collected chat request cancels its
// forward, so the engine sees its connection close within a bound, as for a
// stream: before the engine's response head, and after a head and a chunk
// that only opens the reply. The engine never answered it, so it is held as
// cancelling until the instance's verified stop, and its in-flight slot is
// released.
#[tokio::test]
async fn a_hang_up_mid_collected_request_closes_the_engine_connection() {
    for opening in [false, true] {
        let engine = if opening {
            capyctl_testkit::HeldEngine::opening().await
        } else {
            capyctl_testkit::HeldEngine::start().await
        };
        let (authority, deps) = stub_router(&engine.endpoint());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = capyctl_router::serve_router(deps.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("http://{address}/v1/chat/completions"))
                .json(&serde_json::json!({
                    "model": "public-alias",
                    "messages": [{"role": "user", "content": "hi"}],
                }))
                .send()
                .await
        });
        let bound = std::time::Duration::from_secs(5);
        assert!(
            engine.arrived_within(1, bound).await,
            "never reached the engine"
        );
        // Long enough for the router to read the opening chunk, if any.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(engine.closed(), 0, "opening: {opening}");
        // The client hangs up mid-request, long before its deadline.
        client.abort();
        let _ = client.await;
        assert!(
            engine.closed_within(1, bound).await,
            "the engine connection stayed open after the hang-up (opening: {opening})"
        );
        eventually(|| {
            ends(&authority)
                == vec![Some(capyctl_controller::LeaseEnd::Cancelling {
                    answered: false,
                })]
        })
        .await;
        eventually(|| deps.inflight.current("dep-1") == 0).await;
    }
}

// T17 T38 (SPEC §10, found live 2026-10-22): a client that hangs up a stream
// before the engine's first token cancels its forward at once, as one that
// hangs up after it: before the engine's response head, and after a head and
// a chunk that only opens the reply. Live on SGLang, the router noticed only
// when the engine produced its first chunk for the queued request, which the
// engine started only once the request ahead of it ended. The engine never
// answered it, so it is held as cancelling until the instance's verified
// stop, and its in-flight slot is released.
#[tokio::test]
async fn a_stream_hung_up_before_the_first_token_closes_the_engine_connection() {
    for opening in [false, true] {
        let engine = if opening {
            capyctl_testkit::HeldEngine::opening().await
        } else {
            capyctl_testkit::HeldEngine::start().await
        };
        let (authority, deps) = stub_router(&engine.endpoint());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = capyctl_router::serve_router(deps.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (headed, head) = tokio::sync::oneshot::channel();
        let client = tokio::spawn(async move {
            let response = reqwest::Client::new()
                .post(format!("http://{address}/v1/chat/completions"))
                .json(&serde_json::json!({
                    "model": "public-alias", "stream": true,
                    "messages": [{"role": "user", "content": "hi"}],
                }))
                .send()
                .await
                .unwrap();
            let _ = headed.send(response.status());
            // Held open, reading whatever comes, until the client is aborted.
            let mut body = response.bytes_stream();
            while body.next().await.is_some() {}
            std::future::pending::<()>().await;
        });
        let bound = std::time::Duration::from_secs(5);
        assert!(
            engine.arrived_within(1, bound).await,
            "never reached the engine"
        );
        assert_eq!(head.await.unwrap(), 200);
        // Long enough for the router to read the opening chunk, if any.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(engine.closed(), 0, "opening: {opening}");
        // The client hangs up before any token, long before its deadline.
        client.abort();
        let _ = client.await;
        // Well inside the 15 s between SSE keep-alive comments.
        assert!(
            engine
                .closed_within(1, std::time::Duration::from_secs(2))
                .await,
            "the engine connection stayed open after the hang-up (opening: {opening})"
        );
        eventually(|| {
            ends(&authority)
                == vec![Some(capyctl_controller::LeaseEnd::Cancelling {
                    answered: false,
                })]
        })
        .await;
        eventually(|| deps.inflight.current("dep-1") == 0).await;
    }
}

// T17 T38 (SPEC §10, amended 2026-10-02 and 2026-10-21): a collected request
// cut at its deadline drops its forward too, so the engine sees its
// connection close.
#[tokio::test]
async fn a_collected_request_cut_at_its_deadline_closes_the_engine_connection() {
    let engine = capyctl_testkit::HeldEngine::start().await;
    let (authority, deps) = stub_router(&engine.endpoint());
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            deadline: std::time::Duration::from_millis(300),
            ..Default::default()
        });
    let router = capyctl_router::serve_router(deps.clone());
    let (status, answer) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        chat_once(&router, "public-alias"),
    )
    .await
    .expect("cut at the request deadline");
    assert_eq!(status, 500, "{answer}");
    assert!(
        engine
            .closed_within(1, std::time::Duration::from_secs(5))
            .await,
        "the engine connection stayed open after the cut"
    );
    assert_eq!(
        ends(&authority),
        vec![Some(capyctl_controller::LeaseEnd::Cancelling {
            answered: false
        })]
    );
}

// T15 (SPEC §10 step 2): a detached activation whose task panics answers its
// waiters and frees its slot, so the next request starts a fresh activation
// instead of joining a claim nobody completes.
#[tokio::test]
async fn a_panicking_activation_answers_its_waiters_and_frees_its_slot() {
    let join: Arc<capyctl_router::WakeJoin<String>> = Arc::new(capyctl_router::WakeJoin::new());
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

// T10 T18 (SPEC §10, owner decision 2026-09-25): a request for a deployment
// an operator stopped is 409 `deployment_stopped`, naming the deployment and
// how to start it, never 429 `insufficient_resources`.
#[tokio::test]
async fn a_request_for_an_operator_stopped_deployment_is_deployment_stopped() {
    let (router, _s, controller, _f, _deps) = app().await;
    let id = deploy_ready(&router, &controller, "stopped-chat-m").await;
    let op = controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Stop)
        .await
        .unwrap();
    controller.wait_terminal(&op).await.unwrap();
    let res = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("Authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"model": "stopped-chat-m", "messages": [{"role": "user", "content": "hi"}]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 409);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["code"], "deployment_stopped", "{v}");
    let message = v["message"].as_str().unwrap();
    assert!(message.contains("stopped by an operator"), "{message}");
    assert!(
        message.contains(&format!("capyctl start deployment {id}")),
        "{message}"
    );
}

// T37 (design §9, review focus 4): a keyed router, whatever address it is
// bound on, refuses a missing or wrong key on every inference route.
#[tokio::test]
async fn every_inference_route_needs_the_key() {
    let (router, _s, _c, _f, _deps) = app().await;
    for (method, path) in [("GET", "/v1/models"), ("POST", "/v1/chat/completions")] {
        let call = |header: Option<&str>| {
            let mut request = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json");
            if let Some(value) = header {
                request = request.header("Authorization", value);
            }
            let body = if method == "POST" {
                axum::body::Body::from(
                    serde_json::json!({"model": "nothing", "messages": []}).to_string(),
                )
            } else {
                axum::body::Body::empty()
            };
            router.clone().oneshot(request.body(body).unwrap())
        };
        for header in [
            None,
            Some("Bearer wrong"),
            Some("Basic test-key"),
            Some("Bearer capyctl-local"),
        ] {
            let status = call(header).await.unwrap().status();
            assert_eq!(status, 401, "{method} {path} {header:?}");
        }
        let status = call(Some("Bearer test-key")).await.unwrap().status();
        assert_ne!(status, 401, "{method} {path}");
    }
}

/// ADR 0028 §11: a router over `authority`, its inference key off.
struct TestRouter {
    router: axum::Router,
    authority: Arc<StubAuthority>,
}

fn router_with(authority: &Arc<StubAuthority>) -> TestRouter {
    let port = authority.clone() as Arc<dyn capyctl_controller::LifecyclePort>;
    TestRouter {
        router: capyctl_router::serve_router(RouterDeps {
            controller: port.clone(),
            forwards: Arc::new(capyctl_router::forwarders::LiveForwarders::new(port)),
            limits: QueueLimits {
                max_requests_per_deployment: 4,
                max_buffered_bytes_total: 64 * 1024,
            },
            api_key: None,
            inflight: Arc::new(capyctl_router::admission::InFlight::default()),
            activation_join: Arc::new(capyctl_router::WakeJoin::new()),
        }),
        authority: authority.clone(),
    }
}

impl TestRouter {
    /// The engine accepts every request and never answers it: no first token.
    fn with_upstream_silent(self) -> Self {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let address = socket.local_addr().unwrap();
        let socket = tokio::net::TcpListener::from_std(socket).unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((connection, _)) = socket.accept().await {
                held.push(connection);
            }
        });
        self.authority.runtime.lock().unwrap().endpoint = format!("http://{address}");
        self
    }

    /// The engine answers at once with a complete reply.
    async fn with_upstream_answering(self) -> Self {
        let (endpoint, _) =
            scripted_engine(200, "text/event-stream", engine_sse("served-name")).await;
        self.authority.runtime.lock().unwrap().endpoint = endpoint;
        self
    }

    fn send(&self, alias: &str, stream: bool) -> tokio::task::JoinHandle<axum::http::StatusCode> {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({
                    "model": alias, "stream": stream,
                    "messages": [{"role":"user","content":"hi"}],
                })
                .to_string(),
            ))
            .unwrap();
        let router = self.router.clone();
        tokio::spawn(async move {
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let _ = axum::body::to_bytes(response.into_body(), 64 * 1024).await;
            status
        })
    }

    /// A chat request for `alias`, in flight until it ends.
    fn send_chat(&self, alias: &str) -> tokio::task::JoinHandle<axum::http::StatusCode> {
        self.send(alias, false)
    }

    /// A streamed chat request for `alias`, in flight until it ends.
    fn send_chat_stream(&self, alias: &str) -> tokio::task::JoinHandle<axum::http::StatusCode> {
        self.send(alias, true)
    }
}

// T31 (decided 2026-10-06): a first-token timeout in the router reaches the controller once, naming the generation.
#[tokio::test]
async fn first_token_timeout_reports_a_group_stall() {
    use std::time::Duration;
    let authority = StubAuthority::with_group("g", 0, 1).stall_timeout(Duration::from_millis(50));
    let router = router_with(&authority).with_upstream_silent();
    let _pending = router.send_chat("g");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(authority.stall_reports(), vec![("g".to_string(), 0, 1)]);
    let single = StubAuthority::with_single("s").stall_timeout(Duration::from_millis(50));
    let router = router_with(&single).with_upstream_silent();
    let _pending = router.send_chat("s");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(single.stall_reports().is_empty());
}

// T46 T31 (ADR 0035 §7): a request to an instance on several GPUs of one host
// that gets no first token within the stall timeout is reported once, as a
// group's is; an instance on one GPU never is.
#[tokio::test]
async fn first_token_timeout_reports_a_stall_on_several_gpus() {
    use std::time::Duration;
    let authority = StubAuthority::with_gpus("tp", 2).stall_timeout(Duration::from_millis(50));
    let router = router_with(&authority).with_upstream_silent();
    let _pending = router.send_chat("tp");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(authority.stall_reports(), vec![("tp".to_string(), 0, 1)]);
    let one = StubAuthority::with_gpus("one", 1).stall_timeout(Duration::from_millis(50));
    let router = router_with(&one).with_upstream_silent();
    let _pending = router.send_chat("one");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(one.stall_reports().is_empty(), "N = 1 is never watched");
}

// T31 T39 (ADR 0028 §11): the watch observes a group request without holding
// it: a streamed group request that gets no first token is reported once, and
// a group request answered in time, streamed or not, is never reported and is
// relayed whole.
#[tokio::test]
async fn the_first_token_watch_reports_a_stalled_stream_once_and_never_an_answered_request() {
    use std::time::Duration;
    let authority = StubAuthority::with_group("g", 2, 7).stall_timeout(Duration::from_millis(50));
    let router = router_with(&authority).with_upstream_silent();
    let _pending = router.send_chat_stream("g");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(authority.stall_reports(), vec![("g".to_string(), 2, 7)]);
    let answered = StubAuthority::with_group("g", 0, 1).stall_timeout(Duration::from_millis(50));
    let router = router_with(&answered).with_upstream_answering().await;
    let (status, body) = chat_once(&router.router, "g").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "hello");
    let (status, text) = chat_stream(&router.router, "g").await;
    assert_eq!(status, 200);
    assert!(text.contains("hello") && text.contains("[DONE]"), "{text}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(answered.stall_reports().is_empty());
    assert_eq!(
        ends(&answered),
        vec![Some(capyctl_controller::LeaseEnd::Completed); 2]
    );
}

/// An engine answering every chat request with `engine_sse` and recording the
/// body it received.
async fn recording_engine() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            recorded.lock().unwrap().push(body);
            async move {
                (
                    [("content-type", "text/event-stream")],
                    engine_sse("served-name"),
                )
            }
        }),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (base, seen)
}

// T45 T37 (ADR 0033 §5, SPEC §10): vLLM and SGLang receive, in place of the
// caller's `cache_salt`, the effective salt of the serving launch's deployment
// ID, the revision it runs and the caller's salt, streaming and not. Equal
// inputs share it; another deployment, revision or salt never does.
#[tokio::test]
async fn cache_salt_reaches_vllm_and_sglang_as_the_effective_salt() {
    use capyctl_adapters::forward::effective_cache_salt;
    let salts = ["tenant-a/\u{fc}/0001", "tenant-b"];
    let mut distinct = std::collections::HashSet::new();
    for engine in ["vllm", "sglang"] {
        for (deployment, revision) in [("dep-1", 3), ("dep-2", 3), ("dep-1", 4)] {
            let (endpoint, seen) = recording_engine().await;
            let (authority, deps) =
                stub_router_serving(&endpoint, Some(engine), deployment, revision);
            let router = capyctl_router::serve_router(deps);
            for salt in salts {
                for stream in [false, true] {
                    let body = chat_body(
                        "public-alias",
                        serde_json::json!({"cache_salt": salt, "stream": stream}),
                    );
                    let (status, answer) = post_raw(&router, body, None).await;
                    assert_eq!(status, 200, "{engine} stream={stream}: {answer}");
                }
            }
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 4, "{engine} {deployment} {revision}");
            for (body, salt) in seen.iter().zip(salts.iter().flat_map(|s| [s, s])) {
                let expected = effective_cache_salt(deployment, revision, salt);
                assert_eq!(body["cache_salt"], expected.as_str(), "{engine}: {body}");
                assert_ne!(body["cache_salt"], *salt, "{engine}: {body}");
                if (deployment, revision, *salt) == ("dep-1", 3, "tenant-a/\u{fc}/0001") {
                    assert_eq!(
                        expected,
                        "18d1486516c53f38bd0e5e4b5760aefb4633a2f3b31183ffd69c70457382c645"
                    );
                }
            }
            for salt in salts {
                distinct.insert(effective_cache_salt(deployment, revision, salt));
            }
            assert_eq!(
                ends(&authority),
                vec![Some(capyctl_controller::LeaseEnd::Completed); 4],
                "{engine}"
            );
        }
    }
    // Three launches times two salts, the same for both engine families.
    assert_eq!(distinct.len(), 6);
}

// T45 T37 (ADR 0033 §5): no salt stays no salt; a request without
// `cache_salt` reaches vLLM and SGLang without the field, streaming and not.
#[tokio::test]
async fn a_request_without_cache_salt_reaches_the_engine_without_one() {
    for engine in ["vllm", "sglang"] {
        let (endpoint, seen) = recording_engine().await;
        let (_authority, deps) = stub_router_serving(&endpoint, Some(engine), "dep-1", 3);
        let router = capyctl_router::serve_router(deps);
        for stream in [false, true] {
            let body = chat_body("public-alias", serde_json::json!({"stream": stream}));
            let (status, answer) = post_raw(&router, body, None).await;
            assert_eq!(status, 200, "{engine} stream={stream}: {answer}");
        }
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{engine}");
        for body in &seen {
            assert!(body.get("cache_salt").is_none(), "{engine}: {body}");
        }
    }
}

// SPEC §10 (`cache_salt_unsupported`): TensorFold ignores `cache_salt`, and an
// engine whose family is not recorded cannot be shown to honour it, so a
// request carrying one is refused before anything is sent; isolation is never
// silently absent. The same request without the field is served.
#[tokio::test]
async fn cache_salt_is_refused_for_an_engine_that_would_ignore_it() {
    for engine in [Some("tensorfold"), None] {
        let (endpoint, seen) = recording_engine().await;
        let (authority, deps) = stub_router_on(&endpoint, engine);
        let router = capyctl_router::serve_router(deps.clone());
        let body = chat_body(
            "public-alias",
            serde_json::json!({"cache_salt": "tenant-a"}),
        );
        let (status, answer) = post_raw(&router, body, None).await;
        assert_eq!(status, 400, "{engine:?}: {answer}");
        assert_eq!(
            answer["code"], "cache_salt_unsupported",
            "{engine:?}: {answer}"
        );
        let streamed = chat_body(
            "public-alias",
            serde_json::json!({"cache_salt": "tenant-a", "stream": true}),
        );
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(streamed))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("cache_salt_unsupported"),
            "{engine:?}: {text}"
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing reached the engine"
        );
        assert_eq!(
            ends(&authority),
            vec![Some(capyctl_controller::LeaseEnd::NotAccepted); 2],
            "{engine:?}"
        );
        assert_eq!(deps.inflight.current("dep-1"), 0);
        let (status, answer) = post_raw(
            &router,
            chat_body("public-alias", serde_json::json!({})),
            None,
        )
        .await;
        assert_eq!(status, 200, "{engine:?}: {answer}");
    }
}

// SPEC §10 (`cache_salt`): the value is a non-empty string of at most
// `MAX_CACHE_SALT_BYTES` bytes; anything else is a 400 before any engine.
#[tokio::test]
async fn a_malformed_cache_salt_is_refused_before_any_engine() {
    let (endpoint, seen) = recording_engine().await;
    let (authority, deps) = stub_router_on(&endpoint, Some("sglang"));
    let router = capyctl_router::serve_router(deps);
    let long = "s".repeat(capyctl_adapters::forward::MAX_CACHE_SALT_BYTES + 1);
    for salt in [
        serde_json::json!(""),
        serde_json::json!(7),
        serde_json::json!(["tenant-a"]),
        serde_json::json!(long),
    ] {
        let body = chat_body("public-alias", serde_json::json!({"cache_salt": salt}));
        let (status, answer) = post_raw(&router, body, None).await;
        assert_eq!(status, 400, "{answer}");
        assert_eq!(answer["code"], "invalid_request", "{answer}");
    }
    assert!(seen.lock().unwrap().is_empty());
    assert!(ends(&authority).is_empty(), "no lease was opened");
    let longest = "s".repeat(capyctl_adapters::forward::MAX_CACHE_SALT_BYTES);
    let body = chat_body("public-alias", serde_json::json!({"cache_salt": longest}));
    let (status, answer) = post_raw(&router, body, None).await;
    assert_eq!(status, 200, "{answer}");
}

/// Wait, at most five seconds, until `done` holds.
async fn eventually(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "never happened");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

// T17 (SPEC §10, owner decision 2026-10-18): a completed answer, collected or
// streamed through `[DONE]`, reaches the client before its lease close is
// committed. Until the close is written the lease is open and the request
// still holds its in-flight slot; the close then ends it completed and frees
// the slot.
#[tokio::test]
async fn a_completed_answer_reaches_the_client_before_its_lease_close() {
    let (endpoint, _) = scripted_engine(200, "text/event-stream", engine_sse("served-name")).await;
    let (authority, deps) = stub_router(&endpoint);
    authority
        .closes
        .held
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let router = capyctl_router::serve_router(deps.clone());

    let (status, body) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ends(&authority), vec![None], "answered before the close");
    assert_eq!(
        deps.inflight.current("dep-1"),
        1,
        "the slot waits for the close"
    );
    authority.closes.permits.add_permits(1);
    eventually(|| ends(&authority) == vec![Some(capyctl_controller::LeaseEnd::Completed)]).await;
    eventually(|| deps.inflight.current("dep-1") == 0).await;

    let (status, text) = chat_stream(&router, "public-alias").await;
    assert_eq!(status, 200);
    assert!(text.contains("[DONE]"), "{text}");
    assert_eq!(
        ends(&authority)[1],
        None,
        "the stream ended before its close"
    );
    assert_eq!(
        deps.inflight.current("dep-1"),
        1,
        "the slot waits for the close"
    );
    authority.closes.permits.add_permits(1);
    eventually(|| ends(&authority) == vec![Some(capyctl_controller::LeaseEnd::Completed); 2]).await;
    eventually(|| deps.inflight.current("dep-1") == 0).await;
}

// T17 (SPEC §10, owner decision 2026-10-18): a client that hangs up right
// after its whole answer, collected or streamed, does not stop the lease
// close: the close runs on the dispatch's own task, not on the connection.
#[tokio::test]
async fn a_client_that_leaves_after_its_answer_still_has_its_lease_closed() {
    let (endpoint, _) = scripted_engine(200, "text/event-stream", engine_sse("served-name")).await;
    let (authority, deps) = stub_router(&endpoint);
    authority
        .closes
        .held
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = capyctl_router::serve_router(deps.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    for stream in [false, true] {
        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{address}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": "public-alias", "stream": stream,
                "messages": [{"role": "user", "content": "hi"}],
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.text().await.unwrap();
        assert!(!stream || body.contains("[DONE]"), "{body}");
        // The client goes away with its connection.
        drop(client);
    }
    assert_eq!(ends(&authority), vec![None, None]);
    authority.closes.permits.add_permits(2);
    eventually(|| ends(&authority) == vec![Some(capyctl_controller::LeaseEnd::Completed); 2]).await;
    eventually(|| deps.inflight.current("dep-1") == 0).await;
}

// T17 (SPEC §10, owner decision 2026-10-18): a process that dies between a
// completed answer and its lease close never closes the lease. With the
// durable ledger it stays charged until the quiescence path settles it
// (capyctl-store `a_local_grant_survives_a_crash_before_its_close`).
#[test]
fn a_crash_between_answer_and_close_leaves_the_lease_charged() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let authority = runtime.block_on(async {
        let (endpoint, _) =
            scripted_engine(200, "text/event-stream", engine_sse("served-name")).await;
        let (authority, deps) = stub_router(&endpoint);
        authority
            .closes
            .held
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let router = capyctl_router::serve_router(deps);
        let (status, body) = chat_once(&router, "public-alias").await;
        assert_eq!(status, 200, "{body}");
        authority
    });
    // The process dies with the close still pending.
    drop(runtime);
    assert_eq!(ends(&authority), vec![None], "never closed: still charged");
}

// ---------------------------------------------------------------------------
// ADR 0032: SGLang's per-request abort and the restart when held requests
// fill a deployment's bound.

/// The router's dependencies on `authority`, as [`stub_router_on`] builds them.
fn deps_on(authority: &Arc<StubAuthority>) -> RouterDeps {
    let port = authority.clone() as Arc<dyn capyctl_controller::LifecyclePort>;
    RouterDeps {
        controller: port.clone(),
        forwards: Arc::new(capyctl_router::forwarders::LiveForwarders::new(port)),
        limits: QueueLimits {
            max_requests_per_deployment: 4,
            max_buffered_bytes_total: 64 * 1024,
        },
        api_key: None,
        inflight: Arc::new(capyctl_router::admission::InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    }
}

/// What an SGLang double received: each chat body, with the
/// `X-CapyCTL-Request-Id` header it carried.
type Received = Arc<Mutex<Vec<(serde_json::Value, Option<String>)>>>;

/// How an SGLang double answers each chat request.
#[derive(Clone, Copy)]
enum Script {
    /// A whole stream at once, finished `stop`.
    Answers,
    /// The stream's opening chunk (and a content chunk, when `content`),
    /// then nothing until the authority sends an abort; then its final chunk
    /// with finish reason `abort` and `[DONE]` (SGLang 0.5.22's graceful
    /// abort).
    AbortFinishes { content: bool },
    /// As `AbortFinishes`, ending after the abort with an error event.
    AbortErrors { content: bool },
    /// As `AbortFinishes`, ending after the abort with the final chunk but no
    /// `[DONE]`.
    AbortTruncates { content: bool },
}

/// ADR 0032: an SGLang double for `script`, ending an aborted request when
/// `aborted` gives it a permit (one per abort the authority sent). Each
/// answer's `id` is the request's `rid` when it has one, as SGLang's is.
async fn sglang_engine(script: Script, aborted: Arc<tokio::sync::Semaphore>) -> (String, Received) {
    let received: Received = Arc::default();
    let recorded = received.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(
            move |headers: axum::http::HeaderMap,
                  axum::Json(body): axum::Json<serde_json::Value>| {
                let header = headers
                    .get("x-capyctl-request-id")
                    .map(|value| value.to_str().unwrap().to_owned());
                let id = body["rid"].as_str().unwrap_or("chat-1").to_owned();
                recorded.lock().unwrap().push((body, header));
                let aborted = aborted.clone();
                async move {
                    let chunk = move |delta: serde_json::Value, finish: serde_json::Value| {
                        format!(
                            "data: {}\n\n",
                            serde_json::json!({"id": id, "object": "chat.completion.chunk",
                                "created": 1, "model": "served-name",
                                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
                        )
                    };
                    let null = serde_json::Value::Null;
                    let stream = async_stream::stream! {
                        let opening = chunk(serde_json::json!({"role": "assistant", "content": ""}), null.clone());
                        yield Ok::<_, std::io::Error>(opening);
                        let content = match script {
                            Script::Answers => {
                                yield Ok(chunk(serde_json::json!({"content": "hello"}), "stop".into()));
                                yield Ok("data: [DONE]\n\n".to_owned());
                                return;
                            }
                            Script::AbortFinishes { content }
                            | Script::AbortErrors { content }
                            | Script::AbortTruncates { content } => content,
                        };
                        if content {
                            yield Ok(chunk(serde_json::json!({"content": "he"}), null.clone()));
                        }
                        aborted.acquire().await.unwrap().forget();
                        match script {
                            Script::AbortFinishes { .. } => {
                                yield Ok(chunk(serde_json::json!({}), "abort".into()));
                                yield Ok("data: [DONE]\n\n".to_owned());
                            }
                            Script::AbortErrors { .. } => {
                                let error = serde_json::json!({"error": {"object": "error",
                                    "message": "Generation aborted.", "type": "ServiceUnavailable",
                                    "code": 503}});
                                yield Ok(format!("data: {error}\n\n"));
                            }
                            Script::AbortTruncates { .. } => {
                                yield Ok(chunk(serde_json::json!({}), "abort".into()));
                            }
                            Script::Answers => {}
                        }
                    };
                    (
                        [("content-type", "text/event-stream")],
                        axum::body::Body::from_stream(stream),
                    )
                }
            },
        ),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (base, received)
}

/// ADR 0032: deployment `dep-1` on one SGLang instance answering as `script`,
/// its lane capable when `request_abort`, reached through an ingress when
/// `remote`.
async fn sglang_lane(
    remote: bool,
    request_abort: bool,
    script: Script,
) -> (Arc<StubAuthority>, RouterDeps, Received) {
    let authority = StubAuthority::sglang("http://127.0.0.1:9", remote, request_abort);
    let (endpoint, received) = sglang_engine(script, authority.abort.aborted.clone()).await;
    authority.runtime.lock().unwrap().endpoint = endpoint;
    let deps = deps_on(&authority);
    (authority, deps, received)
}

/// Serve `deps` on a loopback port, for a client that hangs up.
async fn serve(deps: &RouterDeps) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = capyctl_router::serve_router(deps.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    address
}

fn cancelling(answered: bool) -> Vec<Option<capyctl_controller::LeaseEnd>> {
    vec![Some(capyctl_controller::LeaseEnd::Cancelling { answered })]
}

/// The one abort a cut request on `dep-1` asks for: its instance's
/// generation and CapyCTL's request ID, the first lease's.
fn one_abort() -> Vec<(String, i64, String)> {
    vec![("dep-1".into(), 1, "lease-0".into())]
}

// T17 (ADR 0032 §1–§2): on a capable SGLang lane every chat request, streamed
// or collected, reaches the engine with `rid` set to CapyCTL's request ID,
// the value of the response's `X-CapyCTL-Request-Id`, never the caller's
// `X-Request-Id`; through a host's ingress the ID goes as that header and the
// body carries no `rid`. A lane without the capability carries neither. A
// caller's own `rid` is still refused before anything is sent.
#[tokio::test]
async fn capyctl_request_id_reaches_only_a_capable_sglang_lane() {
    for (remote, request_abort) in [(false, true), (true, true), (false, false), (true, false)] {
        let (authority, deps, received) = sglang_lane(remote, request_abort, Script::Answers).await;
        let router = capyctl_router::serve_router(deps);
        let mut ids = Vec::new();
        for stream in [false, true] {
            let response = router
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/v1/chat/completions")
                        .header("content-type", "application/json")
                        .header("x-request-id", "caller-7")
                        .body(axum::body::Body::from(chat_body(
                            "public-alias",
                            serde_json::json!({"stream": stream}),
                        )))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let id = response.headers()["x-capyctl-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            let text = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            assert!(String::from_utf8_lossy(&text).contains("hello"));
            ids.push(id);
        }
        let received = received.lock().unwrap().clone();
        assert_eq!(received.len(), 2);
        for ((body, header), id) in received.iter().zip(&ids) {
            let case = format!("remote {remote}, capable {request_abort}: {body}");
            let rid = body.get("rid").and_then(serde_json::Value::as_str);
            match (request_abort, remote) {
                (true, false) => {
                    assert_eq!(rid, Some(id.as_str()), "{case}");
                    assert_eq!(header, &None, "{case}");
                }
                (true, true) => {
                    assert_eq!(rid, None, "{case}");
                    assert_eq!(header.as_deref(), Some(id.as_str()), "{case}");
                }
                (false, _) => {
                    assert_eq!(rid, None, "{case}");
                    assert_eq!(header, &None, "{case}");
                }
            }
            assert_ne!(id, "caller-7");
        }
        assert!(authority.aborts().is_empty());
        assert_eq!(
            ends(&authority),
            vec![Some(capyctl_controller::LeaseEnd::Completed); 2]
        );
    }
    let (authority, deps, received) = sglang_lane(false, true, Script::Answers).await;
    let router = capyctl_router::serve_router(deps);
    let (status, answer) = post_raw(
        &router,
        chat_body("public-alias", serde_json::json!({"rid": "mine"})),
        None,
    )
    .await;
    assert_eq!(status, 400, "{answer}");
    assert_eq!(answer["code"], "invalid_request");
    assert!(received.lock().unwrap().is_empty());
    assert!(ends(&authority).is_empty());
}

// T17 (ADR 0032 §3–§4): a client that hangs up a stream on a capable lane
// leaves its lease cancelling exactly as before (answered: the engine
// produced output), and the router asks the engine once to abort CapyCTL's
// request ID while it keeps reading the response. The engine's final output
// for it, read whole, acknowledges that lease by its ID.
#[tokio::test]
async fn a_stream_hung_up_on_a_capable_lane_settles_on_its_own_final_output() {
    let (authority, deps, received) =
        sglang_lane(false, true, Script::AbortFinishes { content: true }).await;
    let address = serve(&deps).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{address}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "public-alias", "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-capyctl-request-id"], "lease-0");
    let mut body = response.bytes_stream();
    let mut seen = String::new();
    while !seen.contains("\"he\"") {
        let bytes = body.next().await.unwrap().unwrap();
        seen.push_str(&String::from_utf8_lossy(&bytes));
    }
    drop(body);
    drop(client);
    eventually(|| authority.acks() == vec!["lease-0".to_owned()]).await;
    assert_eq!(authority.aborts(), one_abort());
    assert_eq!(ends(&authority), cancelling(true));
    assert_eq!(received.lock().unwrap()[0].0["rid"], "lease-0");
    eventually(|| deps.inflight.current("dep-1") == 0).await;
}

// T17 (ADR 0032 §3–§4): a client that disconnects in the middle of a
// collected request on a capable lane: cancelling as unanswered, aborted
// once, acknowledged when the engine's final output arrives whole.
#[tokio::test]
async fn a_collected_request_hung_up_on_a_capable_lane_settles_on_its_own_final_output() {
    let (authority, deps, received) =
        sglang_lane(false, true, Script::AbortFinishes { content: false }).await;
    let address = serve(&deps).await;
    let client = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": "public-alias",
                "messages": [{"role": "user", "content": "hi"}],
            }))
            .send()
            .await
    });
    eventually(|| !received.lock().unwrap().is_empty()).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        authority.aborts().is_empty(),
        "nothing is aborted before the cut"
    );
    client.abort();
    let _ = client.await;
    eventually(|| authority.acks() == vec!["lease-0".to_owned()]).await;
    assert_eq!(authority.aborts(), one_abort());
    assert_eq!(ends(&authority), cancelling(false));
    eventually(|| deps.inflight.current("dep-1") == 0).await;
}

// T17 (ADR 0032 §3–§4): a collected request cut at its deadline before any
// output on a capable lane gets today's answer, is cancelling as unanswered,
// and settles on its own final output after the abort.
#[tokio::test]
async fn a_deadline_cut_on_a_capable_lane_settles_on_its_own_final_output() {
    let (authority, deps, _) =
        sglang_lane(false, true, Script::AbortFinishes { content: false }).await;
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            deadline: std::time::Duration::from_millis(300),
            ..Default::default()
        });
    let router = capyctl_router::serve_router(deps.clone());
    let (status, answer) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 500, "{answer}");
    assert_eq!(answer["code"], "engine_error");
    eventually(|| authority.acks() == vec!["lease-0".to_owned()]).await;
    assert_eq!(authority.aborts(), one_abort());
    assert_eq!(ends(&authority), cancelling(false));
}

// T17 (ADR 0032 §3–§4): a stream cut at the idle bound after output on a
// capable lane ends for its client without `[DONE]`, is cancelling as
// answered, and settles on its own final output after the abort.
#[tokio::test]
async fn an_idle_cut_on_a_capable_lane_settles_on_its_own_final_output() {
    let (authority, deps, _) =
        sglang_lane(false, true, Script::AbortFinishes { content: true }).await;
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            stream_idle: std::time::Duration::from_millis(300),
            ..Default::default()
        });
    let router = capyctl_router::serve_router(deps.clone());
    let (status, text) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        chat_stream(&router, "public-alias"),
    )
    .await
    .expect("the client's stream ends at the cut");
    assert_eq!(status, 200);
    assert!(text.contains("\"he\""), "{text}");
    assert!(
        !text.contains("[DONE]") && !text.contains("abort"),
        "{text}"
    );
    eventually(|| authority.acks() == vec!["lease-0".to_owned()]).await;
    assert_eq!(authority.aborts(), one_abort());
    assert_eq!(ends(&authority), cancelling(true));
}

// T17 (ADR 0032 §4): on a capable lane, after the abort, an error event or a
// final chunk without `[DONE]` is not the request's final output read whole:
// no acknowledgement, the lease stays cancelling.
#[tokio::test]
async fn an_error_or_truncated_end_after_the_abort_is_not_acknowledged() {
    for script in [
        Script::AbortErrors { content: true },
        Script::AbortTruncates { content: true },
    ] {
        let (authority, deps, _) = sglang_lane(false, true, script).await;
        deps.inflight
            .waiting
            .set_limits(capyctl_router::queue::WaitLimits {
                stream_idle: std::time::Duration::from_millis(300),
                ..Default::default()
            });
        let router = capyctl_router::serve_router(deps.clone());
        let (status, _) = chat_stream(&router, "public-alias").await;
        assert_eq!(status, 200);
        eventually(|| authority.aborts() == one_abort()).await;
        // Longer than the bound counted from the abort.
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(authority.acks().is_empty());
        assert_eq!(ends(&authority), cancelling(true));
    }
}

// T17 (ADR 0032 §3–§4): when the engine never ends an aborted request, the
// router keeps its connection open for the stream idle bound counted from
// the abort, then closes it; nothing is acknowledged.
#[tokio::test]
async fn an_aborted_request_that_never_ends_is_closed_at_the_bound() {
    let engine = capyctl_testkit::HeldEngine::opening().await;
    let authority = StubAuthority::sglang(&engine.endpoint(), false, true);
    let deps = deps_on(&authority);
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            deadline: std::time::Duration::from_millis(300),
            stream_idle: std::time::Duration::from_millis(800),
            ..Default::default()
        });
    let router = capyctl_router::serve_router(deps.clone());
    let (status, _) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 500);
    assert_eq!(authority.aborts(), one_abort());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(engine.closed(), 0, "still read after the abort");
    assert!(
        engine
            .closed_within(1, std::time::Duration::from_secs(5))
            .await,
        "closed once the bound passed"
    );
    assert!(authority.acks().is_empty());
    assert_eq!(ends(&authority), cancelling(false));
}

// T17 (ADR 0032 §4): an abort that could not be sent leaves nothing to wait
// for: the engine connection closes at once and nothing is acknowledged.
#[tokio::test]
async fn a_cut_whose_abort_was_not_sent_closes_the_engine_connection_at_once() {
    let engine = capyctl_testkit::HeldEngine::opening().await;
    let authority = StubAuthority::sglang(&engine.endpoint(), false, true);
    authority
        .abort
        .sends
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let deps = deps_on(&authority);
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            deadline: std::time::Duration::from_millis(300),
            stream_idle: std::time::Duration::from_secs(60),
            ..Default::default()
        });
    let router = capyctl_router::serve_router(deps.clone());
    let (status, _) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 500);
    assert!(
        engine
            .closed_within(1, std::time::Duration::from_secs(2))
            .await,
        "closed without waiting for the bound"
    );
    assert_eq!(authority.aborts(), one_abort());
    assert!(authority.acks().is_empty());
    assert_eq!(ends(&authority), cancelling(false));
}

// T17 (ADR 0032 §6): an SGLang lane without the capability keeps today's
// rule: a cut drops the forward at once, and nothing is aborted.
#[tokio::test]
async fn a_cut_on_a_lane_without_the_capability_is_not_aborted() {
    let engine = capyctl_testkit::HeldEngine::opening().await;
    let authority = StubAuthority::sglang(&engine.endpoint(), false, false);
    let deps = deps_on(&authority);
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            deadline: std::time::Duration::from_millis(300),
            stream_idle: std::time::Duration::from_secs(60),
            ..Default::default()
        });
    let router = capyctl_router::serve_router(deps.clone());
    let (status, _) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 500);
    assert!(
        engine
            .closed_within(1, std::time::Duration::from_secs(2))
            .await
    );
    assert!(authority.aborts().is_empty());
    assert!(authority.acks().is_empty());
    assert_eq!(ends(&authority), cancelling(false));
}

// T17 (ADR 0032 §7): a grant refused because the deployment's bound is full
// asks the authority for a restart. While the instances holding requests
// until their stop restart, the request waits for them as for an activation
// and is then served; with the instance view and without one.
#[tokio::test]
async fn a_full_bound_of_held_requests_restarts_and_the_request_waits() {
    let (endpoint, _) = scripted_engine(200, "text/event-stream", engine_sse("served-name")).await;
    let instance = StubAuthority::sglang(&endpoint, false, false);
    let (whole, _) = stub_router_on(&endpoint, Some("sglang"));
    for authority in [instance, whole] {
        *authority.refuse_leases.lock().unwrap() = Some(capyctl_controller::LeaseRefused::Full);
        *authority.abort.restart.lock().unwrap() = capyctl_controller::HeldRestart::Restarting;
        authority
            .abort
            .activation_held
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let router = capyctl_router::serve_router(deps_on(&authority));
        let request = tokio::spawn(async move { chat_once(&router, "public-alias").await });
        eventually(|| {
            authority
                .abort
                .activations
                .load(std::sync::atomic::Ordering::SeqCst)
                == 1
        })
        .await;
        assert_eq!(authority.restarts(), vec!["dep-1".to_owned()]);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!request.is_finished(), "waits for the restart");
        authority.abort.activation_permits.add_permits(1);
        let (status, body) = request.await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            ends(&authority),
            vec![Some(capyctl_controller::LeaseEnd::Completed)]
        );
    }
}

// T17 (ADR 0032 §7): when the open leases are not all held until a stop, no
// restart happens and the request is refused `queue_full` as before.
#[tokio::test]
async fn a_full_bound_not_held_is_refused_queue_full() {
    let (endpoint, _) = scripted_engine(200, "text/event-stream", engine_sse("served-name")).await;
    let authority = StubAuthority::sglang(&endpoint, false, false);
    *authority.refuse_leases.lock().unwrap() = Some(capyctl_controller::LeaseRefused::Full);
    let router = capyctl_router::serve_router(deps_on(&authority));
    let (status, body) = chat_once(&router, "public-alias").await;
    assert_eq!(status, 429, "{body}");
    assert_eq!(body["code"], "queue_full");
    assert_eq!(authority.restarts(), vec!["dep-1".to_owned()]);
    assert_eq!(
        authority
            .abort
            .activations
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(ends(&authority).is_empty());
}

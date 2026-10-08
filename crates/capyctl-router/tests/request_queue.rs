//! SPEC §10 steps 1–2 and 8 (W10): a request for a deployment that is not
//! servable waits in a bounded queue and joins that deployment's one
//! activation, which the lifecycle authority drives (switching included); the
//! queued requests are dispatched once it is READY. A client that leaves
//! while queued frees only its own slot.
//!
//! The authority and the engine are in-process doubles. CPU only; passing
//! here never qualifies a native engine (SPEC §18).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use capyctl_adapters::traits::{AdapterError, ChatForward};
use capyctl_controller::{LeaseEnd, LeaseRefused, LifecycleFault, RequestLease};
use capyctl_router::forwarders::{ForwarderError, ForwarderSource};
use capyctl_router::queue::WaitLimits;
use capyctl_router::{QueueLimits, RouterDeps};
use serde_json::{json, Value};

const DEPLOYMENT: &str = "01J0000000000000000000000B";
const ROUTE: &str = "model-b";

/// A deployment that becomes READY only when its activation is released.
struct Authority {
    ready: AtomicBool,
    activations: AtomicUsize,
    finished: AtomicBool,
    release: tokio::sync::Semaphore,
}

impl Authority {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ready: AtomicBool::new(false),
            activations: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
            release: tokio::sync::Semaphore::new(0),
        })
    }
    fn row(&self) -> capyctl_store::deployments::DeploymentRow {
        let state = if self.ready.load(Ordering::SeqCst) {
            capyctl_domain::LifecycleState::Ready
        } else {
            capyctl_domain::LifecycleState::Parked
        };
        capyctl_store::deployments::DeploymentRow {
            id: DEPLOYMENT.into(),
            name: "b".into(),
            kind: "model".into(),
            route_model_id: Some(ROUTE.into()),
            desired_state: capyctl_domain::LifecycleState::Ready,
            observed_state: state,
            schema_version: 1,
            current_generation: 1,
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
    fn list_enabled_route_ids(&self) -> Result<Vec<String>, LifecycleFault> {
        Ok(vec![ROUTE.into()])
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
        _d: &str,
    ) -> Result<Option<capyctl_controller::RuntimeEndpoint>, LifecycleFault> {
        Ok(None)
    }
    async fn open_request_lease(
        &self,
        _d: &str,
        _max: usize,
    ) -> Result<Option<RequestLease>, LeaseRefused> {
        assert!(
            self.ready.load(Ordering::SeqCst),
            "SPEC §10 step 8: nothing is dispatched before the deployment is READY"
        );
        Ok(None)
    }
    async fn close_request_lease(
        &self,
        _l: RequestLease,
        _e: LeaseEnd,
    ) -> Result<(), LeaseRefused> {
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
    /// The switch (or wake) the coordinator would run: held until released.
    async fn activate_for_request(&self, deployment: &str) -> Result<(), LifecycleFault> {
        assert_eq!(deployment, DEPLOYMENT);
        self.activations.fetch_add(1, Ordering::SeqCst);
        self.release.acquire().await.unwrap().forget();
        self.ready.store(true, Ordering::SeqCst);
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct Engine {
    served: AtomicUsize,
}

#[async_trait::async_trait]
impl ChatForward for Engine {
    async fn forward_chat(&self, _body: &Value) -> Result<Value, AdapterError> {
        self.served.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"object": "chat.completion", "choices": [{"index": 0,
            "message": {"role": "assistant", "content": "b"}, "finish_reason": "stop"}]}))
    }
}

struct Forwards(Arc<Engine>);
impl ForwarderSource for Forwards {
    fn forwarder(&self, _d: &str) -> Result<Arc<dyn ChatForward>, ForwarderError> {
        Ok(self.0.clone())
    }
}

struct Fixture {
    deps: RouterDeps,
    authority: Arc<Authority>,
    engine: Arc<Engine>,
}

fn fixture(limits: WaitLimits) -> Fixture {
    let authority = Authority::new();
    let engine = Arc::new(Engine {
        served: AtomicUsize::new(0),
    });
    let deps = RouterDeps {
        controller: authority.clone(),
        forwards: Arc::new(Forwards(engine.clone())),
        limits: QueueLimits {
            max_requests_per_deployment: 64,
            max_buffered_bytes_total: 64 * 1024,
        },
        api_key: None,
        inflight: Arc::new(capyctl_router::admission::InFlight::with_wait_limits(
            limits,
        )),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    };
    Fixture {
        deps,
        authority,
        engine,
    }
}

fn limits(per: usize, bytes: usize, deadline_ms: u64) -> WaitLimits {
    WaitLimits {
        max_pending_per_deployment: per,
        max_pending_total: 256,
        max_buffered_bytes_total: bytes,
        deadline: Duration::from_millis(deadline_ms),
        stream_idle: Duration::from_secs(120),
    }
}

fn body(padding: usize) -> Value {
    json!({"model": ROUTE, "messages": [{"role": "user", "content": "x".repeat(padding)}]})
}

fn request(
    f: &Fixture,
    padding: usize,
) -> tokio::task::JoinHandle<Result<Value, (StatusCode, Value)>> {
    let deps = f.deps.clone();
    tokio::spawn(async move {
        capyctl_router::chat::dispatch(&deps, ROUTE, &body(padding))
            .await
            .map_err(|(status, body)| (status, body.0))
    })
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

// T15 T19 (SPEC §10 steps 1, 2, 8): requests arriving while the deployment
// is not servable queue instead of being refused, join one activation, and
// are all dispatched once it is READY.
#[tokio::test]
async fn queued_requests_join_one_activation_and_are_all_served() {
    let f = fixture(limits(8, 1 << 20, 10_000));
    let queued: Vec<_> = (0..5).map(|_| request(&f, 16)).collect();
    until("five queued", || {
        f.deps.inflight.waiting.waiting(DEPLOYMENT) == 5
    })
    .await;
    assert_eq!(
        f.engine.served.load(Ordering::SeqCst),
        0,
        "no fake tokens while waiting"
    );
    f.authority.release.add_permits(1);
    for outcome in futures::future::join_all(queued).await {
        outcome.unwrap().unwrap();
    }
    assert_eq!(
        f.authority.activations.load(Ordering::SeqCst),
        1,
        "one activation"
    );
    assert_eq!(f.engine.served.load(Ordering::SeqCst), 5);
    assert_eq!(f.deps.inflight.waiting.totals(), (0, 0));
    assert_eq!(f.deps.inflight.current(DEPLOYMENT), 0);
}

// T19 (SPEC §10: bound queues by requests and buffered bytes): past either
// bound a request is refused at once, structured, and nothing else changes.
#[tokio::test]
async fn the_waiting_queue_is_bounded_by_count_and_buffered_bytes() {
    let f = fixture(limits(2, 4_096, 10_000));
    let first = request(&f, 16);
    let second = request(&f, 16);
    until("two queued", || {
        f.deps.inflight.waiting.waiting(DEPLOYMENT) == 2
    })
    .await;
    let (status, refused) = request(&f, 16).await.unwrap().unwrap_err();
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused["code"], "queue_full");

    let f = fixture(limits(8, 4_096, 10_000));
    let waiting = request(&f, 3_000);
    until("one queued", || {
        f.deps.inflight.waiting.waiting(DEPLOYMENT) == 1
    })
    .await;
    let (status, refused) = request(&f, 3_000).await.unwrap().unwrap_err();
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused["code"], "queue_full");
    f.authority.release.add_permits(1);
    waiting.await.unwrap().unwrap();
    drop((first, second));
}

// T17 T38 (SPEC §10: a client disconnect is not proof of anything; the
// activation other requests joined is never cancelled by one of them): a
// queued client that leaves frees its own slot; the activation runs on and a
// later request is served by it without a second activation.
#[tokio::test]
async fn a_client_leaving_the_queue_frees_its_slot_and_never_cancels_the_activation() {
    let f = fixture(limits(8, 1 << 20, 10_000));
    let leaving = request(&f, 64);
    until("queued", || {
        f.deps.inflight.waiting.waiting(DEPLOYMENT) == 1
    })
    .await;
    leaving.abort();
    let _ = leaving.await;
    until("slot freed", || f.deps.inflight.waiting.totals() == (0, 0)).await;
    assert_eq!(f.authority.activations.load(Ordering::SeqCst), 1);
    assert!(!f.authority.finished.load(Ordering::SeqCst));
    f.authority.release.add_permits(1);
    until("activation finished", || {
        f.authority.finished.load(Ordering::SeqCst)
    })
    .await;
    request(&f, 16).await.unwrap().unwrap();
    assert_eq!(f.authority.activations.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.engine.served.load(Ordering::SeqCst),
        1,
        "the leaver was never sent"
    );
}

// T19 (SPEC §10: explicit deadlines include activation): a request that
// waits past its deadline is refused retryably without reaching an engine;
// the activation continues for the others.
#[tokio::test]
async fn a_request_waiting_past_its_deadline_is_refused_retryably() {
    let f = fixture(limits(8, 1 << 20, 100));
    let (status, refused) = request(&f, 16).await.unwrap().unwrap_err();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused["retryable"], true);
    assert_eq!(f.deps.inflight.waiting.totals(), (0, 0));
    assert_eq!(f.engine.served.load(Ordering::SeqCst), 0);
    f.authority.release.add_permits(1);
    until("activation finished", || {
        f.authority.finished.load(Ordering::SeqCst)
    })
    .await;
    assert_eq!(f.authority.activations.load(Ordering::SeqCst), 1);
}

// T15 T19, SPEC §10 (owner decision 2026-10-08): with no waiting allowed
// (`max_pending_per_deployment: 0`), a request for a deployment that is not
// servable is refused at once, retryably, yet it still starts the one
// activation a waiting request would have joined, so a retry after the hint
// finds the deployment servable. Concurrent refusals start no second one.
#[tokio::test]
async fn with_no_waiting_a_request_is_refused_at_once_but_starts_the_activation() {
    let f = fixture(limits(0, 1 << 20, 10_000));
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let (status, refused) = request(&f, 16).await.unwrap().unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it did not wait"
        );
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refused["code"], "queue_full", "{refused}");
        assert_eq!(refused["retryable"], true);
    }
    assert_eq!(f.deps.inflight.waiting.totals(), (0, 0));
    until("the activation started", || {
        f.authority.activations.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(f.engine.served.load(Ordering::SeqCst), 0);
    f.authority.release.add_permits(1);
    until("activation finished", || {
        f.authority.finished.load(Ordering::SeqCst)
    })
    .await;
    request(&f, 16).await.unwrap().unwrap();
    assert_eq!(f.authority.activations.load(Ordering::SeqCst), 1);
    assert_eq!(f.engine.served.load(Ordering::SeqCst), 1);
}

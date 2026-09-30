//! ADR 0013 §10 (unit I3, owner decision D9): the router balances one
//! deployment's requests across its READY instances on two fake hosts, and
//! fails over only before an engine accepted a request.
//!
//! The authority and the engines are in-process doubles: each instance is a
//! fake engine behind its own forwarder, each on its own "host". CPU and fake
//! engines only; passing here never qualifies a native engine (SPEC §18).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use capyctl_adapters::traits::{AdapterError, ChatForward, ChatSink, StreamEnded};
use capyctl_controller::load_table::{EngineGauges, LoadView};
use capyctl_controller::{LeaseEnd, LeaseRefused, LifecycleFault, RequestLease, ServingInstance};
use capyctl_router::forwarders::{ForwarderError, ForwarderSource};
use capyctl_router::{QueueLimits, RouterDeps};
use serde_json::{json, Value};
use tower::ServiceExt;

const DEPLOYMENT: &str = "01J0000000000000000000000D";
const ROUTE: &str = "toy";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Answer after the engine's delay.
    Serve,
    /// Refuse before anything is sent (a refused connection).
    Refuse,
    /// Accept, then lose the host mid-request.
    Lose,
}

/// One fake engine: counts what it served and how many it ran at once.
struct Engine {
    host: String,
    delay: Duration,
    mode: Mutex<Mode>,
    served: AtomicUsize,
    accepted: AtomicUsize,
    active: AtomicUsize,
}

impl Engine {
    fn new(host: &str, delay_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            host: host.into(),
            delay: Duration::from_millis(delay_ms),
            mode: Mutex::new(Mode::Serve),
            served: AtomicUsize::new(0),
            accepted: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
        })
    }
    fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }
    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }
    async fn run(&self) -> Result<String, AdapterError> {
        let mode = *self.mode.lock().unwrap();
        if mode == Mode::Refuse {
            return Err(AdapterError::NotAccepted(
                "the connection to the serving host was refused".into(),
            ));
        }
        self.accepted.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        if mode == Mode::Lose {
            return Err(AdapterError::Uncertain(
                "the host session ended mid-request".into(),
            ));
        }
        self.served.fetch_add(1, Ordering::SeqCst);
        Ok(format!("answer from {}", self.host))
    }
}

#[async_trait::async_trait]
impl ChatForward for Engine {
    async fn forward_chat(&self, _body: &Value) -> Result<Value, AdapterError> {
        let content = self.run().await?;
        Ok(json!({"object": "chat.completion", "choices": [{"index": 0,
            "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}]}))
    }
    async fn forward_chat_stream_async(
        &self,
        _body: &Value,
        sink: &mut dyn ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        let content = self.run().await?;
        let _ = sink
            .send(json!({"choices": [{"index": 0, "delta": {"content": content}}]}).to_string())
            .await;
        Ok(StreamEnded::Completed)
    }
}

/// The lifecycle authority's view of two instances, one per host.
struct Authority {
    instances: Mutex<Vec<ServingInstance>>,
    /// Every lease granted: (lease id, generation, how it ended).
    leases: Mutex<Vec<(String, i64, Option<LeaseEnd>)>>,
    /// Generations whose lease grant is refused as closed.
    closed: Mutex<HashSet<i64>>,
    /// Generations that no longer hold a runtime when the forwarder is resolved.
    moved: Mutex<HashSet<i64>>,
}

impl Authority {
    fn new() -> Arc<Self> {
        let instance = |index: u32, generation: i64, host: &str| ServingInstance {
            instance_index: index,
            generation,
            host_id: Some(host.into()),
            remote_host: Some(host.into()),
            launch_command_id: Some(format!("launch-{generation}")),
            dispatch_open: true,
            host_live: true,
            host_unresponsive: false,
            engine_exited: false,
            load: None,
        };
        Arc::new(Self {
            instances: Mutex::new(vec![instance(0, 5, "host-a"), instance(1, 6, "host-b")]),
            leases: Mutex::new(Vec::new()),
            closed: Mutex::new(HashSet::new()),
            moved: Mutex::new(HashSet::new()),
        })
    }

    fn with<R>(&self, generation: i64, change: impl FnOnce(&mut ServingInstance) -> R) -> R {
        let mut instances = self.instances.lock().unwrap();
        change(
            instances
                .iter_mut()
                .find(|i| i.generation == generation)
                .unwrap(),
        )
    }

    /// A fresh W8 sample for one instance, as the server's load table serves it.
    fn report(&self, generation: i64, running: u32, fresh: bool) {
        self.with(generation, |i| {
            i.load = Some(LoadView {
                deployment_id: DEPLOYMENT.into(),
                generation,
                host_id: i.remote_host.clone().unwrap(),
                owned_handle: i.launch_command_id.clone().unwrap(),
                sampled_at_ms: 0,
                age_ms: if fresh { 500 } else { 4_000 },
                fresh,
                ingress_in_flight: 0,
                engine: Some(EngineGauges {
                    running,
                    waiting: 0,
                    kv_usage_ppm: 0,
                }),
            })
        });
    }

    fn ends(&self, generation: i64) -> Vec<Option<LeaseEnd>> {
        self.leases
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, g, _)| *g == generation)
            .map(|(_, _, end)| *end)
            .collect()
    }

    fn row(&self) -> capyctl_store::deployments::DeploymentRow {
        capyctl_store::deployments::DeploymentRow {
            id: DEPLOYMENT.into(),
            name: "toy".into(),
            kind: "vllm".into(),
            route_model_id: Some(ROUTE.into()),
            desired_state: capyctl_domain::LifecycleState::Ready,
            observed_state: capyctl_domain::LifecycleState::Ready,
            schema_version: 1,
            current_generation: 6,
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
        _deployment_id: &str,
    ) -> Result<Option<capyctl_store::deployments::OperationRow>, LifecycleFault> {
        Ok(None)
    }
    fn ready_deployments_excluding(&self, _d: &str) -> Result<Vec<String>, LifecycleFault> {
        Ok(Vec::new())
    }
    fn runtime_endpoint(
        &self,
        _deployment: &str,
    ) -> Result<Option<capyctl_controller::RuntimeEndpoint>, LifecycleFault> {
        panic!("an authority with an instance view is never asked for the whole deployment")
    }
    async fn open_request_lease(
        &self,
        _deployment: &str,
        _max: usize,
    ) -> Result<Option<RequestLease>, LeaseRefused> {
        panic!("an authority with an instance view grants instance leases only")
    }
    async fn close_request_lease(
        &self,
        lease: RequestLease,
        end: LeaseEnd,
    ) -> Result<(), LeaseRefused> {
        let mut leases = self.leases.lock().unwrap();
        let entry = leases
            .iter_mut()
            .find(|(id, _, _)| id == lease.id())
            .expect("a lease this authority granted");
        assert!(entry.2.is_none(), "a lease is closed once");
        entry.2 = Some(end);
        Ok(())
    }
    fn serving_instances(
        &self,
        deployment: &str,
    ) -> Result<Option<Vec<ServingInstance>>, LifecycleFault> {
        assert_eq!(deployment, DEPLOYMENT);
        Ok(Some(self.instances.lock().unwrap().clone()))
    }
    async fn open_instance_lease(
        &self,
        deployment: &str,
        generation: i64,
        _max: usize,
    ) -> Result<Option<RequestLease>, LeaseRefused> {
        assert_eq!(deployment, DEPLOYMENT);
        if self.closed.lock().unwrap().contains(&generation) {
            return Err(LeaseRefused::Closed);
        }
        let mut leases = self.leases.lock().unwrap();
        let id = format!("lease-{}", leases.len());
        leases.push((id.clone(), generation, None));
        Ok(Some(RequestLease::unrecorded(&id)))
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
        Err(refuse("observe engine work"))
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
        Err(refuse("activate"))
    }
    async fn request_transition(
        &self,
        _d: &str,
        _a: capyctl_domain::LifecycleAction,
    ) -> Result<capyctl_controller::OperationHandle, LifecycleFault> {
        Err(refuse("transition"))
    }
}

/// Forwarders per instance generation, as `LiveForwarders::forwarder_for`
/// resolves them from the authority.
struct Engines {
    authority: Arc<Authority>,
    by_generation: HashMap<i64, Arc<Engine>>,
    /// Every generation a forwarder was resolved for, in order.
    resolved: Mutex<Vec<i64>>,
}

impl ForwarderSource for Engines {
    fn forwarder(&self, _deployment: &str) -> Result<Arc<dyn ChatForward>, ForwarderError> {
        panic!("an instance-routed request resolves its forwarder by generation")
    }
    fn forwarder_for(
        &self,
        deployment: &str,
        generation: i64,
    ) -> Result<Arc<dyn ChatForward>, ForwarderError> {
        self.resolved.lock().unwrap().push(generation);
        if self.authority.moved.lock().unwrap().contains(&generation) {
            return Err(ForwarderError::NoRuntime(deployment.into()));
        }
        Ok(self.by_generation[&generation].clone())
    }
}

struct Fixture {
    deps: RouterDeps,
    authority: Arc<Authority>,
    engines: Arc<Engines>,
    a: Arc<Engine>,
    b: Arc<Engine>,
}

fn fixture(delay_ms: u64) -> Fixture {
    let authority = Authority::new();
    let (a, b) = (
        Engine::new("host-a", delay_ms),
        Engine::new("host-b", delay_ms),
    );
    let engines = Arc::new(Engines {
        authority: authority.clone(),
        by_generation: HashMap::from([(5, a.clone()), (6, b.clone())]),
        resolved: Mutex::new(Vec::new()),
    });
    let deps = RouterDeps {
        controller: authority.clone(),
        forwards: engines.clone(),
        limits: QueueLimits {
            max_requests_per_deployment: 256,
            max_buffered_bytes_total: 64 * 1024,
        },
        api_key: None,
        inflight: Arc::new(capyctl_router::admission::InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    };
    Fixture {
        deps,
        authority,
        engines,
        a,
        b,
    }
}

async fn chat(f: &Fixture) -> Result<Value, (StatusCode, Value)> {
    capyctl_router::chat::dispatch(&f.deps, ROUTE, &json!({"model": ROUTE, "messages": []}))
        .await
        .map_err(|(status, body)| (status, body.0))
}

async fn concurrently(f: &Fixture, n: usize) -> Vec<Result<Value, (StatusCode, Value)>> {
    futures::future::join_all((0..n).map(|_| chat(f))).await
}

// T17 (D9): concurrent load across two instances on two hosts is balanced by
// the router's own in-flight counts; every request is answered once, and each
// lease names the instance that served it.
#[tokio::test]
async fn concurrent_requests_balance_across_two_instances() {
    let f = fixture(60);
    let results = concurrently(&f, 40).await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    let (a, b) = (f.a.served(), f.b.served());
    assert_eq!(a + b, 40);
    assert!((18..=22).contains(&a), "a {a}, b {b}");
    // Every lease was charged to the instance whose engine served it.
    assert_eq!(f.authority.ends(5).len(), a);
    assert_eq!(f.authority.ends(6).len(), b);
    assert!(f
        .authority
        .leases
        .lock()
        .unwrap()
        .iter()
        .all(|(_, _, end)| *end == Some(LeaseEnd::Completed)));
    let resolved = f.engines.resolved.lock().unwrap().clone();
    assert_eq!(resolved.len(), 40, "one forwarder per lease, none extra");
    // Router counts return to zero; per-deployment accounting too.
    assert_eq!(f.deps.inflight.instance_in_flight(DEPLOYMENT, 5), 0);
    assert_eq!(f.deps.inflight.instance_in_flight(DEPLOYMENT, 6), 0);
    assert_eq!(f.deps.inflight.current(DEPLOYMENT), 0);
}

// T17 (long-prompt skew, D9): an instance whose engine reports a queue built
// by long prompts receives new work only once the other catches up.
#[tokio::test]
async fn reported_engine_load_shifts_new_requests_to_the_lighter_instance() {
    let f = fixture(60);
    f.authority.report(5, 8, true);
    // One at a time: the lighter instance takes all of them.
    for _ in 0..4 {
        chat(&f).await.unwrap();
    }
    assert_eq!((f.a.served(), f.b.served()), (0, 4));
    // A burst: host b takes work until its own in-flight matches the queue
    // host a reports; only then do ties share.
    let results = concurrently(&f, 12).await;
    assert!(results.iter().all(Result::is_ok));
    let (a, b) = (f.a.served(), f.b.served() - 4);
    assert_eq!(a + b, 12);
    assert!(b >= 9 && a <= 3, "a {a}, b {b}");
}

// T18 T29 (ADR 0013 §10): a stale sample is unknown load, never evidence of
// either capacity or pressure; the choice falls back to router in-flight.
#[tokio::test]
async fn a_stale_sample_falls_back_to_router_in_flight() {
    let f = fixture(60);
    f.authority.report(5, 50, false);
    let results = concurrently(&f, 20).await;
    assert!(results.iter().all(Result::is_ok));
    assert_eq!((f.a.served(), f.b.served()), (10, 10));
}

// T38 T33 (SPEC §10, §13.2): host loss. A request refused before anything was
// sent fails over; once the host's session is gone the instance is not a
// candidate; after the host re-joins and re-proves readiness it serves again.
#[tokio::test]
async fn host_loss_fails_over_new_requests_and_rejoin_restores_the_instance() {
    let f = fixture(10);
    // The connection to host a is refused before the server has noticed.
    f.a.set(Mode::Refuse);
    for _ in 0..6 {
        let answer = chat(&f).await.unwrap();
        assert_eq!(
            answer["choices"][0]["message"]["content"],
            "answer from host-b"
        );
    }
    assert_eq!(f.b.served(), 6);
    assert_eq!(f.a.accepted.load(Ordering::SeqCst), 0);
    // Each refused offer's lease was closed as not accepted; none uncertain.
    let a_ends = f.authority.ends(5);
    assert!(!a_ends.is_empty());
    assert!(a_ends.iter().all(|end| *end == Some(LeaseEnd::NotAccepted)));
    assert!(f
        .authority
        .ends(6)
        .iter()
        .all(|end| *end == Some(LeaseEnd::Completed)));
    // The session is gone: host a is no longer offered at all.
    f.authority.with(5, |i| i.host_live = false);
    let before = f.authority.ends(5).len();
    for _ in 0..4 {
        chat(&f).await.unwrap();
    }
    assert_eq!(
        f.authority.ends(5).len(),
        before,
        "no lease for a lost host"
    );
    // Rejoin: a new session re-proved readiness; host a serves again.
    f.a.set(Mode::Serve);
    f.authority.with(5, |i| i.host_live = true);
    let results = concurrently(&f, 10).await;
    assert!(results.iter().all(Result::is_ok));
    assert!(f.a.served() >= 4, "a {} after rejoin", f.a.served());
    // Both instances lost: a retryable 503, never a 500.
    f.authority.with(5, |i| i.host_live = false);
    f.authority.with(6, |i| i.dispatch_open = false);
    let (status, body) = chat(&f).await.unwrap_err();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["retryable"], true);
}

// T38 T17 (SPEC §10): a request the engine accepted is never replayed. The
// host is lost mid-request: the client gets an honest failure and the durable
// lease stays charged as uncertain. The per-process in-flight slot is released:
// the ledger, not the router's memory, keeps the conservative charge.
#[tokio::test]
async fn an_accepted_request_is_not_replayed_when_its_host_is_lost() {
    let f = fixture(20);
    f.a.set(Mode::Lose);
    f.b.set(Mode::Lose);
    let (status, body) = chat(&f).await.unwrap_err();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["code"], "engine_error");
    let accepted = f.a.accepted.load(Ordering::SeqCst) + f.b.accepted.load(Ordering::SeqCst);
    assert_eq!(accepted, 1, "offered to exactly one engine");
    let leases = f.authority.leases.lock().unwrap().clone();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].2, Some(LeaseEnd::Uncertain));
    assert_eq!(
        f.deps.inflight.current(DEPLOYMENT),
        0,
        "the ledger keeps the charge; the in-memory slot is released"
    );
    // The routing hint is not accounting: it does not pin the instance forever.
    assert_eq!(
        f.deps.inflight.instance_in_flight(DEPLOYMENT, leases[0].1),
        0
    );
}

// T18 (ADR 0013 §10, the lease-then-forwarder fence): the lease names a
// generation; if that incarnation holds no runtime by the time its forwarder
// is resolved, nothing is sent under that lease, it closes as not accepted and
// the request goes to the other instance under a lease of its own. A lease the
// store refuses because the gate closed fails over the same way.
#[tokio::test]
async fn a_lease_is_forwarded_only_to_the_generation_it_names() {
    let f = fixture(5);
    f.authority.moved.lock().unwrap().insert(5);
    for _ in 0..4 {
        let answer = chat(&f).await.unwrap();
        assert_eq!(
            answer["choices"][0]["message"]["content"],
            "answer from host-b"
        );
    }
    assert_eq!(f.a.accepted.load(Ordering::SeqCst), 0);
    assert!(f
        .authority
        .ends(5)
        .iter()
        .all(|end| *end == Some(LeaseEnd::NotAccepted)));
    f.authority.moved.lock().unwrap().clear();
    f.authority.closed.lock().unwrap().insert(6);
    let before = f.authority.ends(6).len();
    for _ in 0..4 {
        let answer = chat(&f).await.unwrap();
        assert_eq!(
            answer["choices"][0]["message"]["content"],
            "answer from host-a"
        );
    }
    assert_eq!(
        f.authority.ends(6).len(),
        before,
        "a refused grant leaves nothing"
    );
    // Resolution follows the lease: every forwarder resolved for generation 6
    // after this point would be a bug; only 5 was resolved.
    let resolved = f.engines.resolved.lock().unwrap().clone();
    assert!(resolved[resolved.len() - 4..].iter().all(|g| *g == 5));
}

// T38 (SPEC §10): a stream refused before its first byte fails over; the
// client sees one engine's output and one terminator.
#[tokio::test]
async fn a_stream_refused_before_forwarding_fails_over() {
    let f = fixture(5);
    f.a.set(Mode::Refuse);
    // Make host a the first choice so the refusal is exercised.
    f.authority.report(6, 3, true);
    let router = capyctl_router::serve_router(f.deps.clone());
    let response = router
        .oneshot(
            axum::http::Request::post("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"model": ROUTE, "stream": true, "messages": []}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("answer from host-b"), "{text}");
    assert!(!text.contains("host-a"), "{text}");
    assert_eq!(text.matches("[DONE]").count(), 1, "{text}");
    assert_eq!(f.authority.ends(5), vec![Some(LeaseEnd::NotAccepted)]);
    assert_eq!(f.authority.ends(6), vec![Some(LeaseEnd::Completed)]);
}

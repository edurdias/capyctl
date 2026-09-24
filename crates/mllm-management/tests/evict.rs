//! Owner decision 2026-09-23: `start deployment --evict` and `start instance
//! --evict` run the W10 switch plan through the management API; a default
//! start never evicts.
//!
//! The Fake engine refuses every park before any effect, so a victim is
//! released by a verified stop on the plan's next round (ADR 0013 §8 rule 8).
//! CPU and Fake-engine only: nothing here qualifies an engine recipe.
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use mllm_controller::{
    coordinator::{
        CoordinatorError, CoordinatorOptions, ObservationFuture, OwnedCoordinator,
        ServiceObservation,
    },
    switching::{SwitchOptions, Switcher},
    OwnedCoordinatorState,
};
use mllm_domain::resources::MemoryObservation;
use mllm_management::{
    actions::OwnedActionSource, configuration::SharedConfigurationSource, lifecycle_router,
    ManagementCredentials,
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;

use mllm_testkit::fixture;
const MANAGEMENT: &str = "management-credential-012345678901234567890";
const INFERENCE: &str = "inference-credential-0123456789012345678901";

struct Observations(Vec<MemoryObservation>);
impl ServiceObservation for Observations {
    fn observe(&self, _: String) -> ObservationFuture {
        let values = self.0.clone();
        Box::pin(async move { Ok(values) })
    }
}

struct Lab {
    dir: tempfile::TempDir,
    owner: Arc<Mutex<OwnedCoordinatorState>>,
    worker: OwnedCoordinator,
    app: axum::Router,
    switcher: Arc<Switcher>,
    /// The fixture's two deployments: 10 GiB cold and 8 GiB Ready each.
    a: String,
    b: String,
}

/// A host whose 15 GiB managed limit holds one READY deployment beside a
/// cold start of another, never two cold starts.
async fn lab() -> Lab {
    let source = fixture::owned_source().await;
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    {
        let o = owner.lock().unwrap();
        let mut controls = o.store().resource_policy("lab").unwrap().unwrap().controls;
        controls.domains.get_mut("unified").unwrap().managed_limit = 15 << 30;
        controls.queue.admission_window_ms = 50;
        o.store()
            .update_resource_policy(
                o.session(),
                "owner",
                "lab",
                1,
                "evict-budget",
                &controls,
                &source.observations,
                1800,
            )
            .unwrap();
    }
    let worker = mllm_testkit::spawn_fake_coordinator(
        owner.clone(),
        Arc::new(Observations(source.observations.clone())),
        Arc::new(|| Ok::<_, CoordinatorError>(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let host: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let configuration = Arc::new(
        SharedConfigurationSource::new(owner.clone(), host["input"]["host"].clone(), "owner")
            .unwrap(),
    );
    let switcher = Arc::new(Switcher::new(
        worker.commands(),
        SwitchOptions {
            drain_timeout: Duration::from_secs(5),
            poll: Duration::from_millis(10),
            ..Default::default()
        },
    ));
    let app = lifecycle_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(
            OwnedActionSource::new(configuration, worker.commands())
                .unwrap()
                .with_switcher(switcher.clone()),
        ),
    );
    Lab {
        a: source.fence.deployment_id.clone(),
        b: source.other.deployment_id.clone(),
        dir,
        owner,
        worker,
        app,
        switcher,
    }
}

fn action(path: &str, key: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/management/v1/deployments/{path}/actions"))
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn start(evict: bool) -> Value {
    let mut body = json!({"expected_revision": 1, "action": "start", "deadline_ms": 100_000});
    if evict {
        body["evict"] = json!(true);
    }
    body
}

async fn send(lab: &Lab, request: Request<Body>) -> (u16, Value) {
    let response = lab.app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    (status, body)
}

impl Lab {
    fn instance(&self, deployment: &str, index: u32) -> (String, bool) {
        let o = self.owner.lock().unwrap();
        let snapshot = o.store().snapshot().unwrap();
        let d = snapshot
            .deployments
            .into_iter()
            .find(|d| d.id == deployment)
            .unwrap();
        let i = d.instances.into_iter().find(|i| i.index == index).unwrap();
        (i.observed_state, d.dispatch_enabled)
    }

    async fn until(&self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !done(self) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    fn switch_events(&self) -> Vec<String> {
        let sql = rusqlite::Connection::open(self.dir.path().join("srv.sqlite3")).unwrap();
        let mut statement = sql
            .prepare(
                "SELECT kind FROM management_events WHERE kind LIKE 'switch_%' ORDER BY sequence",
            )
            .unwrap();
        statement
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}

// T10 T15 T16 T19 (owner decision 2026-09-23): a default start never evicts:
// B is refused for capacity and A keeps serving. With `evict` the same start
// runs the switch plan, releases A (its park is refused, so it is stopped with
// verified cleanup), starts B and reports A as the victim. A stays eligible
// for on-demand activation. An exact retry replays the receipt and evicts
// nothing more; `evict` on anything but a start is refused.
#[tokio::test]
async fn a_default_start_never_evicts_and_evict_reports_its_victims() {
    let lab = lab().await;
    let (status, a) = send(&lab, action(&lab.a, "start-a", start(false))).await;
    assert_eq!(status, 202, "{a}");
    lab.until("A ready", |l| l.instance(&l.a, 0).0 == "ready")
        .await;

    let (status, refused) = send(&lab, action(&lab.b, "start-b", start(false))).await;
    assert_eq!(status, 503, "{refused}");
    assert_eq!(refused["error"]["code"], "capacity_blocked");
    assert_eq!(lab.instance(&lab.a, 0), ("ready".into(), true));
    assert!(
        lab.switch_events().is_empty(),
        "a default start never evicts"
    );

    let (status, evicted) = send(&lab, action(&lab.b, "start-b-evict", start(true))).await;
    assert_eq!(status, 202, "{evicted}");
    assert_eq!(evicted["victims"], json!([format!("{}/0", lab.a)]));
    assert!(evicted["switch_id"].is_string());
    assert_eq!(evicted["deployment_id"], json!(lab.b));
    lab.until("B ready", |l| l.instance(&l.b, 0).0 == "ready")
        .await;
    assert_ne!(lab.instance(&lab.a, 0).0, "ready");
    assert!(
        !lab.owner
            .lock()
            .unwrap()
            .store()
            .is_admin_stopped(&lab.a)
            .unwrap(),
        "an evicted deployment stays eligible for on-demand activation"
    );
    lab.until("the switch to end", |l| {
        l.switch_events().last().map(String::as_str) == Some("switch_completed")
    })
    .await;
    // Status no longer shows a switch in progress once it ends.
    lab.until("status to clear the switch", |l| {
        let o = l.owner.lock().unwrap();
        o.store()
            .snapshot()
            .unwrap()
            .deployments
            .iter()
            .all(|d| d.switch.is_none())
    })
    .await;

    // An exact retry is the same accepted start; nothing else is released.
    let (status, again) = send(&lab, action(&lab.b, "start-b-evict", start(true))).await;
    assert_eq!(status, 202, "{again}");
    assert_eq!(again["operation_id"], evicted["operation_id"]);
    assert_eq!(again["victims"], json!([]));

    let mut stop = start(true);
    stop["action"] = json!("stop");
    let (status, body) = send(&lab, action(&lab.b, "stop-evict", stop)).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request");
    lab.worker.shutdown().await.unwrap();
}

// T10 T15 (owner decision 2026-09-23): `start instance <d>/<k> --evict` makes
// room for that instance and reports the victims the same way.
#[tokio::test]
async fn start_instance_evict_makes_room_for_that_instance() {
    let lab = lab().await;
    let (status, _) = send(&lab, action(&lab.a, "start-a", start(false))).await;
    assert_eq!(status, 202);
    lab.until("A ready", |l| l.instance(&l.a, 0).0 == "ready")
        .await;
    let path = format!("{}/instances/0", lab.b);
    let (status, refused) = send(&lab, action(&path, "start-b0", start(false))).await;
    assert_eq!(status, 503, "{refused}");
    let (status, evicted) = send(&lab, action(&path, "start-b0-evict", start(true))).await;
    assert_eq!(status, 202, "{evicted}");
    assert_eq!(evicted["instance"], 0);
    assert_eq!(evicted["victims"], json!([format!("{}/0", lab.a)]));
    lab.until("B/0 ready", |l| l.instance(&l.b, 0).0 == "ready")
        .await;
    lab.worker.shutdown().await.unwrap();
}

// T10 T09 (SPEC §6.4, owner decision 2026-09-23): `start --evict` validates
// the start (revision, instance, idempotency key) before it releases anyone.
// A start that would be refused must not have drained and stopped a victim
// first.
#[tokio::test]
async fn an_evicting_start_that_would_be_refused_evicts_nothing() {
    let lab = lab().await;
    let (status, a) = send(&lab, action(&lab.a, "start-a", start(false))).await;
    assert_eq!(status, 202, "{a}");
    lab.until("A ready", |l| l.instance(&l.a, 0).0 == "ready")
        .await;
    let mut stale = start(true);
    stale["expected_revision"] = json!(2);
    let (status, body) = send(&lab, action(&lab.b, "stale-evict", stale.clone())).await;
    assert_eq!(status, 409, "{body}");
    let (status, body) =
        send(&lab, action(&format!("{}/instances/0", lab.b), "stale-evict-0", stale)).await;
    assert_eq!(status, 409, "{body}");
    let (status, body) =
        send(&lab, action(&format!("{}/instances/5", lab.b), "missing-evict", start(true))).await;
    assert_eq!(status, 404, "{body}");
    // A key already used for another command is a conflict, not an eviction.
    let mut stop = start(false);
    stop["action"] = json!("stop");
    let (status, body) = send(&lab, action(&lab.b, "reused", stop)).await;
    assert_eq!(status, 202, "{body}");
    let (status, body) = send(&lab, action(&lab.b, "reused", start(true))).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(lab.instance(&lab.a, 0), ("ready".into(), true), "A was evicted");
    assert!(lab.switch_events().is_empty(), "{:?}", lab.switch_events());
    lab.worker.shutdown().await.unwrap();
}

// T19 (SPEC §10 bounded work; owner decision 2026-09-23): evicting starts run
// as their own tasks past the bounded command slots, so they are bounded
// themselves. While B's switch turn is held, the evicting starts past
// `MAX_EVICTING_STARTS` are refused at once, retryably, and none is spawned.
#[tokio::test]
async fn evicting_starts_in_flight_are_bounded() {
    use mllm_management::actions::MAX_EVICTING_STARTS;
    let lab = lab().await;
    let turn = lab.switcher.target_turn(&lab.b).await;
    let extra = 3;
    let (done, mut finished) = tokio::sync::mpsc::unbounded_channel();
    for n in 0..MAX_EVICTING_STARTS + extra {
        let (app, done) = (lab.app.clone(), done.clone());
        let request = action(&lab.b, &format!("evict-{n}"), start(true));
        tokio::spawn(async move {
            let response = app.oneshot(request).await.unwrap();
            let _ = done.send(response.status().as_u16());
        });
    }
    // Held turn: no accepted evicting start can finish, so every early
    // answer is a refusal, and there are exactly `extra` of them.
    for _ in 0..extra {
        let status = tokio::time::timeout(Duration::from_secs(10), finished.recv())
            .await
            .expect("a refusal past the bound")
            .unwrap();
        assert_eq!(status, 429);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(300), finished.recv())
            .await
            .is_err(),
        "only the evicting starts past the bound are refused"
    );
    drop(turn);
    for _ in 0..MAX_EVICTING_STARTS {
        let status = tokio::time::timeout(Duration::from_secs(30), finished.recv())
            .await
            .expect("admitted evicting starts finish")
            .unwrap();
        assert_ne!(status, 429);
    }
    lab.worker.shutdown().await.unwrap();
}

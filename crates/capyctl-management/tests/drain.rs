//! Owner decision 4 (2026-09-22): `drain host` on an offline host. The drain is
//! accepted at once with its Stops pending; the Stops stay durable and complete
//! through the ordinary cleanup path when the host reconnects; the host takes
//! no new placements while any of them is unsettled. Fake-engine tests only;
//! not qualification.
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use capyctl_controller::{
    coordinator::{
        CoordinatorError, CoordinatorOptions, ObservationFuture, OwnedCoordinator,
        ServiceObservation,
    },
    OwnedCoordinatorState,
};
use capyctl_domain::resources::MemoryObservation;
use capyctl_management::{
    actions::OwnedActionSource, configuration::SharedConfigurationSource,
    drain::drain_router_with_presence, lifecycle_router, ManagementCredentials,
};
use capyctl_testkit::fixture;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tower::ServiceExt;

const MANAGEMENT: &str = "management-credential-012345678901234567890";
const INFERENCE: &str = "inference-credential-0123456789012345678901";

/// The coordinator's view of which remote hosts are connected, shared with the
/// drain router's presence.
struct Presence {
    observations: Vec<MemoryObservation>,
    online: Arc<AtomicBool>,
}
impl ServiceObservation for Presence {
    fn observe(&self, _: String) -> ObservationFuture {
        let values = self.observations.clone();
        Box::pin(async move { Ok(values) })
    }
    fn online_hosts(&self) -> Option<BTreeSet<String>> {
        Some(if self.online.load(Ordering::SeqCst) {
            ["lab".to_string()].into_iter().collect()
        } else {
            BTreeSet::new()
        })
    }
}

struct Setup {
    dir: tempfile::TempDir,
    owner: Arc<Mutex<OwnedCoordinatorState>>,
    worker: OwnedCoordinator,
    id: String,
    actions: axum::Router,
    drain: axum::Router,
    online: Arc<AtomicBool>,
}

async fn setup() -> Setup {
    let source = fixture::owned_source().await;
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let online = Arc::new(AtomicBool::new(true));
    let worker = capyctl_testkit::spawn_fake_coordinator(
        owner.clone(),
        Arc::new(Presence {
            observations: source.observations.clone(),
            online: online.clone(),
        }),
        Arc::new(|| Ok::<_, CoordinatorError>(1900)),
        CoordinatorOptions {
            poll_interval: Duration::from_millis(10),
            ..Default::default()
        },
    )
    .unwrap();
    let host: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let configuration = Arc::new(
        SharedConfigurationSource::new(owner.clone(), host["input"]["host"].clone(), "owner")
            .unwrap(),
    );
    let id = source.fence.deployment_id.clone();
    let source = Arc::new(OwnedActionSource::new(configuration, worker.commands()).unwrap());
    let credentials =
        || ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap();
    let actions = lifecycle_router(credentials(), source.clone());
    let presence = online.clone();
    let drain = drain_router_with_presence(
        credentials(),
        source,
        Vec::new(),
        Arc::new(move |host: &str| host == "lab" && presence.load(Ordering::SeqCst)),
    );
    Setup {
        dir,
        owner,
        worker,
        id,
        actions,
        drain,
        online,
    }
}

async fn body(response: axum::response::Response) -> (u16, Value) {
    let status = response.status().as_u16();
    let value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    (status, value)
}

async fn start(setup: &Setup, id: &str) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/management/v1/deployments/{id}/actions"))
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .header("content-type", "application/json")
        .header("idempotency-key", "start")
        .body(Body::from(
            json!({"expected_revision":1,"action":"start","deadline_ms":10000}).to_string(),
        ))
        .unwrap();
    let (status, started) = body(setup.actions.clone().oneshot(request).await.unwrap()).await;
    assert_eq!(status, 202, "{started}");
    settled(&setup.owner, started["operation_id"].as_str().unwrap()).await;
}

async fn drain(setup: &Setup, key: &str) -> (u16, Value) {
    drain_until(setup, key, 10000).await
}

async fn drain_until(setup: &Setup, key: &str, deadline_ms: i64) -> (u16, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/management/v1/hosts/lab/drain")
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(Body::from(
            json!({ "deadline_ms": deadline_ms }).to_string(),
        ))
        .unwrap();
    body(setup.drain.clone().oneshot(request).await.unwrap()).await
}

fn state_of(owner: &Arc<Mutex<OwnedCoordinatorState>>, id: &str) -> Option<String> {
    owner
        .lock()
        .unwrap()
        .store()
        .snapshot()
        .unwrap()
        .operations
        .iter()
        .find(|op| op.id == id)
        .map(|op| op.state.clone())
}

async fn settled(owner: &Arc<Mutex<OwnedCoordinatorState>>, id: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while state_of(owner, id).as_deref() != Some("succeeded") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

/// Enroll `lab` and bind the running deployment's runtime to it.
fn place_on_lab(dir: &tempfile::TempDir, owner: &Arc<Mutex<OwnedCoordinatorState>>, id: &str) {
    let binding = owner
        .lock()
        .unwrap()
        .store()
        .runtime_binding(id)
        .unwrap()
        .unwrap()
        .id;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    sql.execute(
        "INSERT OR IGNORE INTO enrolled_hosts(host_id,host_name,key_digest) VALUES('lab','lab','digest')",
        [],
    )
    .unwrap();
    sql.execute(
        "INSERT INTO remote_binding_ingress VALUES(?1,'lab','http://100.64.0.1:9443')",
        [binding],
    )
    .unwrap();
}

/// The offline host's drain answers at once with its Stop pending and marks
/// the host; the Stop waits for the host and completes when it reconnects,
/// which clears the marker. A retry with the same key replays the same Stop.
// T10 T32 T33
#[tokio::test]
async fn an_offline_host_drain_returns_at_once_and_completes_on_reconnect() {
    let setup = setup().await;
    let id = setup.id.clone();
    start(&setup, &id).await;
    place_on_lab(&setup.dir, &setup.owner, &id);
    setup.online.store(false, Ordering::SeqCst);

    let answered = std::time::Instant::now();
    let (status, accepted) = drain(&setup, "drain-offline").await;
    assert!(
        answered.elapsed() < Duration::from_secs(5),
        "answered at once"
    );
    assert_eq!(status, 202, "{accepted}");
    assert_eq!(accepted["host"], "lab");
    assert_eq!(accepted["host_state"], "offline");
    assert_eq!(accepted["stops"], "pending");
    let operations = accepted["operations"].as_array().unwrap();
    assert_eq!(operations.len(), 1, "{accepted}");
    assert_eq!(operations[0]["deployment_id"], id.as_str());
    let operation = operations[0]["operation_id"].as_str().unwrap().to_owned();

    // The Stop stays open and the host stays marked while it is offline.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_ne!(
        state_of(&setup.owner, &operation).as_deref(),
        Some("succeeded")
    );
    assert!(setup
        .owner
        .lock()
        .unwrap()
        .store()
        .host_drain_pending("lab")
        .unwrap());
    // An exact retry replays the same Stop.
    let (_, retried) = drain(&setup, "drain-offline").await;
    assert_eq!(retried["operations"], accepted["operations"]);

    // The host reconnects: the Stop completes through the ordinary cleanup
    // path and the marker clears.
    setup.online.store(true, Ordering::SeqCst);
    settled(&setup.owner, &operation).await;
    assert!(!setup
        .owner
        .lock()
        .unwrap()
        .store()
        .host_drain_pending("lab")
        .unwrap());
    let snapshot = setup.owner.lock().unwrap().store().snapshot().unwrap();
    let deployment = snapshot.deployments.iter().find(|d| d.id == id).unwrap();
    // SPEC §6.3: a drain is not an operator stop.
    assert!(!deployment.suspended);
    setup.worker.shutdown().await.unwrap();
}

/// A connected host's drain says so and issues its Stops as before.
// T10
#[tokio::test]
async fn an_online_host_drain_reports_the_host_online() {
    let setup = setup().await;
    let id = setup.id.clone();
    start(&setup, &id).await;
    place_on_lab(&setup.dir, &setup.owner, &id);
    let (status, accepted) = drain(&setup, "drain-online").await;
    assert_eq!(status, 202, "{accepted}");
    assert_eq!(accepted["host_state"], "online");
    assert_eq!(accepted["stops"], "issued");
    let operation = accepted["operations"][0]["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    settled(&setup.owner, &operation).await;
    assert!(!setup
        .owner
        .lock()
        .unwrap()
        .store()
        .host_drain_pending("lab")
        .unwrap());
    setup.worker.shutdown().await.unwrap();
}

/// Router review item 14: a drain interrupted after it wrote its intent and
/// before any Stop (simulated by opening the intent directly) holds the host
/// out of placement; the next drain of the host stops what runs there and
/// completes the intent, so the hold becomes the ordinary Stop marker and
/// clears when that Stop settles.
// T10 T33
#[tokio::test]
async fn an_interrupted_drain_intent_holds_the_host_until_a_drain_completes() {
    let setup = setup().await;
    let id = setup.id.clone();
    start(&setup, &id).await;
    place_on_lab(&setup.dir, &setup.owner, &id);
    let named = setup
        .owner
        .lock()
        .unwrap()
        .store()
        .begin_host_drain("lab", "crashed", 1)
        .unwrap();
    assert_eq!(named.len(), 1, "the intent named the running instance");
    let pending = |setup: &Setup| {
        setup
            .owner
            .lock()
            .unwrap()
            .store()
            .host_drain_pending("lab")
            .unwrap()
    };
    assert!(pending(&setup), "held before any Stop exists");

    let (status, accepted) = drain(&setup, "drain-after-crash").await;
    assert_eq!(status, 202, "{accepted}");
    let operation = accepted["operations"][0]["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    settled(&setup.owner, &operation).await;
    assert!(
        !pending(&setup),
        "both intents completed and the Stop settled"
    );
    setup.worker.shutdown().await.unwrap();
}

/// Router review item 14: a drain of an enrolled host with nothing on it writes
/// and completes its intent, leaving no hold behind.
// T10 T33
#[tokio::test]
async fn a_drain_of_an_idle_host_leaves_no_hold() {
    let setup = setup().await;
    let sql = rusqlite::Connection::open(setup.dir.path().join("srv.sqlite3")).unwrap();
    sql.execute(
        "INSERT OR IGNORE INTO enrolled_hosts(host_id,host_name,key_digest) VALUES('lab','lab','digest')",
        [],
    )
    .unwrap();
    let (status, accepted) = drain(&setup, "drain-idle").await;
    assert_eq!(status, 202, "{accepted}");
    assert_eq!(accepted["operations"], json!([]));
    let intents: i64 = sql
        .query_row(
            "SELECT count(*) FROM host_drain_intents WHERE host_id='lab' AND completed_at_ms IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(intents, 1, "the intent was written and completed");
    // SPEC §4.3: the intent carries the drain's own deadline, so one whose
    // request is abandoned expires instead of holding the host forever.
    let deadline: i64 = sql
        .query_row(
            "SELECT deadline_ms FROM host_drain_intent_deadlines WHERE host_id='lab' AND drain_key='drain-idle'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(deadline, 10000);
    assert!(!setup
        .owner
        .lock()
        .unwrap()
        .store()
        .host_drain_pending("lab")
        .unwrap());
    setup.worker.shutdown().await.unwrap();
}

/// SPEC §4.3, §6: the operator CLI bounds a drain at 900 s, but a Stop's
/// deadline may not lie beyond its launch's request deadline (here the golden
/// fixture's, shorter than 900 s). The drain still stops the deployment: each
/// Stop takes the earlier of the drain's bound and its own request-deadline
/// window, and a retried drain under the same key replays that Stop's receipt
/// rather than conflicting with it.
// T10 T13 T32
#[tokio::test]
async fn a_drain_bound_beyond_the_request_deadline_still_stops() {
    let setup = setup().await;
    let id = setup.id.clone();
    start(&setup, &id).await;
    place_on_lab(&setup.dir, &setup.owner, &id);
    // The worker's clock reads 1900; the CLI's drain window is 900 s.
    let bound = 1900 + 900_000;
    let (status, accepted) = drain_until(&setup, "drain-long", bound).await;
    assert_eq!(status, 202, "{accepted}");
    assert_eq!(accepted["refused"], json!([]), "{accepted}");
    let operations = accepted["operations"].as_array().unwrap();
    assert_eq!(operations.len(), 1, "{accepted}");
    assert_eq!(operations[0]["deployment_id"], id.as_str());
    let operation = operations[0]["operation_id"].as_str().unwrap().to_owned();
    // An exact retry replays the same Stop.
    let (status, retried) = drain_until(&setup, "drain-long", bound).await;
    assert_eq!(status, 202, "{retried}");
    assert_eq!(retried["refused"], json!([]), "{retried}");
    assert_eq!(retried["operations"], accepted["operations"]);
    settled(&setup.owner, &operation).await;
    let snapshot = setup.owner.lock().unwrap().store().snapshot().unwrap();
    let deployment = snapshot.deployments.iter().find(|d| d.id == id).unwrap();
    // SPEC §6.3: a drain is not an operator stop.
    assert!(!deployment.suspended);
    setup.worker.shutdown().await.unwrap();
}

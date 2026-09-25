//! ADR 0018 §4: retiring a runtime profile through the ordinary stop path,
//! and `GET /management/v1/engines`. Fake-engine tests; not qualification.
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use mllm_controller::profile_retirement::{ProfileRetirements, RetirementStep};
use mllm_controller::{
    coordinator::{
        CoordinatorError, CoordinatorOptions, ObservationFuture, OwnedCoordinator,
        ServiceObservation,
    },
    OwnedCoordinatorState,
};
use mllm_domain::resources::MemoryObservation;
use mllm_management::engines::StoreRetirements;
use mllm_management::{
    actions::OwnedActionSource, configuration::SharedConfigurationSource, lifecycle_router,
    ManagementCredentials,
};
use mllm_testkit::fixture;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
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
    let worker = mllm_testkit::spawn_fake_coordinator(
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
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
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
    let actions = lifecycle_router(credentials(), source);
    Setup {
        dir,
        owner,
        worker,
        id,
        actions,
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

fn retirements(setup: &Setup) -> StoreRetirements {
    let host: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let configuration = Arc::new(
        SharedConfigurationSource::new(setup.owner.clone(), host["input"]["host"].clone(), "owner")
            .unwrap(),
    );
    StoreRetirements::new(Arc::new(
        OwnedActionSource::new(configuration, setup.worker.commands()).unwrap(),
    ))
    // The fake coordinator's clock reads 1900 ms.
    .with_clock(Arc::new(|| 1900))
}

// T16 T32: in use without drain names the deployment and leaves it running;
// with drain the ordinary stop runs and the retirement confirms only after
// the stop succeeded on evidence and nothing holds a runtime.
#[tokio::test]
async fn a_drained_retirement_confirms_only_after_the_stop_settles() {
    let setup = setup().await;
    let id = setup.id.clone();
    start(&setup, &id).await;
    place_on_lab(&setup.dir, &setup.owner, &id);
    let service = Arc::new(retirements(&setup));
    let s = service.clone();
    let first = tokio::task::spawn_blocking(move || s.begin("lab", "local", "lab:req-1", false))
        .await
        .unwrap();
    let RetirementStep::InUse(named) = first else {
        panic!("{first:?}")
    };
    assert_eq!(named.len(), 1);
    assert!(setup
        .owner
        .lock()
        .unwrap()
        .store()
        .profile_retirement("lab", "local")
        .unwrap()
        .is_none());
    let s = service.clone();
    let draining = tokio::task::spawn_blocking(move || s.begin("lab", "local", "lab:req-2", true))
        .await
        .unwrap();
    assert!(
        matches!(draining, RetirementStep::Draining(_)),
        "{draining:?}"
    );
    let confirmed = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let s = service.clone();
            if let Some(step) =
                tokio::task::spawn_blocking(move || s.poll("lab", "local", "lab:req-2"))
                    .await
                    .unwrap()
            {
                return step;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(confirmed, RetirementStep::Confirmed);
    {
        let store = setup.owner.lock().unwrap();
        assert_eq!(
            store
                .store()
                .profile_retirement("lab", "local")
                .unwrap()
                .unwrap()
                .1,
            "confirmed"
        );
        assert!(store
            .store()
            .runtime_binding(&id)
            .unwrap()
            .is_none_or(|b| b.state == "released"));
    }
    setup.worker.shutdown().await.unwrap();
}

// T32 (ADR 0018 §4, owner decision 2026-09-25): a drained retirement whose
// stop cannot settle (the host is offline) waits while the 900 s bound
// stands, then answers holding once it passes: unconfirmed, the retirement
// ended, and the runtime's accounting kept.
#[tokio::test]
async fn an_unsettled_retirement_holds_once_its_bound_passes() {
    let setup = setup().await;
    let id = setup.id.clone();
    start(&setup, &id).await;
    place_on_lab(&setup.dir, &setup.owner, &id);
    setup.online.store(false, Ordering::SeqCst);
    let clock = Arc::new(AtomicI64::new(1900));
    let reading = clock.clone();
    let service =
        Arc::new(retirements(&setup).with_clock(Arc::new(move || reading.load(Ordering::SeqCst))));
    let s = service.clone();
    let draining = tokio::task::spawn_blocking(move || s.begin("lab", "local", "lab:req-9", true))
        .await
        .unwrap();
    assert!(
        matches!(draining, RetirementStep::Draining(_)),
        "{draining:?}"
    );
    // Within the bound the unsettled stop keeps the retirement open.
    tokio::time::sleep(Duration::from_millis(200)).await;
    clock.fetch_add(899_000, Ordering::SeqCst);
    let s = service.clone();
    let waiting = tokio::task::spawn_blocking(move || s.poll("lab", "local", "lab:req-9"))
        .await
        .unwrap();
    assert_eq!(waiting, None);
    assert!(setup
        .owner
        .lock()
        .unwrap()
        .store()
        .profile_retirement("lab", "local")
        .unwrap()
        .is_some());
    // Past the bound: holding, naming the deployment; nothing confirmed.
    clock.fetch_add(1_000, Ordering::SeqCst);
    let s = service.clone();
    let held = tokio::task::spawn_blocking(move || s.poll("lab", "local", "lab:req-9"))
        .await
        .unwrap();
    let Some(RetirementStep::Holding(named)) = held else {
        panic!("{held:?}")
    };
    assert_eq!(named.len(), 1);
    {
        let store = setup.owner.lock().unwrap();
        assert!(store
            .store()
            .profile_retirement("lab", "local")
            .unwrap()
            .is_none());
        // Uncertainty keeps accounting: the runtime is still held.
        assert!(store
            .store()
            .runtime_binding(&id)
            .unwrap()
            .is_some_and(|b| b.state != "released"));
    }
    setup.worker.shutdown().await.unwrap();
}

// T07: the engines listing shows each host's published profiles with the
// derived custom mark and the deployments using each.
#[tokio::test]
async fn the_engines_listing_shows_published_profiles() {
    let setup = setup().await;
    let id = setup.id.clone();
    start(&setup, &id).await;
    place_on_lab(&setup.dir, &setup.owner, &id);
    let mut document: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut host = document["input"]["host"].take();
    host["name"] = json!("lab");
    host["state_dir"] = json!("/home/operator/.local/state/mllm");
    host["identity_dir"] = json!("/home/operator/.local/state/mllm/identity");
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("0.29.0+patched");
    {
        let owner = setup.owner.lock().unwrap();
        owner
            .store()
            .publish_host_configuration(&mllm_store::host_publication::HostPublication {
                host_id: "lab".into(),
                config_json: host.to_string(),
                boot_id: "boot".into(),
                fingerprint: mllm_config::remote_resources::policy_fingerprint(&host),
                received_at_ms: 1,
            })
            .unwrap();
    }
    let authority = Arc::new(mllm_controller::enrollment::EnrollmentAuthority::new(
        setup.owner.clone(),
        mllm_agent::identity::CertificateAuthority::generate(100).unwrap(),
    ));
    let router = mllm_management::hosts::hosts_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        setup.owner.clone(),
        mllm_controller::agent_sessions::AgentSessions::new(authority),
    );
    let request = Request::builder()
        .uri("/management/v1/engines")
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .body(Body::empty())
        .unwrap();
    let (status, listing) = body(router.oneshot(request).await.unwrap()).await;
    assert_eq!(status, 200, "{listing}");
    let row = &listing["engines"][0];
    assert_eq!(row["host"], "lab");
    assert_eq!(row["profile"], "local");
    assert_eq!(row["engine"], "vllm");
    assert_eq!(row["version"], "0.29.0+patched");
    assert_eq!(row["custom"], true);
    assert_eq!(row["published"], "published");
    assert_eq!(row["online"], false);
    assert_eq!(row["deployments"].as_array().unwrap().len(), 1);
    setup.worker.shutdown().await.unwrap();
}

// T16 T32 (controller ruling I1): a retried remove, under a new request key
// and without drain, resumes the draining retirement rather than being refused
// as a conflict or cancelling it; its poll follows the standing retirement to
// confirmation, and a later retry of the confirmed one confirms at once.
#[tokio::test]
async fn a_retried_retirement_resumes_under_the_standing_key() {
    let setup = setup().await;
    let id = setup.id.clone();
    start(&setup, &id).await;
    place_on_lab(&setup.dir, &setup.owner, &id);
    let service = Arc::new(retirements(&setup));
    let s = service.clone();
    let draining = tokio::task::spawn_blocking(move || s.begin("lab", "local", "lab:req-1", true))
        .await
        .unwrap();
    assert!(
        matches!(draining, RetirementStep::Draining(_)),
        "{draining:?}"
    );
    let s = service.clone();
    let retried = tokio::task::spawn_blocking(move || s.begin("lab", "local", "lab:req-2", false))
        .await
        .unwrap();
    assert!(
        matches!(retried, RetirementStep::Draining(_)),
        "{retried:?}"
    );
    let confirmed = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let s = service.clone();
            if let Some(step) =
                tokio::task::spawn_blocking(move || s.poll("lab", "local", "lab:req-2"))
                    .await
                    .unwrap()
            {
                return step;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(confirmed, RetirementStep::Confirmed);
    let s = service.clone();
    let again = tokio::task::spawn_blocking(move || s.begin("lab", "local", "lab:req-3", false))
        .await
        .unwrap();
    assert_eq!(again, RetirementStep::Confirmed);
    let (key, state, _) = setup
        .owner
        .lock()
        .unwrap()
        .store()
        .profile_retirement("lab", "local")
        .unwrap()
        .unwrap();
    assert_eq!((key.as_str(), state.as_str()), ("lab:req-1", "confirmed"));
    setup.worker.shutdown().await.unwrap();
}

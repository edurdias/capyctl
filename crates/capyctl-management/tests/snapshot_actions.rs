//! ADR 0040 §8: the snapshot's supported-action hints. Each deployment and
//! instance lists the actions the management API takes, whether each is
//! available in the current state and, when not, the code the action's
//! handler refuses it with. Only refusals that depend on state alone are
//! hinted; the handlers stay authoritative.
//!
//! Fake-engine tests through the real handlers in-process. They are not
//! qualification of any native engine recipe.
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
    actions::OwnedActionSource, configuration::SharedConfigurationSource, lifecycle_router,
    ManagementCredentials,
};
use capyctl_testkit::fixture;
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;

const MANAGEMENT: &str = "management-credential-012345678901234567890";
const INFERENCE: &str = "inference-credential-0123456789012345678901";
const DEPLOYMENT_ACTIONS: [&str; 5] = ["start", "stop", "park", "preinitialize", "delete"];
const INSTANCE_ACTIONS: [&str; 2] = ["start", "stop"];

/// Host observations that wait while the gate is closed, so a start can be
/// held before it arms (`queued`).
struct Gate {
    values: Vec<MemoryObservation>,
    open: tokio::sync::watch::Sender<bool>,
}
impl ServiceObservation for Gate {
    fn observe(&self, _: String) -> ObservationFuture {
        let values = self.values.clone();
        let mut open = self.open.subscribe();
        Box::pin(async move {
            let _ = open.wait_for(|open| *open).await;
            Ok(values)
        })
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    owner: Arc<Mutex<OwnedCoordinatorState>>,
    worker: OwnedCoordinator,
    gate: tokio::sync::watch::Sender<bool>,
    /// The stopped managed deployment `ordinary`, one instance.
    id: String,
    app: axum::Router,
}

impl Setup {
    async fn shutdown(self) {
        self.gate.send_replace(true);
        self.worker.shutdown().await.unwrap();
    }
}

async fn setup() -> Setup {
    let source = fixture::owned_source().await;
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let (gate, _) = tokio::sync::watch::channel(true);
    let worker = capyctl_testkit::spawn_fake_coordinator(
        owner.clone(),
        Arc::new(Gate {
            values: source.observations.clone(),
            open: gate.clone(),
        }),
        Arc::new(|| Ok::<_, CoordinatorError>(1900)),
        // A protocol bound short enough that a stop with a long deadline
        // drains for the whole `stop_drain_timeout` (see `stopping`).
        CoordinatorOptions {
            protocol_timeout: Duration::from_secs(7),
            terminate_grace: Duration::from_secs(1),
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
    let app = lifecycle_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(OwnedActionSource::new(configuration, worker.commands()).unwrap()),
    );
    Setup {
        _dir: dir,
        owner,
        worker,
        gate,
        id: source.fence.deployment_id.clone(),
        app,
    }
}

async fn body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 8 << 20).await.unwrap()).unwrap()
}

/// `path` is `<id>` or `<id>/instances/<n>`.
async fn send(s: &Setup, path: &str, key: &str, action: &str, deadline_ms: i64) -> (u16, Value) {
    let response = s
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/management/v1/deployments/{path}/actions"))
                .header("authorization", format!("Bearer {MANAGEMENT}"))
                .header("content-type", "application/json")
                .header("idempotency-key", key)
                .body(Body::from(
                    json!({"expected_revision":1,"action":action,"deadline_ms":deadline_ms})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    (response.status().as_u16(), body(response).await)
}

/// The deployment as `GET /management/v1/snapshot` answers it.
async fn deployment(s: &Setup) -> Value {
    let response = s
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/management/v1/snapshot")
                .header("authorization", format!("Bearer {MANAGEMENT}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let snapshot = body(response).await;
    snapshot["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == s.id.as_str())
        .unwrap()
        .clone()
}

/// `(action, reason)` in the order listed; `None` is available. Every entry
/// has a reason exactly when it is unavailable.
fn hints(actions: &Value) -> Vec<(String, Option<String>)> {
    actions
        .as_array()
        .unwrap()
        .iter()
        .map(|hint| {
            let reason = hint.get("reason").map(|r| r.as_str().unwrap().to_owned());
            assert_eq!(hint["available"], reason.is_none(), "{hint}");
            (hint["action"].as_str().unwrap().to_owned(), reason)
        })
        .collect()
}

fn expect(table: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
    table
        .iter()
        .map(|(action, reason)| ((*action).to_owned(), reason.map(str::to_owned)))
        .collect()
}

async fn wait_for(s: &Setup, state: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let observed = {
                let owner = s.owner.lock().unwrap();
                let snapshot = owner.store().snapshot().unwrap();
                let d = snapshot.deployments.iter().find(|d| d.id == s.id).unwrap();
                d.observed_state.clone()
            };
            if observed == state {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the deployment reaches {state}"));
}

/// A fresh setup with the deployment held in `state`.
async fn in_state(state: &str) -> Setup {
    let s = setup().await;
    match state {
        "stopped" => {}
        "ready" => {
            let (status, start) = send(&s, &s.id, "to-ready", "start", 10000).await;
            assert_eq!(status, 202, "{start}");
            wait_for(&s, "ready").await;
        }
        // A start accepted and held before it arms.
        "queued" => {
            s.gate.send_replace(false);
            let (status, start) = send(&s, &s.id, "to-queued", "start", 10000).await;
            assert_eq!(status, 202, "{start}");
            wait_for(&s, "queued").await;
        }
        // SPEC §6.3: a stop drains the requests already accepted before it
        // terminates. One request in flight holds the stop in its drain.
        "stopping" => {
            let (status, start) = send(&s, &s.id, "to-ready", "start", 10000).await;
            assert_eq!(status, 202, "{start}");
            wait_for(&s, "ready").await;
            {
                let owner = s.owner.lock().unwrap();
                let granted = owner
                    .store()
                    .apply_request_lease_batch(
                        owner.session(),
                        &[capyctl_store::dispatch::LeaseWrite::Grant {
                            deployment_id: s.id.clone(),
                            max_per_deployment: 8,
                            max_total: 8,
                        }],
                    )
                    .unwrap();
                assert!(granted[0].is_ok(), "{granted:?}");
            }
            let (status, stop) = send(&s, &s.id, "to-stopping", "stop", 60000).await;
            assert_eq!(status, 202, "{stop}");
            wait_for(&s, "stopping").await;
        }
        other => unreachable!("{other}"),
    }
    s
}

// ADR 0040 §8. SPEC §6.3: an operator's Stop of a deployment that holds
// nothing is recorded (it suspends automatic activation), so a stopped
// deployment offers it; it has nothing Ready to park.
#[tokio::test]
async fn a_stopped_deployment_offers_start_stop_preinitialize_and_delete_but_not_park() {
    let s = in_state("stopped").await;
    let d = deployment(&s).await;
    assert_eq!(d["observed_state"], "stopped");
    assert_eq!(
        hints(&d["actions"]),
        expect(&[
            ("start", None),
            ("stop", None),
            ("park", Some("lifecycle_conflict")),
            ("preinitialize", None),
            ("delete", None),
        ])
    );
    assert_eq!(
        hints(&d["instances"][0]["actions"]),
        expect(&[("start", None), ("stop", None)])
    );
    s.shutdown().await;
}

// ADR 0040 §8: a Ready deployment holds a runtime, so a start has nothing to
// start and a delete waits for verified cleanup; both carry the code their
// handler answers.
#[tokio::test]
async fn a_ready_deployment_offers_stop_and_park_and_refuses_delete_until_cleanup() {
    let s = in_state("ready").await;
    let d = deployment(&s).await;
    assert_eq!(d["observed_state"], "ready");
    assert_eq!(
        hints(&d["actions"]),
        expect(&[
            ("start", Some("runtime_retained")),
            ("stop", None),
            ("park", None),
            ("preinitialize", None),
            ("delete", Some("delete_requires_cleanup")),
        ])
    );
    assert_eq!(
        hints(&d["instances"][0]["actions"]),
        expect(&[("start", Some("runtime_retained")), ("stop", None)])
    );
    s.shutdown().await;
}

// ADR 0040 §8: while a stop is in flight no lifecycle action that changes
// what runs is available. Preinitialize is accepted in any state (SPEC §6.5:
// it waits while work is in flight), so it stays offered.
#[tokio::test]
async fn a_stopping_deployment_offers_no_lifecycle_action_but_preinitialize() {
    let s = in_state("stopping").await;
    let d = deployment(&s).await;
    assert_eq!(d["observed_state"], "stopping");
    assert_eq!(
        hints(&d["actions"]),
        expect(&[
            ("start", Some("runtime_retained")),
            ("stop", Some("lifecycle_conflict")),
            ("park", Some("lifecycle_conflict")),
            ("preinitialize", None),
            ("delete", Some("delete_requires_cleanup")),
        ])
    );
    assert_eq!(
        hints(&d["instances"][0]["actions"]),
        expect(&[
            ("start", Some("runtime_retained")),
            ("stop", Some("reconciliation_required")),
        ])
    );
    s.shutdown().await;
}

// ADR 0040 §8: suspend and resume get no hints, and a deployment marked
// suspended reads as stopped, actions included.
#[tokio::test]
async fn a_suspended_deployment_offers_what_a_stopped_one_does() {
    let s = in_state("stopped").await;
    let stopped = deployment(&s).await;
    s.owner
        .lock()
        .unwrap()
        .store()
        .set_suspended(&s.id, true)
        .unwrap();
    let suspended = deployment(&s).await;
    assert_eq!(suspended["suspended"], true);
    assert_eq!(suspended["observed_state"], "stopped");
    assert_eq!(suspended["actions"], stopped["actions"]);
    assert_eq!(
        suspended["instances"][0]["actions"],
        stopped["instances"][0]["actions"]
    );
    let listed: Vec<String> = hints(&suspended["actions"])
        .into_iter()
        .map(|(action, _)| action)
        .collect();
    assert_eq!(listed, DEPLOYMENT_ACTIONS);
    s.shutdown().await;
}

// ADR 0040 §8: parity with the handlers, in both directions, for every state
// the Fake reaches in-process. An action hinted available is accepted; an
// action hinted unavailable is refused with the hinted code. Each action runs
// against its own fresh copy of the state, since an accepted one changes it.
#[tokio::test]
async fn hints_agree_with_the_handlers_in_both_directions() {
    for state in ["stopped", "ready", "queued", "stopping"] {
        let targets = DEPLOYMENT_ACTIONS
            .iter()
            .map(|action| (false, *action))
            .chain(INSTANCE_ACTIONS.iter().map(|action| (true, *action)));
        for (instance, action) in targets {
            let s = in_state(state).await;
            let d = deployment(&s).await;
            assert_eq!(d["observed_state"], state);
            let (actions, path) = if instance {
                (
                    &d["instances"][0]["actions"],
                    format!("{}/instances/0", s.id),
                )
            } else {
                (&d["actions"], s.id.clone())
            };
            let hinted = hints(actions)
                .into_iter()
                .find(|(name, _)| name == action)
                .unwrap_or_else(|| panic!("{state}: {action} is hinted"))
                .1;
            let (status, answer) = send(&s, &path, "parity", action, 10000).await;
            let target = if instance { "instance" } else { "deployment" };
            match hinted {
                None => assert!(
                    (200..300).contains(&status),
                    "{state} {target} {action}: hinted available, answered {status} {answer}"
                ),
                Some(reason) => assert_eq!(
                    answer["error"]["code"],
                    reason.as_str(),
                    "{state} {target} {action}: hinted {reason}, answered {status} {answer}"
                ),
            }
            s.shutdown().await;
        }
    }
}

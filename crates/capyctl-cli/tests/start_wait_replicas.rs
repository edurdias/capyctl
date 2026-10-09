//! Owner decision 2026-09-25: `start deployment --wait` waits for every
//! instance of the deployment, not only the one whose operation the receipt
//! names, and never reports success on a partial start.
//!
//! The shipped binary runs against a scripted management API (the status a
//! real server reports for a two-instance start whose second instance was not
//! placed before its deadline). CPU only; nothing here is live-proven.
mod support;

use axum::{routing, Json, Router};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const TOKEN: &str = "management-credential-012345678901234567890";
const DEPLOYMENT: &str = "01M3B5MCNEHBGFEXHFXTVY9E7X";
const OPERATION: &str = "01M3B5MCV15WJM758EB5P30N1V";

/// The status of a two-instance deployment: instance 0 Ready (its start's
/// operation succeeded), instance 1 as `second` says.
fn snapshot(second: Value) -> Value {
    let mut one = json!({"index": 1, "lifecycle": "active", "operator_stopped": false});
    for (key, value) in second.as_object().unwrap() {
        one[key] = value.clone();
    }
    json!({
        "deployments": [{
            "id": DEPLOYMENT,
            "name": "pair",
            "revision": "1",
            "timeouts": {"request_deadline_ms": 60_000, "initialize_ms": 3_000, "stop_ms": 3_000},
            "instances": [
                {"index": 0, "lifecycle": "active", "observed_state": "ready", "operator_stopped": false},
                one
            ]
        }],
        "operations": [{"id": OPERATION, "state": "succeeded"}]
    })
}

/// Serve the scripted management API; `second(read)` is instance 1 on the
/// `read`-th status read.
async fn management(second: fn(usize) -> Value) -> std::net::SocketAddr {
    let reads = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/management/v1/snapshot",
            routing::get(move || {
                let read = reads.fetch_add(1, Ordering::SeqCst);
                async move { Json(snapshot(second(read))) }
            }),
        )
        .route(
            &format!("/management/v1/deployments/{DEPLOYMENT}/actions"),
            routing::post(|| async {
                (
                    axum::http::StatusCode::ACCEPTED,
                    Json(json!({"api_version": "1", "operation_id": OPERATION,
                        "deployment_id": DEPLOYMENT, "joined": false, "revision": "1"})),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

fn state_dir() -> tempfile::TempDir {
    let dir = support::safe_state_dir();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let identity = dir.path().join("identity");
    std::fs::create_dir(&identity).unwrap();
    std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        identity.join("credentials"),
        format!("admin_token: {TOKEN}\n"),
    )
    .unwrap();
    dir
}

async fn start_wait(second: fn(usize) -> Value) -> std::process::Output {
    let address = management(second).await;
    let dir = state_dir();
    tokio::task::spawn_blocking(move || {
        support::capyctl()
            .env("CAPYCTL_STATE_DIR", dir.path())
            .env(capyctl_cli::roles::MANAGEMENT_ADDR_ENV, address.to_string())
            .args(["start", "deployment", "pair", "--wait", "--format", "json"])
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

// T08 T20 (owner decision 2026-09-25): the start's own operation (instance 0)
// succeeded, but instance 1 was never placed and its start deadline passed.
// Before, `--wait` returned success here; now it fails with
// `insufficient_resources` (exit 4) and names the instance and its reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_fails_when_a_replica_is_never_placed() {
    let output = start_wait(|read| {
        if read < 3 {
            json!({"observed_state": "queued", "last_error": "placement: insufficient_capacity"})
        } else {
            json!({"observed_state": "stopped",
                   "last_error": "placement: insufficient_capacity; start deadline passed"})
        }
    })
    .await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(4), "{stdout}\n{stderr}");
    let said = format!("{stdout}{stderr}");
    assert!(said.contains("insufficient_resources"), "{said}");
    assert!(said.contains("Instance 1"), "{said}");
    assert!(said.contains("start deadline passed"), "{said}");
}

// T08 (owner decision 2026-09-25): `--wait` succeeds once every instance has
// been Ready, and not before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_succeeds_once_every_replica_is_ready() {
    let output = start_wait(|read| {
        if read < 3 {
            json!({"observed_state": "starting"})
        } else {
            json!({"observed_state": "ready"})
        }
    })
    .await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let instances = result["deployment"]["instances"].as_array().unwrap();
    assert!(
        instances.iter().all(|i| i["observed_state"] == "ready"),
        "{result}"
    );
}

// Found live 2026-10-04: a launch refused before anything started (an SGLang
// sizing refusal, a TensorFold memory cap) gives up and closes the instance's
// admission while its operation stays pending until the Initialize deadline.
// `--wait` ends at once with the refusal status shows, not after the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_ends_at_once_when_the_start_gave_up() {
    const REFUSAL: &str = "gave up: launch refused: SGLang keeps 1 bytes of recurrent state";
    let (output, began) = wait_for_given_up(REFUSAL).await;
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        began.elapsed() < std::time::Duration::from_secs(20),
        "{said}"
    );
    assert!(!output.status.success(), "{said}");
    assert!(said.contains(REFUSAL), "{said}");
}

// T29 (found live 2026-10-09): a start that gave up because the host's
// available memory, less its charge, would not leave the free reserve ends
// `insufficient_resources` (exit 4), as a capacity block does, and the JSON
// error carries the four figures: available, the charge, the reserve and the
// shortfall.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_refused_on_available_memory_gives_its_figures() {
    const REFUSAL: &str = "gave up: resource or evidence check failed: insufficient_memory: \
        needs 109.2 GiB of system memory, 118.2 GiB available and a 11.0 GiB free reserve to \
        keep, 2.1 GiB short";
    let (output, _) = wait_for_given_up(REFUSAL).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(4), "{stdout}\n{stderr}");
    let said = format!("{stdout}{stderr}");
    let error: Value = said
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .unwrap_or_else(|| panic!("no JSON error: {said}"));
    assert_eq!(error["code"], "insufficient_resources", "{error}");
    let message = error["message"].as_str().unwrap();
    for figure in [
        "needs 109.2 GiB",
        "118.2 GiB available",
        "11.0 GiB free reserve",
        "2.1 GiB short",
    ] {
        assert!(message.contains(figure), "{figure}: {message}");
    }
}

/// `start deployment pair --wait --format json` against a server whose start
/// operation gave up with `reason`; the output and when the wait began.
async fn wait_for_given_up(reason: &'static str) -> (std::process::Output, std::time::Instant) {
    let latest = json!({"id": OPERATION, "kind": "initialize", "state": "pending",
        "reason": reason, "given_up": true});
    let app = Router::new()
        .route(
            "/management/v1/snapshot",
            routing::get(move || {
                let latest = latest.clone();
                async move {
                    Json(json!({
                        "deployments": [{
                            "id": DEPLOYMENT, "name": "pair", "revision": "1",
                            "timeouts": {"request_deadline_ms": 600_000,
                                         "initialize_ms": 300_000, "stop_ms": 3_000},
                            "latest_operation": latest,
                            "instances": [{"index": 0, "lifecycle": "active",
                                "observed_state": "queued", "operator_stopped": false,
                                "latest_operation": latest}]
                        }],
                        "operations": [{"id": OPERATION, "state": "pending"}]
                    }))
                }
            }),
        )
        .route(
            &format!("/management/v1/deployments/{DEPLOYMENT}/actions"),
            routing::post(|| async {
                (
                    axum::http::StatusCode::ACCEPTED,
                    Json(json!({"api_version": "1", "operation_id": OPERATION,
                        "deployment_id": DEPLOYMENT, "joined": false, "revision": "1"})),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = state_dir();
    let began = std::time::Instant::now();
    let output = tokio::task::spawn_blocking(move || {
        support::capyctl()
            .env("CAPYCTL_STATE_DIR", dir.path())
            .env(capyctl_cli::roles::MANAGEMENT_ADDR_ENV, address.to_string())
            .args(["start", "deployment", "pair", "--wait", "--format", "json"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    (output, began)
}

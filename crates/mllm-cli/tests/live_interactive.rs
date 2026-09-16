//! Throwaway localhost-only interactive Spark lab; not a product API.

/// A state directory the controller lock will accept.
///
/// The lock walks every ancestor of the state path and refuses any that is group- or
/// other-writable, because such an ancestor lets another account replace the
/// directory the lock guards. `/tmp` is 1777 and a checkout is commonly 0775, so
/// neither can hold controller state. The home directory is the usual root that
/// satisfies the rule.
fn safe_state_dir() -> tempfile::TempDir {
    let home = std::env::var("HOME").expect("HOME is set");
    tempfile::TempDir::new_in(home).expect("a state directory under an owner-only root")
}

use mllm_controller::LifecyclePort as _;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use mllm_cli::roles;
use mllm_controller::{CoordinatorLifecycle, DeployRequest};
use mllm_domain::{LifecycleAction, LifecycleState};
use serde_json::{json, Value};
use std::{io::Write, os::unix::fs::OpenOptionsExt, sync::Arc, time::Instant};
use tokio::sync::{Mutex, Notify};

fn action(name: &str) -> Option<LifecycleAction> {
    match name {
        "park" => Some(LifecycleAction::Park),
        "wake" => Some(LifecycleAction::Start),
        "stop" => Some(LifecycleAction::Stop),
        _ => None,
    }
}

#[derive(Clone)]
struct Lab {
    controller: Arc<CoordinatorLifecycle>,
    id: String,
    key: String,
    gate: Arc<Mutex<()>>,
    shutdown: Arc<Notify>,
}

fn authorized(headers: &HeaderMap, key: &str) -> bool {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| h.strip_prefix("Bearer ") == Some(key))
}

async fn status(State(lab): State<Lab>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    if !authorized(&headers, &lab.key) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        );
    }
    let state = lab
        .controller
        .get_deployment(&lab.id)
        .unwrap()
        .unwrap()
        .observed_state;
    (
        StatusCode::OK,
        Json(json!({"deployment":lab.id,"state":format!("{state:?}"),
        "engine_pid":lab.controller.live_identities(&lab.id).ok().and_then(|ids| ids.into_iter().find(|i| i.role == "api").map(|i| i.pid)),"scope":"single-model diagnostic lab"})),
    )
}

async fn transition(
    State(lab): State<Lab>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    if !authorized(&headers, &lab.key) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        );
    }
    let Some(action) = action(&name) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"unknown action"})),
        );
    };
    let Ok(_guard) = lab.gate.try_lock() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"lifecycle operation in progress"})),
        );
    };
    let started = Instant::now();
    let result = async {
        let op = lab.controller.request_transition(&lab.id, action).await?;
        lab.controller.wait_terminal(&op).await
    }
    .await;
    match result {
        Ok(state) => {
            let event = json!({"action":name,"state":format!("{state:?}"),"seconds":started.elapsed().as_secs_f64()});
            eprintln!("LAB-EVENT: {event}");
            if name == "stop" {
                lab.shutdown.notify_one();
            }
            (StatusCode::OK, Json(event))
        }
        Err(error) => (
            StatusCode::CONFLICT,
            Json(json!({"error":error.to_string(),"action":name})),
        ),
    }
}

#[tokio::test]
async fn live_interactive() {
    if std::env::var("MLLM_INTERACTIVE").ok().as_deref() != Some("1") {
        return;
    }
    let dir = std::path::PathBuf::from(
        std::env::var("MLLM_INTERACTIVE_DIR").expect("supervised run directory"),
    );
    let profile = roles::LiveVllmProfile::from_env().expect("live profile required");
    // Reserve the router port before allocating model memory.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8443")
        .await
        .unwrap();
    let app = roles::start_standalone_with_policy(
        &dir,
        mllm_adapters::fake::ParkPolicy::ExperimentalAllowed,
    )
    .await
    .unwrap();
    let id = app
        .controller
        .submit_deploy("standalone", &DeployRequest {
            name: "interactive-lab".into(),
            kind: "vllm-sleep".into(),
            manifest: br#"{"kind":"model","name":"interactive-lab"}"#.to_vec(),
            route_model_id: Some(profile.model_id.clone()),
        })
        .unwrap();
    let started = Instant::now();
    let op = app
        .controller
        .request_transition(&id, LifecycleAction::Start)
        .await
        .unwrap();
    assert_eq!(
        app.controller.wait_terminal(&op).await.unwrap(),
        LifecycleState::Ready
    );
    let shutdown = Arc::new(Notify::new());
    let lab = Lab {
        controller: app.controller.clone(),
        id,
        key: app.api_key().to_owned(),
        gate: Arc::new(Mutex::new(())),
        shutdown: shutdown.clone(),
    };
    let controls = Router::new()
        .route("/lab/status", get(status))
        .route("/lab/{action}", post(transition))
        .with_state(lab);
    let mut session = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join("session.json"))
        .unwrap();
    write!(
        session,
        "{}",
        json!({"url":"http://127.0.0.1:8443","model":profile.model_id,"api_key":app.api_key()})
    )
    .unwrap();
    eprintln!(
        "LAB-READY: startup_seconds={} session={}",
        started.elapsed().as_secs_f64(),
        dir.join("session.json").display()
    );
    axum::serve(listener, app.router().merge(controls))
        .with_graceful_shutdown(async move { shutdown.notified().await })
        .await
        .unwrap();
}

#[test]
fn lab_actions_are_explicit_and_allowlisted() {
    assert!(matches!(action("park"), Some(LifecycleAction::Park)));
    assert!(matches!(action("wake"), Some(LifecycleAction::Start)));
    assert!(matches!(action("stop"), Some(LifecycleAction::Stop)));
    assert!(action("restart").is_none());
    assert!(action("../stop").is_none());
}

#[test]
fn lab_auth_rejects_missing_wrong_and_malformed_credentials() {
    let mut headers = HeaderMap::new();
    assert!(!authorized(&headers, "test-key"));
    for value in ["Bearer wrong", "test-key", "Basic test-key"] {
        headers.insert("authorization", value.parse().unwrap());
        assert!(!authorized(&headers, "test-key"));
    }
    headers.insert("authorization", "Bearer test-key".parse().unwrap());
    assert!(authorized(&headers, "test-key"));
}

#[tokio::test]
async fn lab_http_auth_status_and_busy_controls() {
    let dir = safe_state_dir();
    let app = roles::start_standalone(dir.path()).await.unwrap();
    let id = app
        .controller
        .submit_deploy("standalone", &DeployRequest {
            name: "lab-http-test".into(),
            kind: "model".into(),
            manifest: br#"{"kind":"model","name":"lab-http-test"}"#.to_vec(),
            route_model_id: Some("lab-http-test".into()),
        })
        .unwrap();
    let gate = Arc::new(Mutex::new(()));
    let lab = Lab {
        controller: app.controller.clone(),
        id: id.clone(),
        key: app.api_key().into(),
        gate: gate.clone(),
        shutdown: Arc::new(Notify::new()),
    };
    let router = Router::new()
        .route("/lab/status", get(status))
        .route("/lab/{action}", post(transition))
        .with_state(lab);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .post(format!("{url}/lab/park"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response: Value = client
        .get(format!("{url}/lab/status"))
        .bearer_auth(app.api_key())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["deployment"], id);
    assert_eq!(
        client
            .post(format!("{url}/lab/invalid"))
            .bearer_auth(app.api_key())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let _busy = gate.lock().await;
    assert_eq!(
        client
            .post(format!("{url}/lab/park"))
            .bearer_auth(app.api_key())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    server.abort();
}

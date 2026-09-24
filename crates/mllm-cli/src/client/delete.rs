//! `mllm delete deployment <name|id> --stop` (SPEC §6.3, plan unit W6, owner
//! decision 2026-09-23).
//!
//! SPEC §6.3: delete removes the route and the deployment "after authorized
//! cleanup". The server accepts a delete only once nothing is held, and never
//! stops anything itself. `--stop` is the operator's convenience on top of
//! that: the CLI issues the Stop of every instance, waits until each has
//! settled with verified cleanup, and then sends the ordinary delete, which the
//! server checks again. Nothing is released on the CLI's say-so (AGENTS.md:
//! uncertainty keeps its accounting).
//!
//! The Stop is the operator's (administrative) stop, the management `stop`
//! action. An ordinary stop would leave the deployment eligible for on-demand
//! activation, so an inference request arriving between the Stop and the
//! delete could start it again and the delete would be refused. Suspending
//! activation costs nothing here: the deployment is about to be removed, and
//! if the delete stays pending the operator has already asked for it to be gone.
//!
//! Durability (SPEC §13): the Stop and the delete are two steps of one request
//! journal, each committed before it is sent, so a rerun with the same
//! `--request-id` replays the same Stop (same key, body and deadline) and
//! resumes waiting, and a delete that was accepted replays its receipt. A step
//! the server definitively refused holds no accepted command, so it is dropped
//! and a rerun commits a fresh one.
//!
//! When cleanup cannot be proven yet (an instance's host is offline, the Stop
//! window passed, or the server still finds something held), the command
//! reports `cleanup: "pending"` with the operation ids and leaves the
//! deployment intact, as a drain of an offline host does (owner decision 4,
//! 2026-09-22). The Stops stay durable and complete on the host's evidence.
use super::{deployment, error, refusal, window, Management, WAIT_MARGIN_MS};
use crate::output::StructuredError;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::time::Duration;

const POLL: Duration = Duration::from_millis(250);

fn now_ms() -> Result<i64, StructuredError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|now| i64::try_from(now.as_millis()).ok())
        .ok_or_else(|| error("internal", "System clock unavailable"))
}

fn code(value: &Value) -> &str {
    value["error"]["code"].as_str().unwrap_or("")
}

/// A deployment's action path; the deployment id is its second segment.
fn deployment_of(path: &str) -> Result<String, StructuredError> {
    path.strip_prefix("/deployments/")
        .and_then(|rest| rest.strip_suffix("/actions"))
        .map(str::to_owned)
        .ok_or_else(|| error("invalid_config", "Saved request is not a deployment action"))
}

/// A fresh deployment action body against the deployment's current revision.
fn action_body(deployment: &Value, action: &str) -> Result<(String, Value), StructuredError> {
    let id = deployment["id"]
        .as_str()
        .ok_or_else(|| error("internal", "Missing deployment identity"))?;
    let revision: i64 = deployment["revision"]
        .as_str()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| error("internal", "Invalid deployment revision"))?;
    let deadline = now_ms()?
        .checked_add(window(deployment, action, None)?)
        .ok_or_else(|| error("internal", "System clock unavailable"))?;
    Ok((
        format!("/deployments/{id}/actions"),
        json!({"action": action, "expected_revision": revision, "deadline_ms": deadline}),
    ))
}

enum Settled {
    /// No operation of the deployment is still open.
    Quiet,
    /// Cleanup is not proven yet; the host state is reported.
    Pending { host_state: &'static str },
}

pub(super) async fn delete_after_stop(
    api: &Management,
    id: &str,
) -> Result<Value, StructuredError> {
    let journal = api
        .journal
        .as_ref()
        .ok_or_else(|| error("internal", "Mutation has no request identity"))?;
    let request_id = journal.id().to_owned();

    // A delete already sent is the last step: replay it. An accepted one
    // answers its receipt even though the deployment is gone (T09).
    if let Some(saved) = journal.saved("delete")? {
        let (status, value) = api
            .exchange(Method::POST, &saved.path, Some(("delete", saved.body)))
            .await?;
        if status.is_success() {
            return Ok(deleted(value));
        }
        if code(&value) != "delete_requires_cleanup" {
            return Err(refusal(status, &value));
        }
        journal.forget("delete")?;
    }

    // The Stop: the saved one replays exactly; otherwise a fresh one.
    let saved = journal.saved("stop")?;
    let (path, body) = match saved {
        Some(saved) => (saved.path, saved.body),
        None => action_body(deployment(&api.snapshot().await?, id)?, "stop")?,
    };
    let deployment_id = deployment_of(&path)?;
    let stop_deadline = body["deadline_ms"].as_i64().unwrap_or(0);
    let (status, value) = api
        .exchange(Method::POST, &path, Some(("stop", body)))
        .await?;
    let stop_operation = if status.is_success() {
        Some(
            value["operation_id"]
                .as_str()
                .ok_or_else(|| error("internal", "Missing accepted operation identity"))?
                .to_owned(),
        )
    } else if status == StatusCode::CONFLICT && code(&value) == "lifecycle_conflict" {
        // SPEC §6.3: a deployment holding no runtime has nothing to stop. The
        // refusal accepted nothing, so a rerun decides afresh.
        journal.forget("stop")?;
        None
    } else {
        return Err(refusal(status, &value));
    };

    // Wait until every operation of the deployment has settled. A succeeded
    // Stop committed its cleanup against evidence the processes are gone.
    let remaining = stop_deadline
        .saturating_sub(now_ms()?)
        .saturating_add(WAIT_MARGIN_MS)
        .max(WAIT_MARGIN_MS);
    let wait_until =
        tokio::time::Instant::now() + Duration::from_millis(u64::try_from(remaining).unwrap_or(0));
    let settled = loop {
        let snapshot = api.snapshot().await?;
        let current = deployment(&snapshot, &deployment_id)?;
        if open_operations(&snapshot, &deployment_id).is_empty() {
            break Settled::Quiet;
        }
        if !offline_hosts(api, current).await.is_empty() {
            break Settled::Pending {
                host_state: "offline",
            };
        }
        if tokio::time::Instant::now() >= wait_until {
            break Settled::Pending {
                host_state: "online",
            };
        }
        tokio::time::sleep(POLL).await;
    };
    if let Settled::Pending { host_state } = settled {
        let snapshot = api.snapshot().await?;
        return Ok(pending(
            &snapshot,
            &deployment_id,
            stop_operation.as_deref(),
            host_state,
            &request_id,
        ));
    }

    // Everything settled: the ordinary delete, which the server checks again.
    let snapshot = api.snapshot().await?;
    let (path, body) = action_body(deployment(&snapshot, &deployment_id)?, "delete")?;
    let (status, value) = api
        .exchange(Method::POST, &path, Some(("delete", body)))
        .await?;
    if status.is_success() {
        return Ok(deleted(value));
    }
    if code(&value) != "delete_requires_cleanup" {
        return Err(refusal(status, &value));
    }
    // Refused: nothing was accepted under the key.
    journal.forget("delete")?;
    let snapshot = api.snapshot().await?;
    let unsettled = stop_operation
        .as_deref()
        .is_some_and(|op| operation_state(&snapshot, op) != Some("succeeded"))
        || !open_operations(&snapshot, &deployment_id).is_empty();
    if unsettled {
        Ok(pending(
            &snapshot,
            &deployment_id,
            stop_operation.as_deref(),
            "online",
            &request_id,
        ))
    } else {
        // Nothing is left to wait for, yet something is still held.
        Err(refusal(status, &value))
    }
}

fn deleted(mut receipt: Value) -> Value {
    receipt["deleted"] = json!(true);
    receipt
}

fn operation_state<'a>(snapshot: &'a Value, operation: &str) -> Option<&'a str> {
    snapshot["operations"]
        .as_array()?
        .iter()
        .find(|op| op["id"] == operation)?["state"]
        .as_str()
}

fn open_operations(snapshot: &Value, deployment_id: &str) -> Vec<Value> {
    snapshot["operations"]
        .as_array()
        .map(|ops| {
            ops.iter()
                .filter(|op| {
                    op["deployment_id"] == deployment_id
                        && matches!(op["state"].as_str(), Some("pending" | "running"))
                })
                .map(|op| json!({"operation_id": op["id"], "action": op["action"], "state": op["state"]}))
                .collect()
        })
        .unwrap_or_default()
}

/// Hosts of the deployment's unstopped instances that the server reports
/// offline. A server without a host inventory (the standalone role, whose
/// host is embedded) has none.
async fn offline_hosts(api: &Management, deployment: &Value) -> Vec<String> {
    let hosts: Vec<&str> = deployment["instances"]
        .as_array()
        .map(|instances| {
            instances
                .iter()
                .filter(|instance| instance["observed_state"] != "stopped")
                .filter_map(|instance| instance["host_id"].as_str())
                .collect()
        })
        .unwrap_or_default();
    if hosts.is_empty() {
        return Vec::new();
    }
    let Ok((status, inventory)) = api.exchange(Method::GET, "/hosts", None).await else {
        return Vec::new();
    };
    if !status.is_success() {
        return Vec::new();
    }
    let Some(inventory) = inventory["hosts"].as_array() else {
        return Vec::new();
    };
    let mut offline: Vec<String> = hosts
        .into_iter()
        .filter(|host| {
            inventory
                .iter()
                .any(|entry| entry["host_id"] == *host && entry["online"] == false)
        })
        .map(str::to_owned)
        .collect();
    offline.sort();
    offline.dedup();
    offline
}

fn pending(
    snapshot: &Value,
    deployment_id: &str,
    stop_operation: Option<&str>,
    host_state: &str,
    request_id: &str,
) -> Value {
    let mut operations = open_operations(snapshot, deployment_id);
    if let Some(stop) = stop_operation {
        if !operations.iter().any(|op| op["operation_id"] == stop) {
            operations.insert(
                0,
                json!({"operation_id": stop, "action": "stop", "state": operation_state(snapshot, stop)}),
            );
        }
    }
    json!({
        "deleted": false,
        "cleanup": "pending",
        "deployment_id": deployment_id,
        "host_state": host_state,
        "request_id": request_id,
        "operations": operations,
        "message": format!(
            "Cleanup is not verified yet; the deployment is intact. Rerun with --request-id {request_id} to resume"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_journal::RequestJournal;
    use axum::{
        extract::State,
        http::HeaderMap,
        routing::{get, post},
        Json, Router,
    };
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{Arc, Mutex},
    };

    const DEPLOYMENT: &str = "01JAAAAAAAAAAAAAAAAAAAAAAA";

    /// A management API for one deployment on host `h1`.
    #[derive(Default)]
    struct Mock {
        host_online: bool,
        /// Whether the Stop's operation has settled with verified cleanup.
        stop_settled: bool,
        /// Whether the deployment holds anything a Stop acts on.
        running: bool,
        deleted: bool,
        /// Every action received: (idempotency key, body).
        actions: Vec<(String, Value)>,
    }

    type Shared = Arc<Mutex<Mock>>;

    async fn snapshot(State(mock): State<Shared>) -> Json<Value> {
        let mock = mock.lock().unwrap();
        let stop_state = if mock.stop_settled {
            "succeeded"
        } else {
            "running"
        };
        let stopped = mock.actions.iter().any(|(_, b)| b["action"] == "stop");
        let deployments = if mock.deleted {
            json!([])
        } else {
            json!([{
                "id": DEPLOYMENT, "name": "model", "revision": "1",
                "timeouts": {"stop_ms": 600_000, "initialize_ms": 600_000, "request_deadline_ms": 900_000},
                "instances": [{"index": 0, "host_id": "h1",
                    "observed_state": if mock.stop_settled || !mock.running { "stopped" } else { "ready" }}],
            }])
        };
        let operations = if stopped && mock.running {
            json!([{"id": "op-stop", "deployment_id": DEPLOYMENT, "action": "stop", "state": stop_state}])
        } else {
            json!([])
        };
        Json(json!({"deployments": deployments, "operations": operations}))
    }

    async fn hosts(State(mock): State<Shared>) -> Json<Value> {
        let online = mock.lock().unwrap().host_online;
        Json(json!({"hosts": [{"host_id": "h1", "online": online}]}))
    }

    async fn action(
        State(mock): State<Shared>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (axum::http::StatusCode, Json<Value>) {
        let mut mock = mock.lock().unwrap();
        let key = headers["idempotency-key"].to_str().unwrap().to_owned();
        mock.actions.push((key, body.clone()));
        let conflict = |code: &str| {
            (
                axum::http::StatusCode::CONFLICT,
                Json(json!({"error": {"code": code, "message": code}})),
            )
        };
        match body["action"].as_str() {
            Some("stop") if !mock.running => conflict("lifecycle_conflict"),
            Some("stop") => (
                axum::http::StatusCode::ACCEPTED,
                Json(
                    json!({"operation_id": "op-stop", "deployment_id": DEPLOYMENT, "revision": "1"}),
                ),
            ),
            Some("delete") if mock.running && !mock.stop_settled => {
                conflict("delete_requires_cleanup")
            }
            Some("delete") => {
                mock.deleted = true;
                (
                    axum::http::StatusCode::ACCEPTED,
                    Json(
                        json!({"operation_id": "op-delete", "deployment_id": DEPLOYMENT, "revision": "1", "joined": false}),
                    ),
                )
            }
            _ => conflict("invalid_request"),
        }
    }

    async fn serve(mock: Shared) -> String {
        let app = Router::new()
            .route("/management/v1/snapshot", get(snapshot))
            .route("/management/v1/hosts", get(hosts))
            .route(
                &format!("/management/v1/deployments/{DEPLOYMENT}/actions"),
                post(action),
            )
            .with_state(mock);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}/management/v1")
    }

    fn state_dir() -> tempfile::TempDir {
        let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    /// One CLI invocation: a new process's client and its reopened journal.
    async fn run(endpoint: &str, root: &std::path::Path, request: &str) -> Value {
        let journal = RequestJournal::open(
            root,
            request,
            endpoint,
            "digest".into(),
            json!({"command": "delete", "deployment": "model", "stop": true}),
        )
        .unwrap();
        let api = Management {
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            token: "token".into(),
            endpoint: endpoint.into(),
            journal: Some(journal),
            initialize_timeout_ms: None,
            evict: false,
            wait_start: false,
        };
        delete_after_stop(&api, "model").await.unwrap()
    }

    // T10 T32: an instance's host is offline, so the Stop cannot be proven:
    // the command returns at once with cleanup pending, the Stop's operation
    // id, and the deployment intact (no delete was sent).
    // T09: a rerun with the same request id replays the same Stop (same key,
    // body and deadline), finds its cleanup verified once the host is back,
    // and deletes; a third run replays the accepted delete's receipt.
    #[tokio::test]
    async fn an_offline_host_leaves_the_delete_pending_and_a_rerun_resumes_it() {
        let mock: Shared = Arc::new(Mutex::new(Mock {
            running: true,
            ..Mock::default()
        }));
        let endpoint = serve(mock.clone()).await;
        let root = state_dir();
        let request = ulid::Ulid::new().to_string();

        let started = std::time::Instant::now();
        let report = run(&endpoint, root.path(), &request).await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it did not wait out the window"
        );
        assert_eq!(report["deleted"], false, "{report}");
        assert_eq!(report["cleanup"], "pending", "{report}");
        assert_eq!(report["host_state"], "offline", "{report}");
        assert_eq!(report["request_id"], request.as_str(), "{report}");
        assert_eq!(
            report["operations"][0]["operation_id"], "op-stop",
            "{report}"
        );
        let first = mock.lock().unwrap().actions.clone();
        assert_eq!(first.len(), 1, "only the Stop was sent");
        assert_eq!(first[0].1["action"], "stop");
        assert!(!mock.lock().unwrap().deleted);

        // The host reconnects and the Stop completes with verified cleanup.
        {
            let mut mock = mock.lock().unwrap();
            mock.host_online = true;
            mock.stop_settled = true;
        }
        let report = run(&endpoint, root.path(), &request).await;
        assert_eq!(report["deleted"], true, "{report}");
        assert_eq!(report["operation_id"], "op-delete", "{report}");
        let actions = mock.lock().unwrap().actions.clone();
        assert_eq!(actions.len(), 3, "{actions:?}");
        assert_eq!(actions[1], first[0], "the same Stop was replayed");
        assert_eq!(actions[2].1["action"], "delete");
        assert_eq!(actions[2].0, format!("{request}-delete"));

        // An exact rerun replays the accepted delete and nothing else.
        let replay = run(&endpoint, root.path(), &request).await;
        assert_eq!(replay, report);
        let actions = mock.lock().unwrap().actions.clone();
        assert_eq!(actions.len(), 4);
        assert_eq!(actions[3], actions[2], "the same delete was replayed");
    }

    // T10: a deployment that holds nothing has nothing to stop; the refused
    // Stop is not kept and the delete follows at once.
    #[tokio::test]
    async fn a_stopped_deployment_is_deleted_without_a_stop() {
        let mock: Shared = Arc::new(Mutex::new(Mock::default()));
        let endpoint = serve(mock.clone()).await;
        let root = state_dir();
        let report = run(&endpoint, root.path(), &ulid::Ulid::new().to_string()).await;
        assert_eq!(report["deleted"], true, "{report}");
        let actions = mock.lock().unwrap().actions.clone();
        let sent: Vec<_> = actions.iter().map(|(_, b)| b["action"].clone()).collect();
        assert_eq!(sent, [json!("stop"), json!("delete")]);
    }
}

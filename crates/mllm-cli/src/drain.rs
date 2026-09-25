//! `mllm drain host <name|id>` and `mllm drain standalone`.
//!
//! SPEC §4.3: server and agent shutdown must distinguish an ordinary service
//! restart from explicit draining of deployments. Owner decision P3
//! (2026-09-22) makes every signal a restart that leaves engines running; this
//! command is the explicit drain. It asks the management API to stop every
//! deployment holding a runtime on the host and then waits for each Stop to
//! settle. A Stop succeeds only when its cleanup proved the recorded processes
//! gone, so a succeeded operation is the cleanup evidence; the deployments stay
//! eligible for on-demand activation (SPEC §6.3).
//!
//! SPEC §14 lists no role-stop verb, so stopping a role remains a signal to its
//! foreground process; `drain` is the only verb here, action first like the rest
//! of the grammar.
//!
//! Owner decision 4 (2026-09-22): a drain of an offline host returns at once
//! with `drained: false`, `host_state: "offline"`, `stops: "pending"` and the
//! Stops' operation ids (`host` stays the host id, as in every drain report). The Stops stay durable and complete on the host's gone
//! evidence when it reconnects within the drain window, and the host takes no
//! new placements until they settle. `--wait` still waits for them, up to the
//! drain window.
use crate::client_journal::RequestJournal;
use crate::output::StructuredError;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

/// How long the operator's drain may take, from when it is first sent. The
/// deadline is journaled with the request before it is sent (SPEC §13), so a
/// retried drain with the same `--request-id` sends the same deadline and
/// replays the same Stops rather than conflicting with them, and a request
/// identity minted earlier never starts a drain whose deadline already passed.
/// This is the drain's bound, not each Stop's deadline: the server lowers every
/// Stop to its deployment's request-deadline window (SPEC §6: no operation's
/// deadline lies beyond it), so a deployment whose request deadline is shorter
/// than this window is still drained. An offline host's Stops complete on
/// reconnect only within those deadlines (owner decision 4).
const DRAIN_WINDOW_MS: i64 = 900_000;
const POLL: Duration = Duration::from_millis(250);
/// SPEC §6.4: a transient management failure while waiting is retried with
/// backoff from this delay up to [`MAX_BACKOFF`], within the wait's bound.
const FIRST_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// How long past the drain's deadline its outcome is still polled for.
const SETTLE_MARGIN_MS: i64 = 10_000;

fn error(code: &'static str, message: impl Into<String>) -> StructuredError {
    StructuredError {
        code,
        message: message.into(),
    }
}

fn unavailable() -> StructuredError {
    error(
        "management_unavailable",
        "Management request did not complete; inspect host and deployment status before retrying",
    )
}

/// Resolve where the drain is sent: the server context for an enrolled host,
/// the local standalone management listener for the embedded host.
pub async fn execute(
    host: Option<&str>,
    state_dir: &Path,
    config: Option<&Path>,
    request_id: Option<&str>,
    wait: bool,
) -> Result<Value, StructuredError> {
    let (endpoint, token, path_host, journal_root) = match host {
        Some(host) => {
            let server = crate::remote_roles::server_context(config, state_dir)?;
            let (endpoint, token) = crate::remote_roles::management_context(&server)?;
            (endpoint, token, host.to_owned(), server.state_dir)
        }
        None => {
            let credentials = std::fs::read_to_string(state_dir.join("identity/credentials"))
                .map_err(|_| {
                    error(
                        "invalid_config",
                        "Standalone management credentials are unavailable",
                    )
                })?;
            let token = credentials
                .lines()
                .find_map(|line| line.strip_prefix("admin_token: "))
                .ok_or_else(|| {
                    error(
                        "invalid_config",
                        "Standalone management credential is missing",
                    )
                })?
                .to_owned();
            let address = crate::roles::standalone_management_address()
                .map_err(|failure| error("invalid_config", failure.to_string()))?;
            (
                format!("http://{address}/management/v1"),
                token,
                "standalone".to_owned(),
                state_dir.to_owned(),
            )
        }
    };
    let id = request_id
        .map(str::to_owned)
        .unwrap_or_else(|| ulid::Ulid::new().to_string());
    let authorization = Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let journal = RequestJournal::open(
        &journal_root,
        &id,
        &endpoint,
        authorization,
        json!({"command": "drain", "host": path_host}),
    )?;
    drain(&endpoint, &token, &path_host, &journal, wait).await
}

fn now_ms() -> Result<i64, StructuredError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|now| i64::try_from(now.as_millis()).ok())
        .ok_or_else(|| error("internal", "System clock unavailable"))
}

/// SPEC §6.4: whether a refused management read is worth retrying: the server
/// said it is busy or unavailable, or marked the refusal retryable.
fn transient(status: reqwest::StatusCode, value: &Value) -> bool {
    matches!(status.as_u16(), 429 | 502 | 503 | 504) || value["error"]["retryable"] == true
}

async fn send(
    request: reqwest::RequestBuilder,
) -> Result<(reqwest::StatusCode, Value), StructuredError> {
    let mut response = request.send().await.map_err(|_| unavailable())?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
        if bytes.len().saturating_add(chunk.len()) > 8 * 1024 * 1024 {
            return Err(error("internal", "Management response exceeds its bound"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| error("internal", "Invalid management response"))?;
    Ok((status, value))
}

async fn drain(
    endpoint: &str,
    token: &str,
    host: &str,
    journal: &RequestJournal,
    wait: bool,
) -> Result<Value, StructuredError> {
    let key = journal.id().to_owned();
    eprintln!("Request identity: {key} (reuse --request-id {key} to recover this command)");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| error("internal", "Cannot build management client"))?;
    let path = format!("/hosts/{host}/drain");
    // SPEC §13: the exact body, deadline included, is journaled before the
    // first send; a recovered drain replays it rather than computing another.
    let saved = journal.saved("drain")?;
    let mutation = match saved {
        Some(saved) => saved,
        None => {
            let deadline = now_ms()?
                .checked_add(DRAIN_WINDOW_MS)
                .ok_or_else(|| error("internal", "Clock overflow"))?;
            journal.prepare("drain", &path, json!({ "deadline_ms": deadline }))?
        }
    };
    let deadline_ms = mutation.body["deadline_ms"]
        .as_i64()
        .ok_or_else(|| error("internal", "Invalid saved drain request"))?;
    let expired = now_ms()? >= deadline_ms;
    let accepted = match (expired, journal.saved("accepted")?) {
        // A drain past its deadline is never sent again: it could only ask
        // for Stops that are already due. What it accepted is reported.
        (true, Some(receipt)) => receipt.body,
        (true, None) => {
            return Err(error(
                "command_rejected",
                format!(
                    "Drain request {key} passed its deadline before its answer was received; \
                     nothing was sent again. Inspect the host's deployments, then start a new \
                     drain without --request-id"
                ),
            ))
        }
        (false, _) => {
            let (status, accepted) = send(
                client
                    .post(format!("{endpoint}{}", mutation.path))
                    .bearer_auth(token)
                    .header("idempotency-key", key.clone())
                    .json(&mutation.body),
            )
            .await?;
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(error("not_found", format!("Host {host} not found")));
            }
            if !status.is_success() {
                // SPEC §14: the refusal keeps the server's error class.
                return Err(crate::client::refusal(status, &accepted));
            }
            // Kept with the request so a replay past the deadline can still
            // report what this drain issued.
            let _ = journal.forget("accepted");
            journal.prepare("accepted", &path, accepted.clone())?;
            accepted
        }
    };
    let operations: Vec<Value> = accepted["operations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let refused = accepted["refused"].clone();
    let host_state = accepted["host_state"]
        .as_str()
        .unwrap_or("online")
        .to_owned();
    // Owner decision 4: an offline host's Stops wait for it to reconnect; say so
    // at once instead of polling out the whole window.
    if host_state == "offline" && !wait && !expired {
        let report = json!({
            "host": accepted["host"],
            "host_state": "offline",
            "request_id": key.to_string(),
            "drained": false,
            "stops": if operations.is_empty() { "none" } else { "pending" },
            "operations": operations
                .iter()
                .map(|operation| json!({
                    "deployment_id": operation["deployment_id"],
                    "instance": operation["instance"],
                    "operation_id": operation["operation_id"],
                }))
                .collect::<Vec<_>>(),
            "refused": refused,
        });
        return if refused.as_array().is_none_or(|refused| refused.is_empty()) {
            Ok(report)
        } else {
            Err(error(
                "operation_failed",
                format!("Not every engine on host {host} could be stopped: {report}"),
            ))
        };
    }
    // Wait for every accepted Stop to settle, up to the drain's own deadline
    // plus a margin for the outcome to be recorded. A succeeded Stop committed
    // its cleanup against evidence that the recorded processes are gone.
    let remaining = deadline_ms
        .saturating_add(SETTLE_MARGIN_MS)
        .saturating_sub(now_ms()?)
        .max(0);
    let wait_until =
        tokio::time::Instant::now() + Duration::from_millis(u64::try_from(remaining).unwrap_or(0));
    let mut backoff = FIRST_BACKOFF;
    let settled = loop {
        // SPEC §6.4: a transient failure of one status read does not end the
        // wait; it is retried with bounded backoff until the wait's bound.
        let read = send(
            client
                .get(format!("{endpoint}/snapshot"))
                .bearer_auth(token),
        )
        .await;
        let snapshot = match read {
            Ok((status, snapshot)) if status.is_success() => {
                backoff = FIRST_BACKOFF;
                snapshot
            }
            Ok((status, value)) if transient(status, &value) => {
                if tokio::time::Instant::now() >= wait_until {
                    return Err(unavailable());
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
            Err(failure) if failure.code == "management_unavailable" => {
                if tokio::time::Instant::now() >= wait_until {
                    return Err(failure);
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
            Ok(_) => return Err(unavailable()),
            Err(failure) => return Err(failure),
        };
        let states: Vec<Value> = operations
            .iter()
            .map(|operation| {
                let id = &operation["operation_id"];
                let state = snapshot["operations"]
                    .as_array()
                    .and_then(|all| all.iter().find(|candidate| candidate["id"] == *id))
                    .map(|found| found["state"].clone())
                    .unwrap_or(Value::Null);
                let deployment = snapshot["deployments"]
                    .as_array()
                    .and_then(|all| {
                        all.iter()
                            .find(|candidate| candidate["id"] == operation["deployment_id"])
                    })
                    .cloned()
                    .unwrap_or(Value::Null);
                json!({
                    "deployment_id": operation["deployment_id"],
                    "instance": operation["instance"],
                    "operation_id": id,
                    "state": state,
                    "cleanup": if state == "succeeded" { "verified" } else { "unverified" },
                    "observed_state": deployment["observed_state"],
                    // SPEC §6.3: a drain is not an operator stop; the deployment
                    // stays eligible for on-demand activation.
                    "suspended": deployment["suspended"],
                })
            })
            .collect();
        let terminal = states.iter().all(|state| {
            matches!(
                state["state"].as_str(),
                Some("succeeded" | "failed" | "cancelled")
            )
        });
        if terminal || tokio::time::Instant::now() >= wait_until {
            break states;
        }
        tokio::time::sleep(POLL).await;
    };
    let drained = settled.iter().all(|state| state["state"] == "succeeded")
        && refused.as_array().is_none_or(|refused| refused.is_empty());
    let report = json!({
        "host": accepted["host"],
        "host_state": host_state,
        "request_id": key.to_string(),
        "drained": drained,
        "deployments": settled,
        "refused": refused,
    });
    if drained {
        Ok(report)
    } else {
        Err(error(
            "operation_failed",
            format!(
                "Not every engine on host {host} was stopped with verified cleanup; \
                 retry with --request-id {key}: {report}"
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing, Json, Router};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    /// What the mock management API observed.
    #[derive(Default)]
    struct Seen {
        reads: AtomicUsize,
        posts: AtomicUsize,
        deadlines: Mutex<Vec<i64>>,
    }

    /// A management API whose host is offline: the drain accepts one Stop, and
    /// each snapshot read counts. The first `fail_first` reads answer a
    /// retryable 503; the Stop settles once `settle_after` reads have been
    /// answered (the host reconnected).
    async fn management(settle_after: usize, fail_first: usize) -> (String, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let posted = seen.clone();
        let counted = seen.clone();
        let app = Router::new()
            .route(
                "/management/v1/hosts/lab/drain",
                routing::post(move |Json(body): Json<Value>| {
                    let posted = posted.clone();
                    async move {
                        posted.posts.fetch_add(1, Ordering::SeqCst);
                        posted
                            .deadlines
                            .lock()
                            .unwrap()
                            .push(body["deadline_ms"].as_i64().unwrap());
                        (
                            axum::http::StatusCode::ACCEPTED,
                            Json(json!({
                                "api_version": "1",
                                "host": "lab-id",
                                "host_state": "offline",
                                "stops": "pending",
                                "operations": [{
                                    "deployment_id": "d1",
                                    "instance": 0,
                                    "operation_id": "op-1",
                                    "revision": "1",
                                }],
                                "refused": [],
                            })),
                        )
                    }
                }),
            )
            .route(
                "/management/v1/snapshot",
                routing::get(move || {
                    let read = counted.reads.fetch_add(1, Ordering::SeqCst) + 1;
                    async move {
                        if read <= fail_first {
                            return (
                                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({"error": {"code": "snapshot_unavailable", "retryable": true}})),
                            );
                        }
                        let state = if read > settle_after { "succeeded" } else { "running" };
                        (
                            axum::http::StatusCode::OK,
                            Json(json!({
                                "operations": [{"id": "op-1", "state": state}],
                                "deployments": [{"id": "d1", "observed_state": "stopped", "suspended": false}],
                            })),
                        )
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/management/v1"), seen)
    }

    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn journal(root: &Path, endpoint: &str, id: &str) -> RequestJournal {
        let authorization = Sha256::digest(b"token")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        RequestJournal::open(
            root,
            id,
            endpoint,
            authorization,
            json!({"command": "drain", "host": "lab"}),
        )
        .unwrap()
    }

    // T10 T32: an offline host's drain returns at once with its Stops pending
    // and their operation ids; it never polls the snapshot.
    #[tokio::test]
    async fn an_offline_host_drain_returns_at_once_with_pending_stops() {
        let (endpoint, seen) = management(0, 0).await;
        let root = root();
        let journal = journal(root.path(), &endpoint, &ulid::Ulid::new().to_string());
        let started = std::time::Instant::now();
        let report = drain(&endpoint, "token", "lab", &journal, false)
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(report["drained"], false, "{report}");
        assert_eq!(report["host_state"], "offline", "{report}");
        assert_eq!(report["host"], "lab-id", "{report}");
        assert_eq!(report["stops"], "pending", "{report}");
        assert_eq!(report["operations"][0]["operation_id"], "op-1", "{report}");
        assert_eq!(report["operations"][0]["deployment_id"], "d1", "{report}");
        assert_eq!(seen.reads.load(Ordering::SeqCst), 0, "nothing waited");
    }

    // T10 T33: `--wait` still waits for the offline host's Stops, which settle
    // with verified cleanup once it reconnects.
    #[tokio::test]
    async fn wait_waits_for_an_offline_host_to_reconnect() {
        let (endpoint, seen) = management(2, 0).await;
        let root = root();
        let journal = journal(root.path(), &endpoint, &ulid::Ulid::new().to_string());
        let report = drain(&endpoint, "token", "lab", &journal, true)
            .await
            .unwrap();
        assert!(
            seen.reads.load(Ordering::SeqCst) >= 3,
            "it polled until settled"
        );
        assert_eq!(report["drained"], true, "{report}");
        assert_eq!(report["host_state"], "offline", "{report}");
        assert_eq!(report["deployments"][0]["state"], "succeeded", "{report}");
        assert_eq!(report["deployments"][0]["cleanup"], "verified", "{report}");
    }

    // T08 T38 (SPEC §6.4): a transient failure of a status read while waiting
    // is retried with backoff instead of ending the wait.
    #[tokio::test]
    async fn a_transient_snapshot_failure_is_retried() {
        let (endpoint, seen) = management(3, 2).await;
        let root = root();
        let journal = journal(root.path(), &endpoint, &ulid::Ulid::new().to_string());
        let report = drain(&endpoint, "token", "lab", &journal, true)
            .await
            .unwrap();
        assert!(
            seen.reads.load(Ordering::SeqCst) >= 4,
            "the failed reads were retried"
        );
        assert_eq!(report["drained"], true, "{report}");
    }

    // T09 T13 (SPEC §§6.4, 13): the drain's deadline is taken when it is first
    // sent and journaled with it. A request identity minted long before (its
    // ULID time is an hour old) still gets a live deadline, and a recovered
    // drain replays the same deadline under the same key.
    #[tokio::test]
    async fn the_deadline_is_journaled_and_replayed() {
        let (endpoint, seen) = management(0, 0).await;
        let root = root();
        let old = ulid::Ulid::from_parts(u64::try_from(now_ms().unwrap() - 3_600_000).unwrap(), 7)
            .to_string();
        let before = now_ms().unwrap();
        drain(
            &endpoint,
            "token",
            "lab",
            &journal(root.path(), &endpoint, &old),
            false,
        )
        .await
        .unwrap();
        drain(
            &endpoint,
            "token",
            "lab",
            &journal(root.path(), &endpoint, &old),
            false,
        )
        .await
        .unwrap();
        let deadlines = seen.deadlines.lock().unwrap().clone();
        assert_eq!(deadlines.len(), 2);
        assert_eq!(
            deadlines[0], deadlines[1],
            "a replay sends the journaled deadline"
        );
        assert!(deadlines[0] >= before + DRAIN_WINDOW_MS, "{deadlines:?}");
    }

    // T13 T34 (SPEC §13): a drain replayed after its deadline is never sent
    // again with that expired deadline; it reports what it accepted. One whose
    // answer was never received is refused with a pointer to a new drain.
    #[tokio::test]
    async fn a_replay_past_the_deadline_is_not_sent_again() {
        let (endpoint, seen) = management(0, 0).await;
        let root = root();
        let id = ulid::Ulid::new().to_string();
        let saved = journal(root.path(), &endpoint, &id);
        saved
            .prepare("drain", "/hosts/lab/drain", json!({"deadline_ms": 1_000}))
            .unwrap();
        saved
            .prepare(
                "accepted",
                "/hosts/lab/drain",
                json!({"host": "lab-id", "host_state": "online", "refused": [],
                       "operations": [{"deployment_id": "d1", "instance": 0, "operation_id": "op-1"}]}),
            )
            .unwrap();
        drop(saved);
        let report = drain(
            &endpoint,
            "token",
            "lab",
            &journal(root.path(), &endpoint, &id),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            seen.posts.load(Ordering::SeqCst),
            0,
            "an expired drain was sent"
        );
        assert_eq!(report["deployments"][0]["operation_id"], "op-1", "{report}");
        assert_eq!(report["drained"], true, "{report}");

        let lost = ulid::Ulid::new().to_string();
        let saved = journal(root.path(), &endpoint, &lost);
        saved
            .prepare("drain", "/hosts/lab/drain", json!({"deadline_ms": 1_000}))
            .unwrap();
        drop(saved);
        let refused = drain(
            &endpoint,
            "token",
            "lab",
            &journal(root.path(), &endpoint, &lost),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(refused.code, "command_rejected");
        assert_eq!(seen.posts.load(Ordering::SeqCst), 0);
    }
}

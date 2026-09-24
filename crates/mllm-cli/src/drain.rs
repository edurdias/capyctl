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
use crate::output::StructuredError;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

/// How long the operator's drain may take, from the request's own timestamp. A
/// retried drain with the same `--request-id` sends the same deadline and so
/// replays the same Stops rather than conflicting with them. The store bounds a
/// Stop's deadline by the deployment's request deadline, so an offline host's
/// Stops complete on reconnect only within this window (owner decision 4).
const DRAIN_WINDOW_MS: i64 = 900_000;
const POLL: Duration = Duration::from_millis(250);

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
    let (endpoint, token, path_host) = match host {
        Some(host) => {
            let server = crate::remote_roles::server_context(config, state_dir)?;
            let (endpoint, token) = crate::remote_roles::management_context(&server)?;
            (endpoint, token, host.to_owned())
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
            )
        }
    };
    drain(&endpoint, &token, &path_host, request_id, wait).await
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
    request_id: Option<&str>,
    wait: bool,
) -> Result<Value, StructuredError> {
    let key = match request_id {
        Some(id) => id
            .parse::<ulid::Ulid>()
            .map_err(|_| error("invalid_config", "--request-id must be a ULID"))?,
        None => ulid::Ulid::new(),
    };
    eprintln!("Request identity: {key} (reuse --request-id {key} to recover this command)");
    let issued =
        i64::try_from(key.timestamp_ms()).map_err(|_| error("internal", "Clock overflow"))?;
    let deadline_ms = issued + DRAIN_WINDOW_MS;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| error("internal", "Cannot build management client"))?;
    let (status, accepted) = send(
        client
            .post(format!("{endpoint}/hosts/{host}/drain"))
            .bearer_auth(token)
            .header("idempotency-key", key.to_string())
            .json(&json!({ "deadline_ms": deadline_ms })),
    )
    .await?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(error("not_found", format!("Host {host} not found")));
    }
    if !status.is_success() {
        // SPEC §14: the refusal keeps the server's error class.
        return Err(crate::client::refusal(status, &accepted));
    }
    let operations: Vec<Value> = accepted["operations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let refused = accepted["refused"].clone();
    let host_state = accepted["host_state"].as_str().unwrap_or("online").to_owned();
    // Owner decision 4: an offline host's Stops wait for it to reconnect; say so
    // at once instead of polling out the whole window.
    if host_state == "offline" && !wait {
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
    // Wait for every accepted Stop to settle. A succeeded Stop committed its
    // cleanup against evidence that the recorded processes are gone.
    let wait_until = tokio::time::Instant::now()
        + Duration::from_millis(u64::try_from(DRAIN_WINDOW_MS).unwrap_or(900_000))
        + Duration::from_secs(10);
    let settled = loop {
        let (status, snapshot) = send(
            client
                .get(format!("{endpoint}/snapshot"))
                .bearer_auth(token),
        )
        .await?;
        if !status.is_success() {
            return Err(unavailable());
        }
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
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    /// A management API whose host is offline: the drain accepts one Stop, and
    /// each snapshot read counts; the Stop settles once `settle_after` reads
    /// have been answered (the host reconnected).
    async fn management(settle_after: usize) -> (String, Arc<AtomicUsize>) {
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = reads.clone();
        let app = Router::new()
            .route(
                "/management/v1/hosts/lab/drain",
                routing::post(|| async {
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
                }),
            )
            .route(
                "/management/v1/snapshot",
                routing::get(move || {
                    let read = counted.fetch_add(1, Ordering::SeqCst) + 1;
                    async move {
                        let state = if read > settle_after { "succeeded" } else { "running" };
                        Json(json!({
                            "operations": [{"id": "op-1", "state": state}],
                            "deployments": [{"id": "d1", "observed_state": "stopped", "suspended": false}],
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/management/v1"), reads)
    }

    // T10 T32: an offline host's drain returns at once with its Stops pending
    // and their operation ids; it never polls the snapshot.
    #[tokio::test]
    async fn an_offline_host_drain_returns_at_once_with_pending_stops() {
        let (endpoint, reads) = management(0).await;
        let started = std::time::Instant::now();
        let report = drain(&endpoint, "token", "lab", None, false).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(report["drained"], false, "{report}");
        assert_eq!(report["host_state"], "offline", "{report}");
        assert_eq!(report["host"], "lab-id", "{report}");
        assert_eq!(report["stops"], "pending", "{report}");
        assert_eq!(report["operations"][0]["operation_id"], "op-1", "{report}");
        assert_eq!(report["operations"][0]["deployment_id"], "d1", "{report}");
        assert_eq!(reads.load(Ordering::SeqCst), 0, "nothing waited");
    }

    // T10 T33: `--wait` still waits for the offline host's Stops, which settle
    // with verified cleanup once it reconnects.
    #[tokio::test]
    async fn wait_waits_for_an_offline_host_to_reconnect() {
        let (endpoint, reads) = management(2).await;
        let report = drain(&endpoint, "token", "lab", None, true).await.unwrap();
        assert!(reads.load(Ordering::SeqCst) >= 3, "it polled until settled");
        assert_eq!(report["drained"], true, "{report}");
        assert_eq!(report["host_state"], "offline", "{report}");
        assert_eq!(report["deployments"][0]["state"], "succeeded", "{report}");
        assert_eq!(report["deployments"][0]["cleanup"], "verified", "{report}");
    }
}

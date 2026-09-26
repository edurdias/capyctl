//! Local management client for the generated standalone configuration.
//! SPEC §14 / §16.5: commands use authenticated management, never direct stores.

use crate::client_journal::RequestJournal;
use crate::grammar::{Command, LifecycleAction, ListResource, Resource};
use crate::output::StructuredError;
use reqwest::{Client, Method};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};

// SPEC §6.3 (W6): `delete deployment --stop`.
mod delete;

const ENDPOINT: &str = "http://127.0.0.1:7443/management/v1";

/// ADR 0014 amendment A1: the window used only against a server whose status
/// does not report the deployment's timeouts (the previous fixed window).
const LEGACY_WINDOW_MS: i64 = 900_000;
/// How long `--wait` keeps polling after the operation's own deadline.
const WAIT_MARGIN_MS: i64 = 10_000;
/// SPEC §6.4: the first and largest delay between retries of a status read
/// that failed transiently while `--wait` observes an operation.
const FIRST_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

fn error(code: &'static str, message: impl Into<String>) -> StructuredError {
    StructuredError {
        code,
        message: message.into(),
    }
}

pub fn supports(command: &Command) -> bool {
    matches!(
        command,
        Command::Deploy { .. }
            | Command::Status { .. }
            | Command::List {
                resource: ListResource::Deployments
            }
            | Command::Inspect {
                resource: Resource::Deployment,
                ..
            }
            | Command::Lifecycle {
                action: LifecycleAction::Start
                    | LifecycleAction::Stop
                    | LifecycleAction::Park
                    | LifecycleAction::Preinitialize,
                ..
            }
            | Command::InstanceLifecycle { .. }
            | Command::Delete { .. }
    )
}

struct Management {
    client: Client,
    token: String,
    endpoint: String,
    journal: Option<RequestJournal>,
    /// ADR 0014 amendment A1: the operator's `--initialize-timeout`.
    initialize_timeout_ms: Option<i64>,
    /// Owner decision 2026-09-23: `start --evict`.
    evict: bool,
    /// SPEC §6.4: `start --wait`.
    wait_start: bool,
}

impl Management {
    fn begin_request(
        &mut self,
        root: &Path,
        request_id: Option<&str>,
        intent: Value,
    ) -> Result<(), StructuredError> {
        let id = request_id
            .map(str::to_owned)
            .unwrap_or_else(|| ulid::Ulid::new().to_string());
        let authorization = Sha256::digest(self.token.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        self.journal = Some(RequestJournal::open(
            root,
            &id,
            &self.endpoint,
            authorization,
            intent,
        )?);
        eprintln!("Request identity: {id} (reuse --request-id {id} to recover this command)");
        Ok(())
    }
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, StructuredError> {
        let step = if path == "/deployments" {
            "create"
        } else {
            "action"
        };
        let (status, value) = self
            .exchange_bounded(method, path, body.map(|body| (step, body)), None)
            .await?;
        if !status.is_success() {
            return Err(refusal(status, &value));
        }
        Ok(value)
    }

    /// One management exchange. A mutation's exact body is committed to the
    /// request journal under `step` before it is sent (SPEC §13). A refusal is
    /// returned with its status and body, for callers that act on its code.
    async fn exchange(
        &self,
        method: Method,
        path: &str,
        body: Option<(&str, Value)>,
    ) -> Result<(reqwest::StatusCode, Value), StructuredError> {
        self.exchange_bounded(method, path, body, None).await
    }

    /// As [`Self::exchange`], with a longer bound on this one request (an
    /// evicting start waits for its victims to drain and be released).
    async fn exchange_bounded(
        &self,
        method: Method,
        path: &str,
        body: Option<(&str, Value)>,
        timeout: Option<Duration>,
    ) -> Result<(reqwest::StatusCode, Value), StructuredError> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.endpoint))
            .bearer_auth(&self.token);
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        if let Some((step, body)) = body {
            // SPEC §13: commit exact body/deadline/fence before the first send.
            let mutation = self
                .journal
                .as_ref()
                .ok_or_else(|| error("internal", "Mutation has no request identity"))?
                .prepare(step, path, body)?;
            request = request
                .header("idempotency-key", mutation.key)
                .json(&mutation.body);
        }
        let mut response = request.send().await.map_err(|_| {
            error(
                "management_unavailable",
                "Management request did not complete; inspect status before retrying",
            )
        })?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            error(
                "management_unavailable",
                "Management response was interrupted",
            )
        })? {
            if bytes.len().saturating_add(chunk.len()) > 8 * 1024 * 1024 {
                return Err(error("internal", "Management response exceeds its bound"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| error("internal", "Invalid management response"))?;
        Ok((status, value))
    }

    async fn snapshot(&self) -> Result<Value, StructuredError> {
        self.request(Method::GET, "/snapshot", None).await
    }

    async fn action(&self, id: &str, action: &str) -> Result<(Value, i64), StructuredError> {
        self.action_on(id, action, None).await
    }

    /// Owner decision Q7: `instance` addresses one instance of the deployment.
    /// Returns the receipt and the deadline the command carried.
    async fn action_on(
        &self,
        id: &str,
        action: &str,
        instance: Option<u32>,
    ) -> Result<(Value, i64), StructuredError> {
        if let Some(saved) = self
            .journal
            .as_ref()
            .ok_or_else(|| error("internal", "Mutation has no request identity"))?
            .saved("action")?
        {
            // SPEC §13: a recovered command replays its exact body, deadline
            // and `--evict` included.
            let deadline = saved.body["deadline_ms"].as_i64().unwrap_or(0);
            let evicting = saved.body["evict"].as_bool().unwrap_or(false);
            let receipt = self
                .send_action(
                    &saved.path,
                    saved.body,
                    evicting.then(|| evict_bound(deadline)),
                )
                .await?;
            return Ok((receipt, deadline));
        }
        let snapshot = self.snapshot().await?;
        let deployment = deployment(&snapshot, id)?;
        let id = deployment["id"]
            .as_str()
            .ok_or_else(|| error("internal", "Missing deployment identity"))?;
        let revision: i64 = deployment["revision"]
            .as_str()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| error("internal", "Invalid deployment revision"))?;
        let window = window(deployment, action, self.initialize_timeout_ms)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| error("internal", "System clock unavailable"))?
            .as_millis();
        let deadline = i64::try_from(now)
            .ok()
            .and_then(|now| now.checked_add(window))
            .ok_or_else(|| error("internal", "System clock unavailable"))?;
        let path = match instance {
            None => format!("/deployments/{id}/actions"),
            Some(index) => format!("/deployments/{id}/instances/{index}/actions"),
        };
        let mut body =
            json!({"action": action, "expected_revision": revision, "deadline_ms": deadline});
        // Owner decision 2026-09-23: only a start evicts, and only when asked.
        let evicting = self.evict && action == "start";
        if evicting {
            body["evict"] = json!(true);
        }
        let receipt = self
            .send_action(&path, body, evicting.then(|| evict_bound(deadline)))
            .await?;
        Ok((receipt, deadline))
    }

    /// Send one action body, journaled before it is sent (SPEC §13).
    async fn send_action(
        &self,
        path: &str,
        body: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, StructuredError> {
        let (status, value) = self
            .exchange_bounded(Method::POST, path, Some(("action", body)), timeout)
            .await?;
        if !status.is_success() {
            return Err(refusal(status, &value));
        }
        Ok(value)
    }

    /// Owner decision 2026-09-25: `start deployment --wait` waits for every
    /// instance the start targets, not only the one whose operation the
    /// receipt names. It succeeds only once each active instance has been
    /// Ready; an instance that ends without becoming Ready (not placed before
    /// the start's deadline, a failed launch) fails the wait, and so does the
    /// deadline passing with an instance still queued. Never a success on a
    /// partial start.
    async fn wait_all(&self, receipt: Value, deadline_ms: i64) -> Result<Value, StructuredError> {
        let first = self.wait(receipt.clone(), deadline_ms).await?;
        let id = receipt["deployment_id"].as_str().unwrap_or("").to_owned();
        let mut seen = std::collections::BTreeSet::new();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|now| i64::try_from(now.as_millis()).ok())
            .unwrap_or(0);
        let remaining = if deadline_ms > 0 {
            deadline_ms
                .saturating_sub(now_ms)
                .saturating_add(WAIT_MARGIN_MS)
                .max(WAIT_MARGIN_MS)
        } else {
            LEGACY_WINDOW_MS + WAIT_MARGIN_MS
        };
        let until = tokio::time::Instant::now()
            + Duration::from_millis(u64::try_from(remaining).unwrap_or(0));
        let mut view = first["deployment"].clone();
        loop {
            match replicas(&view, &mut seen) {
                Replicas::AllReady => {
                    return Ok(json!({"receipt": first["receipt"], "deployment": view}))
                }
                Replicas::Failed(failure) => return Err(failure),
                Replicas::Pending => {}
            }
            if tokio::time::Instant::now() >= until {
                let queued: Vec<String> = view["instances"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|i| {
                        !seen.contains(&i["index"].as_u64().unwrap_or(0))
                            && i["lifecycle"] != "retiring"
                    })
                    .map(|i| {
                        format!(
                            "instance {} {}{}",
                            i["index"],
                            i["observed_state"].as_str().unwrap_or("unknown"),
                            i["last_error"]
                                .as_str()
                                .map(|e| format!(" ({e})"))
                                .unwrap_or_default()
                        )
                    })
                    .collect();
                let placement = view["instances"].as_array().into_iter().flatten().any(|i| {
                    i["last_error"]
                        .as_str()
                        .is_some_and(|e| e.starts_with("placement:"))
                });
                return Err(error(
                    if placement { "insufficient_resources" } else { "activation_timeout" },
                    format!(
                        "Wait expired before every instance was ready: {}; the start is partial and was not cancelled",
                        queued.join(", ")
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            // SPEC §6.4: a transient status failure is retried within the bound.
            match self.exchange(Method::GET, "/snapshot", None).await {
                Ok((status, snapshot)) if status.is_success() => {
                    view = deployment(&snapshot, &id)?.clone();
                }
                Ok((status, value))
                    if matches!(status.as_u16(), 429 | 502 | 503 | 504)
                        || value["error"]["retryable"] == true => {}
                Err(failure) if failure.code == "management_unavailable" => {}
                Ok((status, value)) => return Err(refusal(status, &value)),
                Err(failure) => return Err(failure),
            }
        }
    }

    /// ADR 0014 §7 (WE3): a new checkpoint's digest is measured by a host
    /// after the deploy is accepted, and activation waits for it
    /// (`checkpoint_digest_pending`). `deploy model --activate` and `start
    /// --wait` wait here for that measurement instead of being refused, so one
    /// command starts a new checkpoint (found walking the guides 2026-09-25).
    /// The wait is bounded by the start's own Initialize window (the
    /// conservative pending value, or `--initialize-timeout`); nothing is
    /// started if it expires. A digest that is recorded, mismatched or absent
    /// ends the wait at once, and the start that follows decides. ADR 0008:
    /// a declared remote source that is still downloading is waited for the
    /// same way, within the same bound (owner decision 2026-09-25).
    async fn await_activation_inputs(&self, id: &str) -> Result<(), StructuredError> {
        let mut until: Option<(tokio::time::Instant, i64)> = None;
        let mut backoff = FIRST_BACKOFF;
        loop {
            let snapshot = match self.exchange(Method::GET, "/snapshot", None).await {
                Ok((status, snapshot)) if status.is_success() => snapshot,
                // SPEC §6.4: a transient status failure is retried within the bound.
                Ok((status, value))
                    if until.is_some()
                        && (matches!(status.as_u16(), 429 | 502 | 503 | 504)
                            || value["error"]["retryable"] == true) =>
                {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
                Ok((status, value)) => return Err(refusal(status, &value)),
                Err(failure) => return Err(failure),
            };
            backoff = FIRST_BACKOFF;
            let current = deployment(&snapshot, id)?;
            let digest = &current["checkpoint_digest"];
            let digest_pending = digest["state"] == "pending" && digest["provisional"] == true;
            // ADR 0008 (owner decision 2026-09-25): a declared remote source is
            // waited for the same way; found live, `--activate --wait` was
            // refused `model_source_pending` while the download ran.
            let source_pending = model_source_pending(current);
            if !digest_pending && !source_pending {
                return Ok(());
            }
            let (deadline, window_ms) = match until {
                Some(bound) => bound,
                None => {
                    let window_ms = window(current, "start", self.initialize_timeout_ms)?;
                    let name = current["name"].as_str().unwrap_or(id);
                    let what = if source_pending {
                        format!("the model source of {name} to be downloaded and verified")
                    } else {
                        format!("the checkpoint digest of {name} to be measured")
                    };
                    eprintln!("Waiting for {what} (at most {}s)", window_ms / 1000);
                    let bound = (
                        tokio::time::Instant::now()
                            + Duration::from_millis(u64::try_from(window_ms).unwrap_or(0)),
                        window_ms,
                    );
                    until = Some(bound);
                    bound
                }
            };
            if tokio::time::Instant::now() >= deadline {
                let name = current["name"].as_str().unwrap_or(id);
                let message = if source_pending {
                    format!(
                        "model_source_pending: the model source of {name} was still being downloaded after {}s; nothing was started. `mllm status deployment {name}` shows its progress; run `mllm start deployment {name} --wait` again once it is verified",
                        window_ms / 1000
                    )
                } else {
                    format!(
                        "checkpoint_digest_pending: the checkpoint digest of {name} was still being measured after {}s; nothing was started. `mllm status deployment {name}` shows it; run `mllm start deployment {name} --wait` again once it is recorded",
                        window_ms / 1000
                    )
                };
                return Err(error("activation_timeout", message));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn wait(&self, receipt: Value, deadline_ms: i64) -> Result<Value, StructuredError> {
        let operation = receipt["operation_id"]
            .as_str()
            .ok_or_else(|| error("internal", "Missing accepted operation identity"))?;
        // Poll until the operation's own deadline, plus a margin for its outcome
        // to be recorded; the fixed 910 s only when the deadline is unknown.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|now| i64::try_from(now.as_millis()).ok())
            .unwrap_or(0);
        let remaining = if deadline_ms > 0 {
            deadline_ms
                .saturating_sub(now_ms)
                .saturating_add(WAIT_MARGIN_MS)
                .max(WAIT_MARGIN_MS)
        } else {
            LEGACY_WINDOW_MS + WAIT_MARGIN_MS
        };
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(u64::try_from(remaining).unwrap_or(0));
        let mut backoff = FIRST_BACKOFF;
        loop {
            // SPEC §6.4: `--wait` observes the operation; a transient failure
            // of one status read (the request did not complete, or the server
            // was busy or unavailable) is retried with bounded backoff within
            // the wait's bound instead of ending the wait early.
            let snapshot = match self.exchange(Method::GET, "/snapshot", None).await {
                Ok((status, snapshot)) if status.is_success() => {
                    backoff = FIRST_BACKOFF;
                    snapshot
                }
                Ok((status, value))
                    if matches!(status.as_u16(), 429 | 502 | 503 | 504)
                        || value["error"]["retryable"] == true =>
                {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(refusal(status, &value));
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
                Err(failure) if failure.code == "management_unavailable" => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(failure);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
                Ok((status, value)) => return Err(refusal(status, &value)),
                Err(failure) => return Err(failure),
            };
            let result = snapshot["operations"]
                .as_array()
                .and_then(|ops| ops.iter().find(|op| op["id"] == operation));
            if let Some(result) = result {
                match result["state"].as_str() {
                    Some("succeeded") => {
                        return Ok(
                            json!({"receipt": receipt, "deployment": deployment(&snapshot, receipt["deployment_id"].as_str().unwrap_or(""))?}),
                        )
                    }
                    Some("failed" | "cancelled") => {
                        return Err(error(
                            "operation_failed",
                            failure_message(&snapshot, operation),
                        ))
                    }
                    _ => {}
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(error("activation_timeout", format!("Wait expired for operation {operation}; the accepted operation was not cancelled")));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// Owner decision 2026-09-25: what `start deployment --wait` concludes from
/// one status read once the start's own operation succeeded. `seen` holds the
/// instances observed Ready so far during this wait.
#[derive(Debug, PartialEq, Eq)]
enum Replicas {
    /// Every active instance has been Ready during the wait.
    AllReady,
    /// Some instance is still queued, starting or waking.
    Pending,
    /// An instance the start targets ended without becoming Ready.
    Failed(StructuredError),
}

fn replicas(deployment: &Value, seen: &mut std::collections::BTreeSet<u64>) -> Replicas {
    // A server that reports no instances: the start's operation is the answer.
    let Some(instances) = deployment["instances"].as_array() else {
        return Replicas::AllReady;
    };
    let mut pending = false;
    for instance in instances.iter().filter(|i| i["lifecycle"] != "retiring") {
        let index = instance["index"].as_u64().unwrap_or(0);
        let state = instance["observed_state"].as_str().unwrap_or("");
        if state == "ready" {
            seen.insert(index);
        }
        if seen.contains(&index) {
            continue;
        }
        match state {
            // Ended without becoming Ready: a start that could not be placed
            // before its deadline, a failed launch, or stopped meanwhile.
            "stopped" | "failed" | "parked" => {
                let last_error = instance["last_error"].as_str().unwrap_or("");
                let reason = instance["latest_operation"]["reason"]
                    .as_str()
                    .filter(|_| state == "failed")
                    .unwrap_or(last_error);
                let code = if last_error.starts_with("placement:") {
                    "insufficient_resources"
                } else {
                    "operation_failed"
                };
                let why = if reason.is_empty() {
                    "status shows no reason".to_owned()
                } else {
                    reason.to_owned()
                };
                return Replicas::Failed(error(
                    code,
                    format!(
                        "Instance {index} of deployment {} did not become ready ({state}): {why}; the start is partial",
                        deployment["name"].as_str().or(deployment["id"].as_str()).unwrap_or("")
                    ),
                ));
            }
            _ => pending = true,
        }
    }
    if pending {
        Replicas::Pending
    } else {
        Replicas::AllReady
    }
}

/// ADR 0014 amendment A1: how far ahead a command's deadline lies. A start
/// takes `--initialize-timeout` when given, else the deployment's
/// `timeouts.initialize`; a stop takes the Stop window. The store refuses any
/// deadline beyond the request deadline (SPEC §6), so an override beyond it is
/// refused here, before anything is recorded or sent.
fn window(
    deployment: &Value,
    action: &str,
    initialize_timeout_ms: Option<i64>,
) -> Result<i64, StructuredError> {
    let windows = &deployment["timeouts"];
    let limit = windows["request_deadline_ms"].as_i64();
    // SPEC §6.5 (W5): a preinitialize starts each instance in turn, so it is
    // bounded by the request deadline, not one Initialize window.
    if action == "preinitialize" {
        return Ok(limit.unwrap_or(LEGACY_WINDOW_MS));
    }
    if action != "start" {
        return Ok(windows["stop_ms"].as_i64().unwrap_or(LEGACY_WINDOW_MS));
    }
    match initialize_timeout_ms {
        Some(ms) => match limit {
            Some(limit) if ms > limit => Err(error(
                "invalid_config",
                format!(
                    "--initialize-timeout {}s exceeds the deployment's request deadline of {}s",
                    ms / 1000,
                    limit / 1000
                ),
            )),
            _ => Ok(ms),
        },
        None => Ok(windows["initialize_ms"]
            .as_i64()
            .unwrap_or(LEGACY_WINDOW_MS)),
    }
}

/// Owner decision 2026-09-23: how long an evicting start's request may take:
/// until its deadline (the switch drains and releases before the start is
/// accepted), never less than the ordinary 30 s.
fn evict_bound(deadline_ms: i64) -> Duration {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|now| i64::try_from(now.as_millis()).ok())
        .unwrap_or(0);
    let remaining = u64::try_from(deadline_ms.saturating_sub(now)).unwrap_or(0);
    Duration::from_millis(remaining).max(Duration::from_secs(30))
}

/// SPEC §13: a request identity names one intent; `--evict` is part of it.
/// Absent, the intent is unchanged from before the flag existed.
fn with_evict(mut intent: Value, evict: bool) -> Value {
    if evict {
        intent["evict"] = json!(true);
    }
    intent
}

/// SPEC §13: a request identity names one intent; an override is part of it.
/// Absent, the intent is unchanged from before overrides existed.
fn with_timeout(mut intent: Value, initialize_timeout_ms: Option<i64>) -> Value {
    if let Some(ms) = initialize_timeout_ms {
        intent["initialize_timeout_ms"] = json!(ms);
    }
    intent
}

/// SPEC §6.4: why the operation `--wait` observed failed, as status shows it:
/// the reason and hint of that operation where the deployment or one of its
/// instances reports it as its latest operation, else its closed error code
/// with that code's fixed hint, else only where to look.
fn failure_message(snapshot: &Value, operation: &str) -> String {
    let latest = snapshot["deployments"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|d| {
            std::iter::once(&d["latest_operation"]).chain(
                d["instances"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|i| &i["latest_operation"]),
            )
        })
        .find(|op| op["id"] == operation);
    let code = snapshot["operations"]
        .as_array()
        .and_then(|ops| ops.iter().find(|op| op["id"] == operation))
        .and_then(|op| op["error_code"].as_str());
    let reason = latest.and_then(|op| op["reason"].as_str());
    let hint = latest
        .and_then(|op| op["hint"].as_str())
        .or_else(|| code.and_then(mllm_domain::diagnostics::operator_hint));
    let mut message = match (reason, code) {
        (Some(reason), _) => format!("Operation {operation} failed: {reason}"),
        (None, Some(code)) => format!("Operation {operation} failed: {code}"),
        (None, None) => return format!("Operation {operation} failed; inspect deployment status"),
    };
    if let Some(hint) = hint {
        message.push_str(&format!(" (hint: {hint})"));
    }
    message
}

/// A management refusal as the CLI reports it.
///
/// SPEC §14: structured errors keep their meaning. The server's closed code
/// selects the CLI class (and so the exit code): a capacity block is not an
/// invalid configuration, and a server fault is not the operator's input. The
/// server's own code prefixes the message so it stays visible either way.
pub(crate) fn refusal(status: reqwest::StatusCode, value: &Value) -> StructuredError {
    let server = value["error"]["code"].as_str().unwrap_or("");
    let code = match server {
        _ if status == reqwest::StatusCode::UNAUTHORIZED => "unauthorized",
        "capacity_blocked" | "startup_requires_empty_host" => "insufficient_resources",
        // Owner decision 2026-09-25: no allowed host is eligible for
        // placement; neither a capacity block nor the operator's input.
        "host_ineligible" => "host_ineligible",
        // ADR 0018 §7: the deploy named a runtime profile no allowed host
        // publishes; nothing was stored.
        "profile_not_published" => "profile_not_published",
        "reconciliation_required" => "unreconciled",
        "unsupported_capability" => "unsupported",
        "not_found" => "not_found",
        "invalid_config" | "invalid_request" | "body_too_large" => "invalid_config",
        // A command whose response deadline passed may still complete; the
        // retry with the same request identity decides (SPEC §6.4).
        "deadline_exceeded" => "management_unavailable",
        "internal" => "internal",
        _ if status.is_server_error() && server.is_empty() => "internal",
        _ => "command_rejected",
    };
    let message = value["error"]["message"]
        .as_str()
        .unwrap_or("Management command rejected");
    // ADR 0014 §7: a start without `--wait` stays asynchronous; the refusal
    // says which command waits for the measurement.
    let message = if server == "checkpoint_digest_pending" || server == "model_source_pending" {
        format!("{message}; run the start again with --wait (`mllm start deployment <name> --wait`), which waits for the measurement and then starts")
    } else {
        message.to_owned()
    };
    if server.is_empty() {
        error(code, message)
    } else {
        error(code, format!("{server}: {message}"))
    }
}

/// ADR 0008: whether activation still waits for a declared remote source: no
/// host holds a verified copy and some attempt is not failed. A failed source
/// ends the wait, and the start that follows reports it (`model_source_failed`).
fn model_source_pending(deployment: &Value) -> bool {
    let Some(sources) = deployment["model_sources"].as_array() else {
        return false;
    };
    !sources.is_empty()
        && !sources.iter().any(|source| source["state"] == "verified")
        && sources.iter().any(|source| source["state"] != "failed")
}

fn deployment<'a>(snapshot: &'a Value, id: &str) -> Result<&'a Value, StructuredError> {
    snapshot["deployments"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item["id"] == id || item["name"] == id)
        })
        .ok_or_else(|| error("not_found", "Deployment not found"))
}

pub async fn execute(command: &Command, state_dir: &Path) -> Result<Value, StructuredError> {
    execute_with_context(command, state_dir, None, None).await
}
pub async fn execute_with_context(
    command: &Command,
    state_dir: &Path,
    config: Option<&Path>,
    request_id: Option<&str>,
) -> Result<Value, StructuredError> {
    execute_with_options(command, state_dir, config, request_id, None).await
}

/// ADR 0014 amendment A1: as [`execute_with_context`], with the operator's
/// `--initialize-timeout` (milliseconds) for a start.
pub async fn execute_with_options(
    command: &Command,
    state_dir: &Path,
    config: Option<&Path>,
    request_id: Option<&str>,
    initialize_timeout_ms: Option<i64>,
) -> Result<Value, StructuredError> {
    execute_with_evict(
        command,
        state_dir,
        config,
        request_id,
        initialize_timeout_ms,
        false,
    )
    .await
}

/// Owner decision 2026-09-23: as [`execute_with_options`], with `--evict` for
/// `start deployment` and `start instance`.
pub async fn execute_with_evict(
    command: &Command,
    state_dir: &Path,
    config: Option<&Path>,
    request_id: Option<&str>,
    initialize_timeout_ms: Option<i64>,
    evict: bool,
) -> Result<Value, StructuredError> {
    execute_with_start_options(
        command,
        state_dir,
        config,
        request_id,
        initialize_timeout_ms,
        evict,
        false,
    )
    .await
}

/// The management endpoint, its admin credential and the request journal
/// root: the server's (`--config`) or the standalone role's.
fn management_context(
    state_dir: &Path,
    config: Option<&Path>,
) -> Result<(String, String, std::path::PathBuf), StructuredError> {
    Ok(if let Some(path) = config {
        let config = crate::remote_roles::server_context(Some(path), state_dir)?;
        let (endpoint, token) = crate::remote_roles::management_context(&config)?;
        (endpoint, token, config.state_dir)
    } else {
        let credentials =
            std::fs::read_to_string(state_dir.join("identity/credentials")).map_err(|_| {
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
        // SPEC §16.5: the standalone management listener, at its loopback
        // default unless this run names another loopback address.
        let endpoint = match std::env::var_os(crate::roles::MANAGEMENT_ADDR_ENV) {
            None => ENDPOINT.to_owned(),
            Some(_) => format!(
                "http://{}/management/v1",
                crate::roles::standalone_management_address()
                    .map_err(|failure| error("invalid_config", failure.to_string()))?
            ),
        };
        (endpoint, token, state_dir.to_owned())
    })
}

/// Owner decision 2026-09-25: host names for a table view, from the host
/// inventory. Best effort: an unreachable or older server, or a role without
/// an inventory, yields no names and the table shows host ids.
pub async fn host_names(state_dir: &Path, config: Option<&Path>) -> crate::table::HostNames {
    let Ok((endpoint, token, _)) = management_context(state_dir, config) else {
        return Default::default();
    };
    let Ok(client) = Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
    else {
        return Default::default();
    };
    let Ok(response) = client
        .get(format!("{endpoint}/hosts"))
        .bearer_auth(token)
        .send()
        .await
    else {
        return Default::default();
    };
    if !response.status().is_success() {
        return Default::default();
    }
    match response.json::<Value>().await {
        Ok(inventory) => crate::table::host_names(&inventory),
        Err(_) => Default::default(),
    }
}

/// SPEC §6.4: as [`execute_with_evict`], with `--wait` for `start deployment`
/// and `start instance`: the start's operation is observed to its end, and a
/// failure reports the reason and hint status shows for it.
pub async fn execute_with_start_options(
    command: &Command,
    state_dir: &Path,
    config: Option<&Path>,
    request_id: Option<&str>,
    initialize_timeout_ms: Option<i64>,
    evict: bool,
    wait_start: bool,
) -> Result<Value, StructuredError> {
    let (endpoint, token, journal_root) = management_context(state_dir, config)?;
    let mut api = Management {
        client: Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| error("internal", "Cannot build management client"))?,
        token,
        endpoint,
        journal: None,
        initialize_timeout_ms,
        evict,
        wait_start,
    };
    match command {
        Command::Deploy {
            file: Some(file),
            revision: Some(expected),
            hf_endpoint,
            ..
        } => {
            // SPEC §14: an explicit, revision-aware update of the deployment
            // the file names. The server refuses a stale revision
            // (`revision_conflict`) and replays an exact retry by request id.
            let config =
                read_deployment_file(file, hf_endpoint.as_deref(), state_dir, config).await?;
            let name = config["name"]
                .as_str()
                .ok_or_else(|| error("invalid_config", "Deployment file names no deployment"))?
                .to_owned();
            api.begin_request(
                &journal_root,
                request_id,
                json!({"command":"revise","config":config,"expected_revision":expected}),
            )?;
            let path = match api
                .journal
                .as_ref()
                .and_then(|j| j.saved("revise").ok().flatten())
            {
                Some(saved) => saved.path,
                None => {
                    let snapshot = api.snapshot().await?;
                    let id = deployment(&snapshot, &name)?["id"]
                        .as_str()
                        .ok_or_else(|| error("internal", "Missing deployment identity"))?
                        .to_owned();
                    format!("/deployments/{id}")
                }
            };
            let (status, value) = api
                .exchange(
                    Method::PUT,
                    &path,
                    Some((
                        "revise",
                        json!({"config": config, "expected_revision": expected}),
                    )),
                )
                .await?;
            if !status.is_success() {
                return Err(refusal(status, &value));
            }
            Ok(value)
        }
        Command::Deploy {
            file: Some(file),
            activate,
            wait,
            revision: None,
            hf_endpoint,
        } => {
            let config =
                read_deployment_file(file, hf_endpoint.as_deref(), state_dir, config).await?;
            api.begin_request(
                &journal_root,
                request_id,
                with_timeout(
                    json!({"command":"deploy","config":config,"activate":activate}),
                    initialize_timeout_ms,
                ),
            )?;
            let receipt = api
                .request(
                    Method::POST,
                    "/deployments",
                    Some(json!({"config": config, "activate": false})),
                )
                .await?;
            if !activate {
                // ADR 0014 §7: the deploy stays asynchronous; say what starts it.
                let mut receipt = receipt;
                if receipt["checkpoint_digest"] == "pending" {
                    let name = config["name"].as_str().unwrap_or("<name>");
                    receipt["notice"] = json!(format!(
                        "the checkpoint digest of {name} is being measured; `mllm start deployment {name} --wait` waits for it and starts the deployment"
                    ));
                }
                return Ok(receipt);
            }
            let id = receipt["deployment_id"]
                .as_str()
                .ok_or_else(|| error("internal", "Missing deployment identity"))?;
            // ADR 0014 §7: `--activate` waits for a new checkpoint's digest.
            api.await_activation_inputs(id).await?;
            let (action, deadline) = api.action(id, "start").await?;
            if *wait {
                api.wait_all(action, deadline).await
            } else {
                Ok(action)
            }
        }
        Command::Deploy { .. } => Err(error(
            "invalid_config",
            "Use deploy model --file <deployment.yaml>",
        )),
        Command::Lifecycle {
            action,
            deployment: id,
        } => {
            // SPEC §6.3 (W5): park and preinitialize are deployment actions
            // like start and stop.
            let action = match action {
                LifecycleAction::Start => "start",
                LifecycleAction::Stop => "stop",
                LifecycleAction::Park => "park",
                LifecycleAction::Preinitialize => "preinitialize",
            };
            api.begin_request(
                &journal_root,
                request_id,
                with_evict(
                    with_timeout(
                        json!({"command":action,"deployment":id}),
                        initialize_timeout_ms,
                    ),
                    evict && action == "start",
                ),
            )?;
            // ADR 0014 §7: `start --wait` waits for a new checkpoint's digest.
            if api.wait_start && action == "start" {
                api.await_activation_inputs(id).await?;
            }
            let (receipt, deadline) = api.action(id, action).await?;
            if api.wait_start && action == "start" {
                return api.wait_all(receipt, deadline).await;
            }
            Ok(receipt)
        }
        // SPEC §6.3 (W6): removes the routes and the deployment after verified
        // cleanup. Plain, it is refused while anything is still held; with
        // `--stop` it first stops every instance and waits for that cleanup.
        Command::Delete {
            deployment: id,
            stop: false,
        } => {
            api.begin_request(
                &journal_root,
                request_id,
                json!({"command":"delete","deployment":id}),
            )?;
            api.action(id, "delete").await.map(|(receipt, _)| receipt)
        }
        Command::Delete {
            deployment: id,
            stop: true,
        } => {
            api.begin_request(
                &journal_root,
                request_id,
                json!({"command":"delete","deployment":id,"stop":true}),
            )?;
            delete::delete_after_stop(&api, id).await
        }
        Command::InstanceLifecycle {
            action,
            deployment: id,
            instance,
        } => {
            let action = match action {
                LifecycleAction::Start => "start",
                LifecycleAction::Stop => "stop",
                _ => return Err(error("unsupported", "Lifecycle action unavailable")),
            };
            api.begin_request(
                &journal_root,
                request_id,
                with_evict(
                    with_timeout(
                        json!({"command":action,"deployment":id,"instance":instance}),
                        initialize_timeout_ms,
                    ),
                    evict && action == "start",
                ),
            )?;
            // ADR 0014 §7: `start --wait` waits for a new checkpoint's digest.
            if api.wait_start && action == "start" {
                api.await_activation_inputs(id).await?;
            }
            let (receipt, deadline) = api.action_on(id, action, Some(*instance)).await?;
            if api.wait_start && action == "start" {
                return api.wait(receipt, deadline).await;
            }
            Ok(receipt)
        }
        Command::Status {
            deployment: id,
            watch: false,
        }
        | Command::Inspect {
            resource: Resource::Deployment,
            id: Some(id),
            effective: false,
        } => {
            let mut view = deployment(&api.snapshot().await?, id)?.clone();
            // ADR 0008 (owner decision 2026-09-23): the embedded host's engine
            // installation (registered fingerprint, drift). A server that does
            // not report one leaves the view as it was.
            if let Ok((status, body)) = api.exchange(Method::GET, "/installation", None).await {
                if status.is_success() && body["installation"].is_object() {
                    view["installation"] = body["installation"].clone();
                }
            }
            // Design §9: the inference listener's bind and authentication,
            // so status repeats the start warning. A server that does not
            // report one leaves the view as it was.
            if let Ok((status, body)) = api.exchange(Method::GET, "/inference-listener", None).await
            {
                if status.is_success() && body["inference_listener"].is_object() {
                    view["inference_listener"] = body["inference_listener"].clone();
                }
            }
            // SPEC §17 (M80): the deployment's latency distributions (router,
            // host ingress and engine tiers). A server without the view, or an
            // id the filter would refuse, leaves the view as it was.
            // The view is keyed by deployment id, so a status by name asks
            // for the id it resolved to (found live 2026-09-24, M80).
            if let Some(path) = latency_path(&view) {
                if let Ok((status, body)) = api.exchange(Method::GET, &path, None).await {
                    if status.is_success() {
                        if let Some(latency) = latency_of(&view, &body) {
                            view["latency"] = latency;
                        }
                    }
                }
            }
            Ok(view)
        }
        // SPEC §8.2: the resolved configuration and its provenance, with
        // secrets redacted by the server.
        Command::Inspect {
            resource: Resource::Deployment,
            id: Some(id),
            effective: true,
        } => {
            let snapshot = api.snapshot().await?;
            let deployment_id = deployment(&snapshot, id)?["id"]
                .as_str()
                .ok_or_else(|| error("internal", "Deployment has no id"))?
                .to_owned();
            let path = format!("/deployments/{deployment_id}/effective-config");
            let (status, body) = api.exchange(Method::GET, &path, None).await?;
            if !status.is_success() {
                return Err(refusal(status, &body));
            }
            Ok(body)
        }
        Command::List {
            resource: ListResource::Deployments,
        } => Ok(api.snapshot().await?["deployments"].clone()),
        _ => Err(error(
            "unsupported",
            "Requested management view is not implemented",
        )),
    }
}

/// A deployment document from `--file`, bounded and strictly parsed, with
/// the defaults a minimal file leaves out (owner decision 2026-09-25,
/// `crate::deployment_file`).
async fn read_deployment_file(
    file: &std::path::Path,
    hf_endpoint: Option<&str>,
    state_dir: &Path,
    config: Option<&Path>,
) -> Result<Value, StructuredError> {
    // Owner rule 2026-09-25: `--hf-endpoint` > `MLLM_HF_ENDPOINT` >
    // `HF_ENDPOINT` > the role document's `model_sources.huggingface_endpoint`
    // > Hugging Face.
    let endpoint = crate::deployment_file::pin_endpoint(hf_endpoint, state_dir, config)?;
    crate::deployment_file::prepare(&crate::deployment_file::read_text(file)?, &endpoint).await
}

/// SPEC §17 (M80): the latency view query for a resolved deployment view, by
/// its id; `None` when the view carries no id the filter would accept.
fn latency_path(view: &Value) -> Option<String> {
    let id = view["id"].as_str()?;
    (!id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)))
    .then(|| format!("/metrics/latency?deployment={id}"))
}

/// The view's own entry in a latency report, matched by deployment id.
fn latency_of(view: &Value, body: &Value) -> Option<Value> {
    let id = view["id"].as_str()?;
    body["deployments"]
        .as_array()?
        .iter()
        .find(|d| d["deployment_id"] == id)
        .map(|entry| entry["instances"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(timeouts: Value) -> Value {
        json!({"id": "d", "timeouts": timeouts})
    }

    /// ADR 0014 amendment A1: a start takes the deployment's Initialize
    /// timeout, an override wins within the request deadline, and a stop takes
    /// the Stop window, which never exceeds the request deadline.
    // T20
    #[test]
    fn a_start_takes_the_deployment_timeout_unless_overridden() {
        let d = status(
            json!({"request_deadline_ms": 600_000, "initialize_ms": 300_000, "stop_ms": 600_000}),
        );
        assert_eq!(window(&d, "start", None).unwrap(), 300_000);
        assert_eq!(window(&d, "start", Some(540_000)).unwrap(), 540_000);
        assert_eq!(window(&d, "stop", Some(540_000)).unwrap(), 600_000);
        let refused = window(&d, "start", Some(600_001)).unwrap_err();
        assert_eq!(refused.code, "invalid_config");
    }

    /// A server that reports no timeouts keeps the previous fixed window.
    // T08
    #[test]
    fn an_older_server_keeps_the_previous_window() {
        let d = json!({"id": "d"});
        assert_eq!(window(&d, "start", None).unwrap(), LEGACY_WINDOW_MS);
        assert_eq!(window(&d, "stop", None).unwrap(), LEGACY_WINDOW_MS);
        assert_eq!(window(&d, "start", Some(1_200_000)).unwrap(), 1_200_000);
    }

    /// SPEC §17 (found live 2026-09-24, M80): `status deployment <name>` asked
    /// the latency view for the name, which the server keys by deployment id,
    /// so every live M80 cell recorded no router, ingress or engine series.
    // T40
    #[test]
    fn the_latency_view_is_read_by_the_deployment_id_even_when_named() {
        let view = json!({"id": "01M38MBH6X88H5YSYEWH1QSYZ7", "name": "sb-4-bn"});
        assert_eq!(
            latency_path(&view).as_deref(),
            Some("/metrics/latency?deployment=01M38MBH6X88H5YSYEWH1QSYZ7")
        );
        let body = json!({"deployments": [
            {"deployment_id": "other", "instances": [{"instance": 9}]},
            {"deployment_id": "01M38MBH6X88H5YSYEWH1QSYZ7", "instances": [{"instance": 0}]}
        ]});
        assert_eq!(latency_of(&view, &body), Some(json!([{"instance": 0}])));
        // A view without a well-formed id asks nothing.
        assert_eq!(latency_path(&json!({"name": "x"})), None);
        assert_eq!(latency_path(&json!({"id": "a b"})), None);
    }

    /// SPEC §14: a management refusal keeps its structured meaning. A capacity
    /// block, an unreconciled owner or a server fault must not exit as an
    /// invalid configuration, and the server's own code stays visible.
    // T08 T29
    #[test]
    fn a_refusal_keeps_the_servers_error_class() {
        use crate::output::ExitCode;
        use reqwest::StatusCode;
        let body = |code: &str| json!({"error": {"code": code, "message": "why"}});
        let cases = [
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "capacity_blocked",
                ExitCode::INSUFFICIENT_RESOURCES,
            ),
            (
                StatusCode::CONFLICT,
                "startup_requires_empty_host",
                ExitCode::INSUFFICIENT_RESOURCES,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "reconciliation_required",
                ExitCode::UNRECONCILED,
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                ExitCode::INTERNAL,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "unsupported_capability",
                ExitCode::UNSUPPORTED,
            ),
            (
                StatusCode::GATEWAY_TIMEOUT,
                "deadline_exceeded",
                ExitCode::INTERNAL,
            ),
            (StatusCode::NOT_FOUND, "not_found", ExitCode::INVALID_CONFIG),
            (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                ExitCode::UNAUTHORIZED,
            ),
            (
                StatusCode::CONFLICT,
                "revision_conflict",
                ExitCode::INVALID_CONFIG,
            ),
            // Owner decision 2026-09-25: not a capacity block, not bad input.
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "host_ineligible",
                ExitCode::HOST_INELIGIBLE,
            ),
        ];
        for (status, code, exit) in cases {
            let refused = refusal(status, &body(code));
            assert_eq!(refused.exit_code(), exit, "{code}");
            assert!(
                refused.message.contains(code),
                "{code}: {}",
                refused.message
            );
            assert!(
                refused.message.contains("why"),
                "{code}: {}",
                refused.message
            );
        }
        // A 5xx without a recognised code is a server fault, not bad input.
        let bare = refusal(StatusCode::BAD_GATEWAY, &json!({}));
        assert_eq!(bare.exit_code(), ExitCode::INTERNAL);
    }

    /// SPEC §13: the override is part of a recovered command's intent.
    // T08
    #[test]
    fn the_override_is_part_of_the_intent_only_when_given() {
        let base = json!({"command": "start", "deployment": "d"});
        assert_eq!(with_timeout(base.clone(), None), base);
        assert_eq!(
            with_timeout(base, Some(60_000))["initialize_timeout_ms"],
            60_000
        );
    }

    /// SPEC §6.4: `--wait` reports why the operation it waited on failed:
    /// the reason and hint status shows for that operation, from the
    /// deployment's or an instance's latest operation; otherwise its closed
    /// error code; otherwise only where to look.
    // T08 T29
    #[test]
    fn a_failed_wait_prints_the_reason_and_hint() {
        let snapshot = json!({
            "deployments": [{"id": "d", "latest_operation": {"id": "other", "state": "running"},
                "instances": [{"index": 0, "latest_operation": {"id": "op", "kind": "initialize", "state": "failed",
                    "error_code": "launch_failed",
                    "reason": "host policy refused the launch before any effect: capability_missing:deep_park",
                    "hint": "declare residency restart_only"}}]}],
            "operations": [{"id": "op", "state": "failed", "error_code": "launch_failed"},
                           {"id": "bare", "state": "failed", "error_code": "startup_requires_empty_host"},
                           {"id": "silent", "state": "failed"}]
        });
        let message = failure_message(&snapshot, "op");
        assert!(
            message.contains("capability_missing:deep_park"),
            "{message}"
        );
        assert!(
            message.contains("hint: declare residency restart_only"),
            "{message}"
        );
        let bare = failure_message(&snapshot, "bare");
        assert!(bare.contains("startup_requires_empty_host"), "{bare}");
        assert!(bare.contains("--evict"), "{bare}");
        assert_eq!(
            failure_message(&snapshot, "silent"),
            "Operation silent failed; inspect deployment status"
        );
    }

    /// SPEC §6.4: `--wait` observes its operation through transient status
    /// failures. Two 503s from the snapshot are retried with backoff and the
    /// wait still reports the succeeded operation; a non-retryable refusal
    /// ends it at once.
    // T08 T38
    #[tokio::test]
    async fn wait_retries_a_transient_snapshot_failure() {
        use axum::{routing, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = reads.clone();
        let app = Router::new()
            .route(
                "/management/v1/snapshot",
                routing::get(move || {
                    let read = counted.fetch_add(1, Ordering::SeqCst) + 1;
                    async move {
                        match read {
                            1 | 2 => (
                                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({"error": {"code": "snapshot_unavailable", "retryable": true}})),
                            ),
                            _ => (
                                axum::http::StatusCode::OK,
                                Json(json!({
                                    "operations": [{"id": "op", "state": "succeeded"}],
                                    "deployments": [{"id": "d", "observed_state": "ready"}],
                                })),
                            ),
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let api = Management {
            client: Client::builder().no_proxy().build().unwrap(),
            token: "token".into(),
            endpoint: format!("http://{address}/management/v1"),
            journal: None,
            initialize_timeout_ms: None,
            evict: false,
            wait_start: true,
        };
        let deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
            + 30_000;
        let receipt = json!({"operation_id": "op", "deployment_id": "d"});
        let done = api.wait(receipt, deadline).await.unwrap();
        assert_eq!(done["deployment"]["observed_state"], "ready");
        assert_eq!(reads.load(Ordering::SeqCst), 3);

        // A refusal that is not transient ends the wait at once.
        let denied = Router::new().route(
            "/management/v1/snapshot",
            routing::get(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    Json(json!({"error": {"code": "unauthenticated", "retryable": false}})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, denied).await.unwrap() });
        let api = Management {
            endpoint: format!("http://{address}/management/v1"),
            ..api
        };
        let refused = api
            .wait(
                json!({"operation_id": "op", "deployment_id": "d"}),
                deadline,
            )
            .await
            .unwrap_err();
        assert_eq!(refused.code, "unauthorized");
    }
}

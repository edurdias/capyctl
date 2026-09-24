//! Chat dispatch: resolve the alias to an explicit deployment, admit, and
//! forward through the deployment's adapter (F1 design §5). Admission
//! joins one activation operation when the deployment is not READY
//! (simultaneous requests join one wake — T15).

use async_trait::async_trait;

use axum::http::StatusCode;
use axum::Json;

use crate::forwarders::ForwarderError;
use crate::RouterDeps;
use mllm_adapters::traits::AdapterError;
use mllm_controller::{LeaseEnd, LeaseRefused, RequestLease};
use crate::admission::StaticStreamGuard;
use crate::timing::RequestTiming;

/// Resolve the alias to an explicit deployment (no model-name guessing —
/// SPEC §10) and ensure READY, joining the single activation operation.
/// Returns the deployment id.
///
/// The profile kind used to be returned with it, to pick a forwarder from a
/// boot-time table. Dispatch now asks the lifecycle authority where this
/// deployment's engine is, so the family name is no longer part of the answer.
pub async fn resolve(
    deps: &RouterDeps,
    model: &str,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    resolve_waiting(deps, model, 0).await
}

/// SPEC §10 steps 1–2 (W10): resolve the route and, when no instance of its
/// deployment serves, queue the request (holding `body_bytes` against the
/// buffered-bytes bound) and join the deployment's one activation, which the
/// lifecycle authority drives through any transition in progress and any
/// switch it needs. The activation runs detached: a client that disconnects
/// while queued releases only its own slot. The wait is bounded by the queue
/// deadline, which includes activation.
pub async fn resolve_waiting(
    deps: &RouterDeps,
    model: &str,
    body_bytes: usize,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    resolve_timed(deps, model, body_bytes, &mut RequestTiming::untracked()).await
}

/// As [`resolve_waiting`], recording the deployment, its engine family and the
/// time spent waiting on `timing` (SPEC §17).
pub async fn resolve_timed(
    deps: &RouterDeps,
    model: &str,
    body_bytes: usize,
    timing: &mut RequestTiming,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    resolve_held(deps, model, body_bytes, tokio::time::Instant::now(), timing)
        .await
        .map(|(deployment, _)| deployment)
}

/// As [`resolve_timed`], returning the waiting-queue ticket the request took
/// if it had to wait, so a request that goes on to wait for an in-flight slot
/// keeps its place against the queue's bounds. The wait ends at the request's
/// queue deadline counted from `received` (SPEC §10: activation included).
async fn resolve_held(
    deps: &RouterDeps,
    model: &str,
    body_bytes: usize,
    received: tokio::time::Instant,
    timing: &mut RequestTiming,
) -> Result<(String, Option<crate::queue::WaitTicket>), (StatusCode, Json<serde_json::Value>)> {
    let row = deps
        .controller
        .find_deployment_by_route(model)
        .map_err(|e| err("internal", &format!("store: {e}")))?
        .ok_or_else(|| {
            err(
                "unknown_model",
                &format!("no deployment serves model id {model}"),
            )
        })?;
    let deployment_id = row.id.clone();
    timing.resolved(&deployment_id, &row.kind);
    if serves_now(deps, &deployment_id, row.observed_state)? {
        timing.queued(std::time::Duration::ZERO, None);
        return Ok((deployment_id, None));
    }
    // SPEC §17: the W10 wait, measured from entering the queue.
    let waited = std::time::Instant::now();
    let ticket = enter_queue(deps, &deployment_id, body_bytes)?;
    let deadline = received + deps.inflight.waiting.limits().deadline;
    let controller = deps.controller.clone();
    let id = deployment_id.clone();
    let joined = deps.activation_join.join_detached(&deployment_id, move || async move {
        controller
            .activate_for_request(&id)
            .await
            .map(|()| 0)
            .map_err(map_controller)
    }, err("activation_uncertain", "the activation task ended without an outcome; retry shortly"));
    let joined_at = std::time::Instant::now();
    match tokio::time::timeout_at(deadline, joined).await {
        Ok(Ok(_)) => {
            timing.queued(waited.elapsed(), Some(joined_at.elapsed()));
            Ok((deployment_id, Some(ticket)))
        }
        Ok(Err(refusal)) => Err(refusal),
        // The activation keeps running for whoever waits next; this request
        // gives up without having reached any engine.
        Err(_) => Err(err(
            "unavailable",
            &format!(
                "deployment {deployment_id} did not become servable within the queue deadline; retry shortly"
            ),
        )),
    }
}

/// SPEC §10 step 1 (T19): hold a place in the bounded waiting queue.
fn enter_queue(
    deps: &RouterDeps,
    deployment_id: &str,
    body_bytes: usize,
) -> Result<crate::queue::WaitTicket, (StatusCode, Json<serde_json::Value>)> {
    deps.inflight
        .waiting
        .enter(deployment_id, body_bytes)
        .map_err(|refusal| match refusal {
            crate::queue::WaitRefusal::Full => err(
                "queue_full",
                &format!("too many requests are waiting for deployment {deployment_id}"),
            ),
            crate::queue::WaitRefusal::Bytes => err(
                "queue_full",
                "waiting requests exceed the buffered-bytes bound",
            ),
        })
}

/// SPEC §10 steps 1–2 and T19: resolve the route, wait for the deployment to
/// be servable, then take one of its in-flight slots. A request that finds the
/// bound reached waits for a slot in arrival order, holding its place in the
/// bounded waiting queue, until its queue deadline counted from `received`;
/// it is never refused at once after having waited, and a fresh arrival never
/// takes a slot ahead of a request already waiting for one.
pub async fn admit_timed(
    deps: &RouterDeps,
    model: &str,
    body_bytes: usize,
    received: tokio::time::Instant,
    timing: &mut RequestTiming,
) -> Result<(String, StaticStreamGuard), (StatusCode, Json<serde_json::Value>)> {
    let (deployment_id, ticket) = resolve_held(deps, model, body_bytes, received, timing).await?;
    let max = deps.limits.max_requests_per_deployment;
    if let Some(guard) = deps.inflight.try_guard_arc(&deployment_id, max) {
        return Ok((deployment_id, guard));
    }
    let _ticket = match ticket {
        Some(ticket) => ticket,
        None => enter_queue(deps, &deployment_id, body_bytes)?,
    };
    let deadline = received + deps.inflight.waiting.limits().deadline;
    match deps.inflight.acquire_arc(&deployment_id, max, deadline).await {
        Some(guard) => Ok((deployment_id, guard)),
        None => Err(err(
            "queue_full",
            &format!(
                "deployment {deployment_id} stayed at its in-flight bound for the queue deadline"
            ),
        )),
    }
}

/// Whether a request can go straight to dispatch: the deployment is READY and,
/// when the authority reports instances, one of them is open or closed for a
/// reason waiting would not fix (a lost or frozen host, an exited engine —
/// those are answered by the instance planner as before).
fn serves_now(
    deps: &RouterDeps,
    deployment: &str,
    observed: mllm_domain::LifecycleState,
) -> Result<bool, (StatusCode, Json<serde_json::Value>)> {
    if observed != mllm_domain::LifecycleState::Ready {
        return Ok(false);
    }
    let Some(instances) = deps
        .controller
        .serving_instances(deployment)
        .map_err(map_controller)?
    else {
        return Ok(true);
    };
    // SPEC §6.1: every instance closed only by its own gate (DRAINING,
    // PARKING, a switch) queues; any other state dispatches or is refused
    // by the planner.
    Ok(!instances.is_empty()
        && instances.iter().any(|i| {
            i.dispatch_open || i.host_unresponsive || i.engine_exited || !i.host_live
        }))
}

/// Non-streaming dispatch: resolve + forward through the deployment's
/// adapter; the in-flight guard releases on completion (accounting is
/// conservative on every other path).
///
/// SPEC §10 (owner decision 2026-09-22): a durable request lease is opened
/// before anything reaches the engine and closed only on evidence. The forward
/// runs as its own task, so a client that disconnects does not cancel it: the
/// backend's completion is still observed and closes the lease, instead of a
/// dropped future leaving the lease charged forever.
pub async fn dispatch(
    deps: &RouterDeps,
    model: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let bytes = body.to_string().len();
    dispatch_timed(
        deps,
        model,
        body.clone(),
        bytes,
        tokio::time::Instant::now(),
        RequestTiming::untracked(),
    )
    .await
    .map(|(response, _)| response)
}

/// As [`dispatch`], timing the request (SPEC §17). Returns the response with
/// the request's completed clock, for its timing header. `body_bytes` is the
/// size of the request as received and `received` when it arrived: the queue
/// deadline and the backend's first-event bound count from it (SPEC §10).
pub async fn dispatch_timed(
    deps: &RouterDeps,
    model: &str,
    body: serde_json::Value,
    body_bytes: usize,
    received: tokio::time::Instant,
    mut timing: RequestTiming,
) -> Result<(serde_json::Value, RequestTiming), (StatusCode, Json<serde_json::Value>)> {
    // SPEC §10 step 1 and T19: resolve, wait if needed, take an in-flight slot.
    let (deployment_id, guard) =
        admit_timed(deps, model, body_bytes, received, &mut timing).await?;
    // ADR 0013 §10 (I3): choose an instance, open its lease and resolve its
    // forwarder. Nothing has been sent yet: a refusal releases the guard on drop.
    let started = std::time::Instant::now();
    let mut plan = crate::balance::Plan::new(deps, &deployment_id)?;
    timing.selected(started.elapsed());
    let started = std::time::Instant::now();
    let first = plan.next().await?;
    timing.leased(started.elapsed());
    // SPEC §10: the collected response is bounded like a relayed stream (the
    // request deadline for its first backend event, then the idle bound), not
    // by a fixed wall-clock cap.
    let bounds = crate::stream::StreamBounds::for_request(received, &deps.inflight.waiting.limits());
    // From the first poll onward the engine may have accepted work. Dropping
    // this request or receiving an uncertain transport error cannot free it.
    let guard = guard.abandon();
    let dispatched = tokio::spawn(async move {
        let mut attempt = first;
        loop {
            let generation = attempt.generation;
            timing.forwarding(attempt.instance, generation);
            let progress = crate::stream::Progress::default();
            let mut observer = crate::stream::ProgressOnly(progress.clone());
            let result = crate::stream::bounded(
                attempt.forward.forward_chat_observed(&body, &mut observer),
                &progress,
                &bounds,
            )
            .await
            .unwrap_or_else(|()| {
                Err(AdapterError::Uncertain(
                    "the backend missed the request's deadline or idle bound".into(),
                ))
            });
            if result.is_ok() {
                timing.response();
            }
            let end = lease_end(result.as_ref().err());
            let durable = attempt.settle(end).await;
            match result {
                // SPEC §10, T38: nothing reached the engine, so the request may
                // be offered to the next instance; never after acceptance.
                Err(AdapterError::NotAccepted(reason)) => {
                    plan.refused(generation, &reason);
                    let started = std::time::Instant::now();
                    let next = plan.next().await;
                    timing.leased(started.elapsed());
                    match next {
                        Ok(next) => attempt = next,
                        Err(refusal) => {
                            guard.release();
                            return Err(Refused::Before(refusal));
                        }
                    }
                }
                Ok(response) => {
                    guard.release();
                    timing.finish();
                    return Ok((response, timing));
                }
                // SPEC §10, T19: refused deterministically before sending;
                // another instance would refuse it the same way.
                Err(error) if refused_before_sending(&error) => {
                    guard.release();
                    return Err(Refused::Before(adapter_refusal(&error)));
                }
                // Anything else may have been accepted. SPEC §10, T17: the
                // durable lease keeps the conservative charge (uncertain until
                // reconciled), so the in-memory slot — a per-process admission
                // count — is released; left taken, every uncertain end would
                // shrink the deployment's bound for the life of the process.
                // Only an authority with no durable ledger keeps it taken.
                Err(_) => {
                    if durable {
                        guard.release();
                    }
                    return Err(Refused::Uncertain);
                }
            }
        }
    });
    match dispatched.await {
        Ok(Ok(response)) => Ok(response),
        // T17 T38: every offer was refused before forwarding (a host shutting
        // down, a refused connection, a gate that closed). Nothing reached an
        // engine, so this is a retryable 503 with each lease already closed.
        Ok(Err(Refused::Before(refusal))) => Err(refusal),
        Ok(Err(Refused::Uncertain)) | Err(_) => {
            Err(err("engine_error", "backend completion unverified"))
        }
    }
}

/// How a dispatch that produced no response ended.
enum Refused {
    /// Refused before anything reached an engine, with the answer to give.
    Before((StatusCode, Json<serde_json::Value>)),
    /// The engine may have accepted it.
    Uncertain,
}

/// The router's error answer, for the instance planner.
pub(crate) fn refusal(code: &str, message: &str) -> (StatusCode, Json<serde_json::Value>) {
    err(code, message)
}

/// How the evidence from one forward ends its lease.
pub(crate) fn lease_end(error: Option<&AdapterError>) -> LeaseEnd {
    match error {
        None => LeaseEnd::Completed,
        Some(AdapterError::NotAccepted(_)) => LeaseEnd::NotAccepted,
        // SPEC §10: a request the forwarder refused by policy or capability was
        // never sent; that is evidence of non-acceptance, not uncertainty.
        Some(error) if refused_before_sending(error) => LeaseEnd::NotAccepted,
        Some(_) => LeaseEnd::Uncertain,
    }
}

/// Whether a forward error is a deterministic refusal decided before anything
/// was sent (the request's fields, or a capability the forwarder lacks).
pub(crate) fn refused_before_sending(error: &AdapterError) -> bool {
    matches!(
        error,
        AdapterError::PolicyDenied
            | AdapterError::UnsupportedCapability
            | AdapterError::UnsupportedCombination
    )
}

/// The client answer for a refusal decided before sending.
pub(crate) fn adapter_refusal(error: &AdapterError) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        AdapterError::PolicyDenied => err("invalid_request", "the request carries a field that is not accepted"),
        _ => err("unsupported", &format!("{error}")),
    }
}

/// SPEC §10, T19 T21: refuse a request whose fields cannot be forwarded, before
/// any admission accounting, activation or engine is touched.
pub fn validate_request(body: &serde_json::Value) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    use mllm_adapters::forward::ChatRequestRefusal as R;
    mllm_adapters::forward::validate_chat_request(body).map_err(|refusal| match refusal {
        R::Malformed(_) | R::Field(_) => err("invalid_request", &refusal.to_string()),
        R::Unsupported(_) => err("unsupported_parameter", &refusal.to_string()),
    })
}

/// Open the durable lease for one dispatch (SPEC §10). `None` only from an
/// authority that keeps no ledger at all.
pub(crate) async fn open_lease(
    deps: &RouterDeps,
    deployment: &str,
) -> Result<Option<RequestLease>, (StatusCode, Json<serde_json::Value>)> {
    deps.controller
        .open_request_lease(deployment, deps.limits.max_requests_per_deployment)
        .await
        .map_err(|refused| match refused {
            // SPEC §§6.1, 13.2: the gate closed (a host draining or re-proving
            // readiness, a stop in progress). Retryable, never a 500.
            LeaseRefused::Closed => err(
                "unavailable",
                &format!("dispatch to deployment {deployment} is closed; retry shortly"),
            ),
            LeaseRefused::Full => err("queue_full", "outstanding request bound reached"),
            LeaseRefused::Unavailable(reason) => err("unavailable", &reason),
        })
}

/// Close or retain a lease on the evidence. A failed write leaves it charged,
/// which is the conservative outcome, so it is reported and not retried here.
pub(crate) async fn close_lease(
    controller: &dyn mllm_controller::LifecyclePort,
    lease: Option<RequestLease>,
    end: LeaseEnd,
) {
    if let Some(lease) = lease {
        let id = lease.id().to_owned();
        if let Err(error) = controller.close_request_lease(lease, end).await {
            eprintln!("request lease {id} stays charged: {error}");
        }
    }
}

fn err(code: &str, message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        match code {
            "unknown_model" => StatusCode::NOT_FOUND,
            // SPEC §14: a request that can never be forwarded is the client's.
            "invalid_request" | "unsupported_parameter" => StatusCode::BAD_REQUEST,
            // SPEC §10 (T19): a full queue is transient; 413 is for body size.
            "queue_full" => StatusCode::TOO_MANY_REQUESTS,
            "unsupported" => StatusCode::NOT_IMPLEMENTED,
            "insufficient_resources" => StatusCode::TOO_MANY_REQUESTS,
            "conflict" => StatusCode::CONFLICT,
            // Still in progress as far as anyone can tell: not a failure the client
            // should read as "nothing happened".
            "activation_uncertain" => StatusCode::SERVICE_UNAVAILABLE,
            "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        },
        // SPEC §4.3: every 503 here means "not now", and says so, so a client
        // can tell a restart or a closed gate from a failure.
        Json(match code {
            "activation_uncertain" | "unavailable" | "shutting_down" | "queue_full" => {
                serde_json::json!({ "code": code, "message": message, "retryable": true })
            }
            _ => serde_json::json!({ "code": code, "message": message }),
        }),
    )
}

/// Exhaustive on purpose. The previous catch-all reported every unclassified
/// outcome as an activation failure, which told a client that nothing happened even
/// when the activation was still running. A new fault variant must be a compile
/// error here, not silently absorbed into that claim.
pub(crate) fn map_controller(e: mllm_controller::LifecycleFault) -> (StatusCode, Json<serde_json::Value>) {
    use mllm_controller::LifecycleFault as F;
    match e {
        F::NotFound(d) => err("unknown_model", &format!("deployment {d} vanished")),
        F::Blocked(m) => err("insufficient_resources", &format!("admission blocked: {m}")),
        F::Conflict(m) => err("conflict", &m),
        // The activation may still be running. Saying it failed would invite a
        // client to treat the deployment as untouched.
        F::Uncertain(m) => err("activation_uncertain", &m),
        F::Failed(m) => err("activation_failed", &m),
        F::Unavailable(m) => err("unavailable", &m),
    }
}

/// Exhaustive for the same reason as `map_controller`: each outcome tells the client
/// something different about what is true of the deployment, and a catch-all would
/// flatten "nothing is running" into "the router is broken".
pub fn map_forwarder(e: ForwarderError) -> (StatusCode, Json<serde_json::Value>) {
    match e {
        // The deployment resolved and was made READY, yet no runtime is recorded.
        // That is a transient disagreement between the authority's view and its
        // store, not a client error, so it is retryable rather than a 4xx.
        ForwarderError::NoRuntime(d) => err(
            "unavailable",
            &format!("deployment {d} has no running engine to forward to"),
        ),
        ForwarderError::Authority(fault) => map_controller(fault),
        ForwarderError::Endpoint(d) => err(
            "internal",
            &format!("the runtime recorded for deployment {d} has an unusable endpoint"),
        ),
    }
}

/// Placeholder forwarder for attached deployments: an attached service is
/// routed through its endpoint URL at dispatch time (F3+ wiring); in F1 the
/// attached chat path reports unsupported rather than improvising.
pub struct NoForward;

#[async_trait]
impl mllm_adapters::traits::ChatForward for NoForward {
    async fn forward_chat(
        &self,
        _body: &serde_json::Value,
    ) -> Result<serde_json::Value, mllm_adapters::traits::AdapterError> {
        Err(mllm_adapters::traits::AdapterError::UnsupportedCapability)
    }
}

//! Chat dispatch: resolve the alias to an explicit deployment, admit, and
//! forward through the deployment's adapter (F1 design §5). Admission
//! joins one activation operation when the deployment is not READY
//! (simultaneous requests join one wake — T15).

use async_trait::async_trait;
use std::future::Future;

use axum::http::StatusCode;
use axum::Json;

use crate::admission::StaticStreamGuard;
use crate::balance::InstanceView;
use crate::endpoint::Endpoint;
use crate::forwarders::ForwarderError;
use crate::timing::RequestTiming;
use crate::RouterDeps;
use capyctl_adapters::traits::AdapterError;
use capyctl_controller::{LeaseEnd, LeaseRefused, RequestLease};

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
    resolve_held(
        deps,
        model,
        Endpoint::ChatCompletions,
        body_bytes,
        tokio::time::Instant::now(),
        timing,
    )
    .await
    .map(|(deployment, _)| deployment)
}

/// How [`resolve_held`] found the deployment servable.
enum Resolution {
    /// At once, on this read of its instances.
    Now(InstanceView),
    /// After waiting, holding this place in the waiting queue.
    Waited(crate::queue::WaitTicket),
}

/// As [`resolve_timed`] for `endpoint`, returning the waiting-queue ticket the
/// request took if it had to wait, so a request that goes on to wait for an
/// in-flight slot keeps its place against the queue's bounds, or the instance
/// view that found it servable at once. The wait ends at the request's queue
/// deadline counted from `received` (SPEC §10: activation included).
async fn resolve_held(
    deps: &RouterDeps,
    model: &str,
    endpoint: Endpoint,
    body_bytes: usize,
    received: tokio::time::Instant,
    timing: &mut RequestTiming,
) -> Result<(String, Resolution), (StatusCode, Json<serde_json::Value>)> {
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
    // ADR 0030 §2: the endpoint that does not serve the deployment's API is
    // refused here, before the request waits or anything activates.
    crate::endpoint::check_api(deps, &deployment_id, model, endpoint)?;
    timing.resolved(&deployment_id, &row.kind);
    if let Some(view) = serves_now(deps, &deployment_id, row.observed_state)? {
        timing.queued(std::time::Duration::ZERO, None);
        return Ok((deployment_id, Resolution::Now(view)));
    }
    // SPEC §10 (owner decision 2026-10-08): with no waiting allowed the request
    // is refused at once, but it still starts (or joins) the one activation a
    // waiting request would have joined, so a retry finds the deployment
    // servable. The activation runs detached; this request does not await it.
    // A deployment no request may start (an operator's stop, an explicit
    // activation; SPEC §6.3, §6.5) is answered with that refusal instead: a
    // retry would not help.
    if deps.inflight.waiting.limits().max_pending_per_deployment == 0 {
        deps.controller
            .refuse_inactive(&deployment_id)
            .map_err(map_controller)?;
        let _ = futures::FutureExt::now_or_never(join_activation(deps, &deployment_id));
        return Err(err(
            "queue_full",
            &format!(
                "deployment {deployment_id} is not servable now and lets no request wait; retry shortly"
            ),
        ));
    }
    // SPEC §17: the W10 wait, measured from entering the queue.
    let waited = std::time::Instant::now();
    let ticket = enter_queue(deps, &deployment_id, body_bytes)?;
    let deadline = received + deps.inflight.waiting.limits().deadline;
    let joined = join_activation(deps, &deployment_id);
    let joined_at = std::time::Instant::now();
    match tokio::time::timeout_at(deadline, joined).await {
        Ok(Ok(_)) => {
            timing.queued(waited.elapsed(), Some(joined_at.elapsed()));
            Ok((deployment_id, Resolution::Waited(ticket)))
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

/// SPEC §10 step 2 (T15, W10): join (or start) the deployment's one
/// activation. It runs as its own task, so no caller that stops waiting (a
/// deadline, a disconnect, a refusal) cancels it for the others.
async fn join_activation(
    deps: &RouterDeps,
    deployment_id: &str,
) -> Result<u64, (StatusCode, Json<serde_json::Value>)> {
    let controller = deps.controller.clone();
    let id = deployment_id.to_owned();
    deps.activation_join
        .join_detached(
            deployment_id,
            move || async move {
                controller
                    .activate_for_request(&id)
                    .await
                    .map(|()| 0)
                    .map_err(map_controller)
            },
            err(
                "activation_uncertain",
                "the activation task ended without an outcome; retry shortly",
            ),
        )
        .await
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

/// SPEC §10 steps 1–2 and T19: resolve the route, refuse an `endpoint` the
/// deployment does not answer (ADR 0030 §2), wait for the deployment to be
/// servable, then take one of its in-flight slots. A request that finds the
/// bound reached waits for a slot in arrival order, holding its place in the
/// bounded waiting queue, until its queue deadline counted from `received`;
/// it is never refused at once after having waited, and a fresh arrival never
/// takes a slot ahead of a request already waiting for one.
///
/// A request that waited for nothing also returns the instance view that
/// found its deployment servable, for [`crate::balance::Plan::new`] to rank
/// rather than read again (ADR 0013 §10: the lease grant re-checks the gate,
/// so a view is only ever a hint). After any wait it returns none, and the
/// planner reads the instances afresh.
pub async fn admit_timed(
    deps: &RouterDeps,
    endpoint: Endpoint,
    model: &str,
    body_bytes: usize,
    received: tokio::time::Instant,
    timing: &mut RequestTiming,
) -> Result<(String, StaticStreamGuard, Option<InstanceView>), (StatusCode, Json<serde_json::Value>)>
{
    let (deployment_id, resolution) =
        resolve_held(deps, model, endpoint, body_bytes, received, timing).await?;
    let (ticket, view) = match resolution {
        Resolution::Now(view) => (None, Some(view)),
        Resolution::Waited(ticket) => (Some(ticket), None),
    };
    let max = deps.limits.max_requests_per_deployment;
    if let Some(guard) = deps.inflight.try_guard_arc(&deployment_id, max) {
        return Ok((deployment_id, guard, view));
    }
    let _ticket = match ticket {
        Some(ticket) => ticket,
        // SPEC §10 (owner decision 2026-10-08): no request waits for a slot.
        None if deps.inflight.waiting.limits().max_pending_per_deployment == 0 => {
            return Err(err(
                "queue_full",
                &format!(
                    "deployment {deployment_id} is at its in-flight bound and lets no request wait; retry shortly"
                ),
            ));
        }
        None => enter_queue(deps, &deployment_id, body_bytes)?,
    };
    let deadline = received + deps.inflight.waiting.limits().deadline;
    match deps
        .inflight
        .acquire_arc(&deployment_id, max, deadline)
        .await
    {
        Some(guard) => Ok((deployment_id, guard, None)),
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
/// those are answered by the instance planner as before). When it can, the
/// instance view that decided it.
fn serves_now(
    deps: &RouterDeps,
    deployment: &str,
    observed: capyctl_domain::LifecycleState,
) -> Result<Option<InstanceView>, (StatusCode, Json<serde_json::Value>)> {
    if observed != capyctl_domain::LifecycleState::Ready {
        return Ok(None);
    }
    let Some(instances) = deps
        .controller
        .serving_instances(deployment)
        .map_err(map_controller)?
    else {
        return Ok(Some(InstanceView(None)));
    };
    // SPEC §6.1: every instance closed only by its own gate (DRAINING,
    // PARKING, a switch) queues; any other state dispatches or is refused
    // by the planner.
    let serves = !instances.is_empty()
        && instances
            .iter()
            .any(|i| i.dispatch_open || i.host_unresponsive || i.engine_exited || !i.host_live);
    Ok(serves.then_some(InstanceView(Some(instances))))
}

/// Non-streaming dispatch: resolve + forward through the deployment's
/// adapter; the in-flight guard releases on completion (accounting is
/// conservative on every other path).
///
/// SPEC §10 (owner decision 2026-09-22): a durable request lease is opened
/// before anything reaches the engine and closed only on evidence. The forward
/// runs as its own task, so a dropped handler never leaves the lease charged
/// forever: the task settles it whatever the client does. Amended 2026-10-21:
/// a client that disconnects cancels the forward, which closes the engine
/// connection as for a stream whose client hung up, and the task closes the
/// lease as cancelling.
pub async fn dispatch(
    deps: &RouterDeps,
    model: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let bytes = body.to_string().len();
    dispatch_timed(
        deps,
        Endpoint::ChatCompletions,
        model,
        body.clone().into(),
        bytes,
        tokio::time::Instant::now(),
        RequestTiming::untracked(),
        std::sync::Arc::new(crate::request_id::RequestIds::untracked()),
    )
    .await
    .map(|(response, _)| response)
}

/// As [`dispatch`], for `endpoint`, timing the request (SPEC §17). Returns the
/// response with the request's completed clock, for its timing header.
/// `body_bytes` is the size of the request as received and `received` when it
/// arrived: the queue deadline and the backend's first-event bound count from
/// it (SPEC §10). `ids` are the request's IDs (SPEC §10, owner decision
/// 2026-10-19): its first granted lease names it.
///
/// ADR 0030 §2: a decision request is admitted, leased and settled exactly as
/// a chat completion; only its forward differs.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_timed(
    deps: &RouterDeps,
    endpoint: Endpoint,
    model: &str,
    body: capyctl_adapters::request::InferenceRequest,
    body_bytes: usize,
    received: tokio::time::Instant,
    mut timing: RequestTiming,
    ids: std::sync::Arc<crate::request_id::RequestIds>,
) -> Result<(serde_json::Value, RequestTiming), (StatusCode, Json<serde_json::Value>)> {
    // SPEC §10 step 1 and T19: resolve, wait if needed, take an in-flight slot.
    let (deployment_id, guard, view) =
        admit_timed(deps, endpoint, model, body_bytes, received, &mut timing).await?;
    // ADR 0013 §10 (I3): choose an instance, open its lease and resolve its
    // forwarder. Nothing has been sent yet: a refusal releases the guard on drop.
    let started = std::time::Instant::now();
    let deadline = received + deps.inflight.waiting.limits().deadline;
    let mut plan = crate::balance::Plan::new(deps, &deployment_id, view, ids, deadline)?;
    timing.selected(started.elapsed());
    let started = std::time::Instant::now();
    let first = plan.next().await?;
    timing.leased(started.elapsed());
    // SPEC §10: the collected response is bounded like a relayed stream (the
    // request deadline for its first backend event, then the idle bound), not
    // by a fixed wall-clock cap. ADR 0030 §2: a decision has no first-token
    // event and reports no progress, so its whole answer, head and complete
    // body, is due by the request deadline.
    let bounds =
        crate::stream::StreamBounds::for_request(received, &deps.inflight.waiting.limits());
    // From the first poll onward the engine may have accepted work. Dropping
    // this request or receiving an uncertain transport error cannot free it.
    let guard = guard.abandon();
    // SPEC §10 (owner decision 2026-10-18): a completed answer is handed back
    // before its lease close is committed; the dispatch task then closes the
    // lease and only after that releases the in-flight slot, so the
    // deployment's bound counts the request until the ledger no longer does.
    let (mut reply, answer) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut attempt = first;
        let refused = loop {
            // SPEC §10 (amended 2026-10-21): a client gone before this attempt
            // was sent leaves nothing to forward, and nothing reached the engine.
            if reply.is_closed() {
                attempt.settle(LeaseEnd::NotAccepted).await;
                guard.release();
                return;
            }
            let generation = attempt.generation;
            timing.forwarding(attempt.instance, generation);
            let progress = crate::stream::Progress::default();
            // ADR 0032 §1: on a capable lane the observer names CapyCTL's
            // request ID, which the forwarder sends to the engine.
            let mut observer =
                crate::stream::ProgressOnly::new(progress.clone()).carrying(attempt.request_id());
            let discard = observer.discard_switch();
            // ADR 0028 §11: a request to a group's head is watched for its
            // first token; the watch holds nothing of the response.
            let stall = attempt.stall.take();
            let bounded = match endpoint {
                Endpoint::ChatCompletions => {
                    let after_cut = attempt.after_cut();
                    let forwarder = attempt.forward.clone();
                    let mut forward =
                        std::pin::pin!(forwarder.forward_chat_observed(&body, &mut observer));
                    let bounded = unless_hung_up(
                        crate::stream::bounded(forward.as_mut(), &progress, &bounds, stall),
                        &mut reply,
                    )
                    .await;
                    match (bounded, after_cut) {
                        // ADR 0032 §3: a cut on a capable lane (a hang-up,
                        // the deadline, the idle bound) settles and answers
                        // exactly as below, then keeps reading the same
                        // response, discarding it, after asking the engine
                        // to abort the request.
                        (Err(()), Some(after_cut)) => {
                            discard.store(true, std::sync::atomic::Ordering::SeqCst);
                            let answered = progress.produced();
                            let cut_by = if reply.is_closed() {
                                crate::stream::CutBy::HangUp
                            } else {
                                crate::stream::CutBy::bound(answered)
                            };
                            if attempt.settle(LeaseEnd::Cancelling { answered }).await {
                                guard.release();
                            }
                            let _ = reply.send(Err(Refused::Uncertain));
                            after_cut
                                .read(cut_by, forward, bounds.idle, |ended| ended.is_ok())
                                .await;
                            return;
                        }
                        (bounded, _) => bounded,
                    }
                }
                Endpoint::Decisions => {
                    unless_hung_up(
                        crate::stream::bounded(
                            attempt.forward.forward_decision(&body),
                            &progress,
                            &bounds,
                            stall,
                        ),
                        &mut reply,
                    )
                    .await
                }
            };
            // SPEC §10 (found live 2026-10-02): a cut drops the forward, which
            // closes the engine connection, so the lease is cancelling, as for
            // a stream whose client hung up. Amended 2026-10-21: so does a
            // client that disconnects (owner decision for 0.1.7), and it
            // settles on the engine's quiescence only if the engine produced
            // output for it first. A decision reports no progress, and its
            // answer comes whole, so a cut decision was never answered.
            let cut = bounded.is_err();
            let answered = progress.produced();
            let result = bounded.unwrap_or_else(|()| {
                Err(AdapterError::Uncertain(
                    "the client went away, or the backend missed the request's deadline or \
                     idle bound"
                        .into(),
                ))
            });
            let error = match result {
                Ok(response) => {
                    timing.response();
                    // SPEC §17: the first generated text, for time to first
                    // token and decode speed; a decision generates none (ADR
                    // 0030 §2).
                    if let Some(at) = observer.generated {
                        timing.generated_at(at);
                    }
                    timing.finish();
                    let _ = reply.send(Ok((response, timing)));
                    // SPEC §10: closed on completion evidence. A crash before
                    // this commit leaves the lease charged for the quiescence
                    // cleanup.
                    attempt.settle(LeaseEnd::Completed).await;
                    guard.release();
                    return;
                }
                Err(error) => error,
            };
            let end = if cut {
                LeaseEnd::Cancelling { answered }
            } else {
                lease_end(Some(&error))
            };
            let durable = attempt.settle(end).await;
            match error {
                // SPEC §10, T38: nothing reached the engine, so the request may
                // be offered to the next instance; never after acceptance.
                AdapterError::NotAccepted(reason) => {
                    plan.refused(generation, &reason);
                    // Amended 2026-10-21: no other instance is offered a
                    // request whose client is gone.
                    if reply.is_closed() {
                        guard.release();
                        return;
                    }
                    let started = std::time::Instant::now();
                    let next = plan.next().await;
                    timing.leased(started.elapsed());
                    match next {
                        Ok(next) => attempt = next,
                        Err(refusal) => {
                            guard.release();
                            break Refused::Before(refusal);
                        }
                    }
                }
                // SPEC §10, T19 (found live 2026-09-24): the engine answered the
                // request as invalid. That answer is complete, so the lease closed
                // as completed; another instance would reject it the same way.
                AdapterError::Rejected { status, message } => {
                    guard.release();
                    break Refused::Before(engine_rejection(status, &message));
                }
                // SPEC §10 (amended 2026-10-17, found live): the engine answered
                // with an error. The request reached an engine, so it is not
                // offered to another instance. Its lease is cancelling: the
                // ledger keeps the charge until the engine reports quiescence,
                // so the in-memory slot is released as for an uncertain end.
                // ADR 0031 §6: the same on an engine that reports no idle
                // signal, where only its verified exit settles the charge.
                AdapterError::EngineError { message, .. } => {
                    if durable {
                        guard.release();
                    }
                    break Refused::Engine(engine_failure(&message));
                }
                // SPEC §10, T19: refused deterministically before sending;
                // another instance would refuse it the same way.
                error if refused_before_sending(&error) => {
                    guard.release();
                    break Refused::Before(adapter_refusal(&error));
                }
                // Anything else may have been accepted. SPEC §10, T17: the
                // durable lease keeps the conservative charge (uncertain until
                // reconciled), so the in-memory slot — a per-process admission
                // count — is released; left taken, every uncertain end would
                // shrink the deployment's bound for the life of the process.
                // Only an authority with no durable ledger keeps it taken.
                _ => {
                    if durable {
                        guard.release();
                    }
                    break Refused::Uncertain;
                }
            }
        };
        let _ = reply.send(Err(refused));
    });
    match answer.await {
        Ok(Ok(response)) => Ok(response),
        // T17 T38: every offer was refused before forwarding (a host shutting
        // down, a refused connection, a gate that closed). Nothing reached an
        // engine, so this is a retryable 503 with each lease already closed.
        // An engine's error answer is relayed as it is.
        Ok(Err(Refused::Before(answer) | Refused::Engine(answer))) => Err(answer),
        // The task ended without answering (it panicked): the engine may have
        // accepted the request.
        Ok(Err(Refused::Uncertain)) | Err(_) => {
            Err(err("engine_error", "backend completion unverified"))
        }
    }
}

/// SPEC §10 (amended 2026-10-21, owner decision for 0.1.7): run a non-streamed
/// request's bounded forward until it ends or its client goes away. A client
/// that disconnects drops its handler, and with it the answer's receiver;
/// the forward is then dropped, which closes the engine connection, and the
/// request is cut (`Err(())`), as one cut at its deadline. An end ready at the
/// same moment wins, so a completion observed before the cut still closes
/// the lease completed.
async fn unless_hung_up<T, R>(
    forward: impl Future<Output = Result<T, ()>>,
    reply: &mut tokio::sync::oneshot::Sender<R>,
) -> Result<T, ()> {
    tokio::select! {
        biased;
        ended = forward => ended,
        () = reply.closed() => Err(()),
    }
}

/// How a dispatch that produced no response ended.
enum Refused {
    /// Refused before anything reached an engine, with the answer to give.
    Before((StatusCode, Json<serde_json::Value>)),
    /// The engine may have accepted it.
    Uncertain,
    /// SPEC §10 (amended 2026-10-17): the engine answered with an error, with
    /// the answer to give; its lease is cancelling.
    Engine((StatusCode, Json<serde_json::Value>)),
}

/// SPEC §10: the client answer for an engine's invalid-request rejection: the
/// engine's status (400, 413 or 422) and its bounded message, never a 500.
pub(crate) fn engine_rejection(
    status: u16,
    message: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    let (_, body) = err("engine_rejected", message);
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
        body,
    )
}

/// SPEC §10 (amended 2026-10-17, found live): the client answer for an
/// engine's error answer: 502 `engine_error` with the engine's bounded
/// message, not retryable, since the request reached an engine.
pub(crate) fn engine_failure(message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({ "code": "engine_error", "message": message, "retryable": false })),
    )
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
        // SPEC §10: the engine's complete invalid-request answer ends the work.
        Some(AdapterError::Rejected { .. }) => LeaseEnd::Completed,
        // SPEC §10 (amended 2026-10-17, found live): an engine's error answer
        // does not prove it stopped; charged until it reports quiescence.
        // Amended 2026-10-21: it is the engine's complete answer to this
        // request, so the engine had the request and its quiescence settles it.
        Some(AdapterError::EngineError { .. }) => LeaseEnd::Cancelling { answered: true },
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
            | AdapterError::CacheSaltUnsupported
    )
}

/// The client answer for a refusal decided before sending.
pub(crate) fn adapter_refusal(error: &AdapterError) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        AdapterError::PolicyDenied => err(
            "invalid_request",
            "the request carries a field that is not accepted",
        ),
        // SPEC §10: the request asks for a prefix-cache partition this
        // deployment's engine would ignore; isolation is never silently absent.
        AdapterError::CacheSaltUnsupported => err("cache_salt_unsupported", &format!("{error}")),
        _ => err("unsupported", &format!("{error}")),
    }
}

/// SPEC §10, T19 T21: refuse a request whose fields cannot be forwarded, before
/// any admission accounting, activation or engine is touched.
pub fn validate_request(
    body: &serde_json::Value,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    use capyctl_adapters::forward::ChatRequestRefusal as R;
    capyctl_adapters::forward::validate_chat_request(body).map_err(|refusal| match refusal {
        R::Malformed(_) | R::Field(_) => err("invalid_request", &refusal.to_string()),
        R::Unsupported(_) => err("unsupported_parameter", &refusal.to_string()),
    })
}

/// The answer to a refused durable lease grant for one dispatch (SPEC §10).
/// A full bound is answered so only once [`restart_when_full`] found no
/// restart to wait for.
pub(crate) fn lease_refusal(
    deployment: &str,
    refused: LeaseRefused,
) -> (StatusCode, Json<serde_json::Value>) {
    match refused {
        // SPEC §§6.1, 13.2: the gate closed (a host draining or re-proving
        // readiness, a stop in progress). Retryable, never a 500.
        LeaseRefused::Closed => err(
            "unavailable",
            &format!("dispatch to deployment {deployment} is closed; retry shortly"),
        ),
        LeaseRefused::Full => queue_full(),
        LeaseRefused::Unavailable(reason) => err("unavailable", &reason),
    }
}

/// SPEC §10 (T19): the deployment's durable bound of outstanding requests is
/// reached.
fn queue_full() -> (StatusCode, Json<serde_json::Value>) {
    err("queue_full", "outstanding request bound reached")
}

/// ADR 0032 §7: the deployment's durable lease bound refused a grant as full.
/// When every open lease of `deployment` is a cancelled request that settles
/// only at its instance's next stop, the authority restarts each instance
/// holding them; the request then waits for the restart as for an
/// activation (joining the deployment's one activation, SPEC §10 step 2),
/// until `deadline`, and `Ok` lets the caller ask for its grant again.
/// Otherwise the refusal stands: `queue_full`, as before.
pub(crate) async fn restart_when_full(
    deps: &RouterDeps,
    deployment: &str,
    deadline: tokio::time::Instant,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if tokio::time::Instant::now() >= deadline {
        return Err(queue_full());
    }
    match deps.controller.restart_for_held_requests(deployment).await {
        Ok(capyctl_controller::HeldRestart::Restarting) => {}
        Ok(capyctl_controller::HeldRestart::NotHeld) => return Err(queue_full()),
        Err(fault) => {
            capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Warning,
                &format!(
                    "deployment {deployment}: could not ask for a restart of instances holding \
                     requests until their stop: {fault}"
                ),
            );
            return Err(queue_full());
        }
    }
    match tokio::time::timeout_at(deadline, join_activation(deps, deployment)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(refusal)) => Err(refusal),
        Err(_) => Err(err(
            "unavailable",
            &format!(
                "deployment {deployment} was restarting to settle requests held until its stop \
                 and did not become servable within the queue deadline; retry shortly"
            ),
        )),
    }
}

/// Close or retain a lease on the evidence. A failed write leaves it charged,
/// which is the conservative outcome, so it is reported and not retried here,
/// with the request's IDs (SPEC §10, owner decision 2026-10-19).
pub(crate) async fn close_lease(
    controller: &dyn capyctl_controller::LifecyclePort,
    lease: Option<RequestLease>,
    end: LeaseEnd,
    ids: Option<&crate::request_id::RequestIds>,
) {
    if let Some(lease) = lease {
        let id = lease.id().to_owned();
        if let Err(error) = controller.close_request_lease(lease, end).await {
            let request = ids.map(|ids| ids.log_suffix()).unwrap_or_default();
            capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Warning,
                &format!("request lease {id} stays charged: {error}{request}"),
            );
        }
    }
}

fn err(code: &str, message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        match code {
            "unknown_model" => StatusCode::NOT_FOUND,
            // SPEC §14: a request that can never be forwarded is the client's.
            // ADR 0030 §2: the endpoint does not answer the deployment's API.
            "invalid_request"
            | "unsupported_parameter"
            | "cache_salt_unsupported"
            | "api_mismatch" => StatusCode::BAD_REQUEST,
            // SPEC §10 (T19): a full queue is transient; 413 is for body size.
            "queue_full" => StatusCode::TOO_MANY_REQUESTS,
            "unsupported" => StatusCode::NOT_IMPLEMENTED,
            "insufficient_resources" => StatusCode::TOO_MANY_REQUESTS,
            "conflict" => StatusCode::CONFLICT,
            // SPEC §10 (owner decision 2026-09-25): an operator's stop is not
            // a capacity refusal; waiting does not help, starting it does.
            "deployment_stopped" => StatusCode::CONFLICT,
            // SPEC §6.5, §10: an explicit activation that is not serving
            // starts only on an operator's action; waiting does not help.
            "deployment_inactive" => StatusCode::CONFLICT,
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
pub(crate) fn map_controller(
    e: capyctl_controller::LifecycleFault,
) -> (StatusCode, Json<serde_json::Value>) {
    use capyctl_controller::LifecycleFault as F;
    match e {
        F::NotFound(d) => err("unknown_model", &format!("deployment {d} vanished")),
        F::Blocked(m) => err("insufficient_resources", &format!("admission blocked: {m}")),
        F::Stopped(m) => err("deployment_stopped", &m),
        F::Inactive(m) => err("deployment_inactive", &m),
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
impl capyctl_adapters::traits::ChatForward for NoForward {
    async fn forward_chat(
        &self,
        _body: &capyctl_adapters::request::InferenceRequest,
    ) -> Result<serde_json::Value, capyctl_adapters::traits::AdapterError> {
        Err(capyctl_adapters::traits::AdapterError::UnsupportedCapability)
    }
}

#[cfg(test)]
mod tests {
    use super::unless_hung_up;

    // T17 (SPEC §10, amended 2026-10-21): a forward whose end is ready when
    // its client has already gone still ends with that end, so a completion
    // observed before the cut closes the lease completed.
    #[tokio::test]
    async fn a_completion_ready_with_the_hang_up_wins() {
        let (mut reply, answer) = tokio::sync::oneshot::channel::<()>();
        drop(answer);
        let ended = unless_hung_up(std::future::ready(Ok::<_, ()>("done")), &mut reply).await;
        assert_eq!(ended, Ok("done"));
    }

    // T17 (SPEC §10, amended 2026-10-21): a client that goes away while the
    // forward is still waiting cuts it, and the forward is dropped (which is
    // what closes the engine connection).
    #[tokio::test]
    async fn a_hang_up_cuts_and_drops_a_pending_forward() {
        struct Dropped(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let held = Dropped(dropped.clone());
        let forward = async move {
            let _held = held;
            std::future::pending::<Result<(), ()>>().await
        };
        let (mut reply, answer) = tokio::sync::oneshot::channel::<()>();
        let racing = tokio::spawn(async move { unless_hung_up(forward, &mut reply).await });
        tokio::task::yield_now().await;
        assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));
        drop(answer);
        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), racing)
            .await
            .expect("cut on the hang-up")
            .unwrap();
        assert_eq!(ended, Err(()));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }
}

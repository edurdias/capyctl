//! ADR 0036 §§3-4, 9: `POST /v1/embeddings` for `api: embeddings`
//! deployments. Admitted, selected, leased and failed over exactly as a
//! decision (ADR 0030 §2), but the answer is streamed through: the forwarder
//! checks it as it passes and hands its bytes to [`ClientSink`], which relays
//! them to the client with backpressure; the router writes `capyctl.metrics`
//! and the closing brace only after the last check passed.
//!
//! The forward runs on its own task, so a client that hangs up, or a request
//! cut by its deadline, idle bound or a stalled delivery, never drops it (owner
//! decision 2026-10-27, question 7): the lease is closed `cancelling` at the
//! cut, the answer is read and checked to its end with nothing more relayed,
//! and a complete answer settles the lease on it. The engine's complete error
//! answer is answered and settles on the engine's idle reading, as today. Only
//! an answer that never completes keeps today's rule (SPEC §10: the
//! instance's next verified stop, and ADR 0032 §7's restart when such
//! requests fill the deployment).

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::Json;
use capyctl_adapters::traits::{AdapterError, DeliveryFailed, EmbeddingSink};
use capyctl_controller::LeaseEnd;

use crate::admission::StaticStreamGuard;
use crate::balance::{Attempt, Plan};
use crate::chat::{
    adapter_refusal, engine_failure, engine_rejection, lease_end, map_controller, refusal,
    refused_before_sending,
};
use crate::endpoint::Endpoint;
use crate::request_id::RequestIds;
use crate::stream::{bounded, CutBy, Progress, StreamBounds};
use crate::timing::RequestTiming;
use crate::RouterDeps;

type Refusal = (StatusCode, Json<serde_json::Value>);

/// SPEC §10: how long one delivery to the client may wait for room, as for a
/// relayed chat chunk. A delivery that misses it is a cut.
const DELIVERY_BOUND: Duration = Duration::from_secs(10);

/// How many relayed pieces may wait for the client; past that the forwarder
/// waits (backpressure), within [`DELIVERY_BOUND`].
const QUEUED_PIECES: usize = 16;

/// What the forward's task hands the handler.
enum Frame {
    /// Answer bytes, relayed as they arrived.
    Bytes(axum::body::Bytes),
    /// SPEC §10: the request was answered with an error before any answer
    /// byte was relayed.
    Refused(Refusal),
}

/// Wait until the cut switch is set: `true`, or `false` once its sender is
/// gone without a cut. Holds no borrow of the switch's value across a wait.
async fn cut_set(cut: &mut tokio::sync::watch::Receiver<bool>) -> bool {
    cut.wait_for(|cut| *cut).await.is_ok()
}

/// The answer for an end the router cannot prove, before any byte was
/// relayed (as for a decision).
fn unverified() -> Refusal {
    refusal("engine_error", "backend completion unverified")
}

/// ADR 0036 §4: the destination of one attempt's answer bytes.
struct ClientSink<'a> {
    /// `None` once a delivery failed or the request was cut.
    tx: Option<tokio::sync::mpsc::Sender<Frame>>,
    progress: Progress,
    /// Whether any answer byte reached the client's queue.
    relayed: Arc<AtomicBool>,
    /// Set at a cut, by the router or by a failed delivery here; from then on
    /// nothing more is relayed and the client's body ends.
    cut: Arc<tokio::sync::watch::Sender<bool>>,
    /// SPEC §17: the request's clock; engine bytes mark its first byte.
    timing: &'a mut RequestTiming,
}

#[async_trait::async_trait]
impl EmbeddingSink for ClientSink<'_> {
    async fn send(&mut self, bytes: Vec<u8>) -> Result<(), DeliveryFailed> {
        if *self.cut.borrow() {
            self.tx = None;
        }
        let Some(tx) = &self.tx else {
            return Err(DeliveryFailed);
        };
        let mut cut = self.cut.subscribe();
        let delivery = async {
            tokio::select! {
                biased;
                _ = cut_set(&mut cut) => false,
                sent = tx.send(Frame::Bytes(bytes.into())) => sent.is_ok(),
            }
        };
        if tokio::time::timeout(DELIVERY_BOUND, delivery).await == Ok(true) {
            self.relayed.store(true, Ordering::SeqCst);
            return Ok(());
        }
        // A client gone, or one that did not take the bytes within the bound:
        // the forwarder stops relaying and reads on (§4).
        self.tx = None;
        self.cut.send_replace(true);
        Err(DeliveryFailed)
    }

    fn progressed(&mut self) {
        self.progress.mark();
        self.timing.chunk(false);
    }
}

/// ADR 0036 §3 (owner decision 2026-10-27, question 4): llama.cpp ignores
/// `dimensions`, so a request asking for other than the instance's native
/// dimension is refused. The native dimension is the one the instance's
/// embedding probe answered at readiness, as its recorded wake canary
/// reference holds it. An instance with none recorded yet (its reference is
/// written just after readiness) is not refused: llama.cpp answers full
/// vectors and the answer's own checks hold them to `dimensions`. A
/// deployment dispatched whole (no instance view) has no reference to read.
fn native_dimensions(
    deps: &RouterDeps,
    deployment: &str,
    generation: Option<i64>,
    body: &serde_json::Value,
) -> Result<(), Refusal> {
    let Some(generation) = generation.filter(|_| body.get("dimensions").is_some()) else {
        return Ok(());
    };
    let engine = deps
        .controller
        .instance_endpoint(deployment, generation)
        .map_err(map_controller)?
        .and_then(|endpoint| endpoint.engine);
    let llamacpp = capyctl_config::engine_policy::Engine::Llamacpp.name();
    if engine.as_deref() != Some(llamacpp) {
        return Ok(());
    }
    let Some(native) = deps
        .controller
        .embedding_dimension(deployment, generation)
        .map_err(map_controller)?
    else {
        return Ok(());
    };
    capyctl_adapters::embedding_request::validate_native_dimensions(body, native)
        .map_err(|refused| refusal("invalid_request", &refused.to_string()))
}

/// ADR 0036 §§3-4: dispatch one embeddings request and answer it: HTTP 200
/// with the answer streamed through, without `Content-Length`, once its first
/// bytes arrive; or the error given before any byte (a refusal before
/// forwarding, `engine_rejected`, `engine_error`). `received` is when it
/// arrived, `body_bytes` its size, `ids` its request IDs (SPEC §10).
pub(crate) async fn dispatch(
    deps: &RouterDeps,
    model: &str,
    body: serde_json::Value,
    body_bytes: usize,
    received: tokio::time::Instant,
    mut timing: RequestTiming,
    ids: Arc<RequestIds>,
) -> Result<axum::response::Response, Refusal> {
    let (deployment, guard, view) = crate::chat::admit_timed(
        deps,
        Endpoint::Embeddings,
        model,
        body_bytes,
        received,
        &mut timing,
    )
    .await?;
    // ADR 0013 §10 (I3): choose an instance and open its lease. Nothing has
    // been sent yet: a refusal releases the guard on drop.
    let started = std::time::Instant::now();
    let deadline = received + deps.inflight.waiting.limits().deadline;
    let mut plan = Plan::new(deps, &deployment, view, ids.clone(), deadline)?;
    timing.selected(started.elapsed());
    let started = std::time::Instant::now();
    let first = plan.next().await?;
    timing.leased(started.elapsed());
    // SPEC §17: the phases known before the first byte, as for a stream.
    let header = timing.header_enabled().then(|| timing.header_value());
    // ADR 0030 §2: an embeddings answer has no first-token event; its first
    // bytes are due by the request deadline, later ones within the idle bound.
    let bounds = StreamBounds::for_request(received, &deps.inflight.waiting.limits());
    let (reply, mut frames) = tokio::sync::mpsc::channel(QUEUED_PIECES);
    let cut = Arc::new(tokio::sync::watch::Sender::new(false));
    let mut client_cut = cut.subscribe();
    tokio::spawn(
        Pump {
            deps: deps.clone(),
            deployment,
            plan,
            body,
            // From the first poll onward the engine may have accepted work.
            guard: Some(guard.abandon()),
            timing,
            bounds,
            reply,
            cut,
            ids,
        }
        .run(first),
    );
    // The handler waits for the answer's first bytes or the error given
    // before them. A client gone meanwhile drops this receiver, which the
    // forward's task observes as a hang-up.
    let head = match frames.recv().await {
        Some(Frame::Bytes(bytes)) => bytes,
        Some(Frame::Refused(refused)) => return Err(refused),
        None => return Err(unverified()),
    };
    let stream = async_stream::stream! {
        yield Ok::<_, std::convert::Infallible>(head);
        // ADR 0036 §4: at a cut nothing more reaches the client, and the body
        // ends without the closing brace. A task that ended drops the cut
        // switch, which is not a cut: what it queued is still delivered.
        let mut watching = true;
        loop {
            tokio::select! {
                biased;
                cut = cut_set(&mut client_cut), if watching => {
                    if cut {
                        break;
                    }
                    watching = false;
                }
                frame = frames.recv() => match frame {
                    Some(Frame::Bytes(bytes)) => yield Ok(bytes),
                    Some(Frame::Refused(_)) | None => break,
                },
            }
        }
    };
    let mut response = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from_stream(stream))
        .map_err(|_| unverified())?;
    if let Some(value) = header.and_then(|h| h.parse().ok()) {
        response
            .headers_mut()
            .insert(crate::timing::TIMING_HEADER, value);
    }
    Ok(response)
}

/// The forward's task: everything one request needs once it is dispatched.
struct Pump {
    deps: RouterDeps,
    deployment: String,
    plan: Plan,
    body: serde_json::Value,
    /// The in-flight slot, until released on evidence.
    guard: Option<StaticStreamGuard>,
    timing: RequestTiming,
    bounds: StreamBounds,
    reply: tokio::sync::mpsc::Sender<Frame>,
    cut: Arc<tokio::sync::watch::Sender<bool>>,
    ids: Arc<RequestIds>,
}

impl Pump {
    fn release(&mut self) {
        if let Some(guard) = self.guard.take() {
            guard.release();
        }
    }

    /// Hand the client the error given before any byte; after bytes, the
    /// body just ends, without the closing brace.
    async fn refuse(&self, relayed: &AtomicBool, refused: Refusal) {
        if !relayed.load(Ordering::SeqCst) {
            let _ = tokio::time::timeout(DELIVERY_BOUND, self.reply.send(Frame::Refused(refused)))
                .await;
        }
    }

    async fn run(mut self, first: Attempt) {
        let mut attempt = first;
        let relayed = Arc::new(AtomicBool::new(false));
        loop {
            // SPEC §10 (amended 2026-10-21): a client gone before this
            // attempt was sent leaves nothing to forward.
            if self.reply.is_closed() {
                attempt.settle(LeaseEnd::NotAccepted).await;
                self.release();
                return;
            }
            if let Err(refused) =
                native_dimensions(&self.deps, &self.deployment, attempt.generation, &self.body)
            {
                attempt.settle(LeaseEnd::NotAccepted).await;
                self.release();
                self.refuse(&relayed, refused).await;
                return;
            }
            let generation = attempt.generation;
            self.timing.forwarding(attempt.instance, generation);
            let progress = Progress::default();
            let stall = attempt.stall.take();
            let result = {
                let gone = self.reply.clone();
                let mut delivery_cut = self.cut.subscribe();
                let mut sink = ClientSink {
                    tx: Some(self.reply.clone()),
                    progress: progress.clone(),
                    relayed: relayed.clone(),
                    cut: self.cut.clone(),
                    timing: &mut self.timing,
                };
                let forwarder = attempt.forward.clone();
                let mut forward =
                    std::pin::pin!(forwarder.forward_embedding(&self.body, &mut sink));
                // `Err`: the router cut the forward, which has not ended. An
                // end ready at the same moment as a cut still wins.
                let ended = tokio::select! {
                    biased;
                    ended = bounded(forward.as_mut(), &progress, &self.bounds, stall) => {
                        ended.map_err(|()| CutBy::bound(progress.produced()))
                    }
                    () = gone.closed() => Err(CutBy::HangUp),
                    _ = cut_set(&mut delivery_cut) => Err(CutBy::Delivery),
                };
                drop(gone);
                match ended {
                    Ok(result) => result,
                    Err(cut_by) => {
                        // ADR 0036 §4: the lease is cancelling while the
                        // answer is read to its end; nothing more reaches
                        // the client.
                        self.cut.send_replace(true);
                        let held = attempt.cut_lease();
                        let answered = progress.produced();
                        let durable = attempt.settle(LeaseEnd::Cancelling { answered }).await;
                        // SPEC §10, T17: the durable ledger keeps the charge,
                        // so the in-memory slot is released as for any cut.
                        // Without one, the slot is the only record and waits
                        // for the answer.
                        let mut guard = self.guard.take();
                        if durable {
                            if let Some(guard) = guard.take() {
                                guard.release();
                            }
                        }
                        if !matches!(cut_by, CutBy::HangUp) && !relayed.load(Ordering::SeqCst) {
                            let _ = self.reply.try_send(Frame::Refused(unverified()));
                        }
                        // The client's body ends now: only the sink's sender
                        // is left, and it sends nothing after the cut.
                        let (reply, _) = tokio::sync::mpsc::channel(1);
                        drop(std::mem::replace(&mut self.reply, reply));
                        let outcome = read_to_end(forward, &progress, self.bounds.idle).await;
                        settle_cut(
                            held,
                            guard,
                            outcome,
                            cut_by,
                            &self.deployment,
                            generation,
                            &self.ids,
                        )
                        .await;
                        return;
                    }
                }
            };
            match result {
                Ok(answered) => {
                    let to_response = self.timing.finish_answer();
                    // ADR 0036 §4, §9: every check passed; the figures and the
                    // closing brace go out, then the lease closes on the
                    // completion (SPEC §10, owner decision 2026-10-18).
                    if !answered.client_gone {
                        let tail = crate::engine_metrics::embedding_tail(
                            &answered,
                            &self.timing,
                            to_response,
                        );
                        let _ = tokio::time::timeout(
                            DELIVERY_BOUND,
                            self.reply.send(Frame::Bytes(tail.into())),
                        )
                        .await;
                    }
                    let (reply, _) = tokio::sync::mpsc::channel(1);
                    drop(std::mem::replace(&mut self.reply, reply));
                    attempt.settle(LeaseEnd::Completed).await;
                    self.release();
                    return;
                }
                Err(error) => {
                    let durable = attempt.settle(lease_end(Some(&error))).await;
                    let refused = match error {
                        // SPEC §10, T38: nothing reached the engine, so the
                        // request may be offered to the next instance.
                        AdapterError::NotAccepted(reason) => {
                            self.plan.refused(generation, &reason);
                            if self.reply.is_closed() {
                                self.release();
                                return;
                            }
                            let started = std::time::Instant::now();
                            let next = self.plan.next().await;
                            self.timing.leased(started.elapsed());
                            match next {
                                Ok(next) => {
                                    attempt = next;
                                    continue;
                                }
                                Err(refused) => {
                                    self.release();
                                    refused
                                }
                            }
                        }
                        // SPEC §10: the engine's complete invalid-request
                        // answer (an input over the model's length, a
                        // `dimensions` it does not support); completed.
                        AdapterError::Rejected { status, message } => {
                            self.release();
                            engine_rejection(status, &message)
                        }
                        // SPEC §10 (amended 2026-10-17): the engine's complete
                        // error answer; its lease is cancelling.
                        AdapterError::EngineError { message, .. } => {
                            if durable {
                                self.release();
                            }
                            engine_failure(&message)
                        }
                        error if refused_before_sending(&error) => {
                            self.release();
                            adapter_refusal(&error)
                        }
                        // ADR 0036 §4: a malformed answer, a closed
                        // connection: uncertain, never replayed. After bytes
                        // the body ends without its closing brace.
                        _ => {
                            if durable {
                                self.release();
                            }
                            unverified()
                        }
                    };
                    self.refuse(&relayed, refused).await;
                    return;
                }
            }
        }
    }
}

/// How the read of a cut request ended.
enum ReadOn<T> {
    /// The forward ended with this.
    Ended(T),
    /// The engine went silent past the idle bound; the forward is dropped.
    Silent,
}

/// ADR 0036 §4: read a cut request's answer to its end, relaying nothing.
/// The engine is never held back by an unread connection; only silence past
/// `idle` (counted from the cut, re-armed by progress) gives up on it. Called
/// at the cut, once whether the engine answered was taken from `progress`.
async fn read_to_end<F: Future>(
    forward: std::pin::Pin<&mut F>,
    progress: &Progress,
    idle: Duration,
) -> ReadOn<F::Output> {
    // `bounded` counts silence from the last mark; an idle cut's last engine
    // byte is already `idle` old, and a hang-up's or delivery's may be.
    progress.mark();
    let bounds = StreamBounds {
        first_event_by: tokio::time::Instant::now() + idle,
        idle,
    };
    match bounded(forward, progress, &bounds, None).await {
        Ok(output) => ReadOn::Ended(output),
        Err(()) => ReadOn::Silent,
    }
}

/// A failed ledger write leaves the lease charged, the conservative outcome;
/// reported as a failed close is.
fn stays_charged(lease: &str, error: &capyctl_controller::LeaseRefused, ids: &RequestIds) {
    capyctl_domain::role_log::notice(
        capyctl_domain::role_log::Level::Warning,
        &format!(
            "request lease {lease} stays charged: {error}{}",
            ids.log_suffix()
        ),
    );
}

/// ADR 0036 §4: settle a cut request on how its read ended. A complete
/// answer (or the engine's complete invalid-request answer, SPEC §10's other
/// completion) settles its cancelling lease on it. The engine's complete
/// error answer is answered, as today: its cancellation is recorded as
/// answered and settles on the engine's idle reading, the lease charged
/// until then. Anything else leaves it to today's rule. Role-logged with the
/// request's IDs and the lease.
async fn settle_cut(
    held: Option<(Arc<dyn capyctl_controller::LifecyclePort>, String)>,
    guard: Option<StaticStreamGuard>,
    outcome: ReadOn<Result<capyctl_adapters::traits::EmbeddingAnswered, AdapterError>>,
    cut: CutBy,
    deployment: &str,
    generation: Option<i64>,
    ids: &RequestIds,
) {
    let completed = matches!(
        outcome,
        ReadOn::Ended(Ok(_) | Err(AdapterError::Rejected { .. }))
    );
    let settled = match (&outcome, &held) {
        (_, Some((authority, lease))) if completed => {
            match authority.settle_answered_cut(lease).await {
                Ok(true) => Ok("settled_on_answer"),
                // Already settled on the engine's idle reading, or uncertain.
                Ok(false) => Err("lease_not_cancelling"),
                Err(error) => {
                    stays_charged(lease, &error, ids);
                    Err("settlement_failed")
                }
            }
        }
        (_, None) if completed => Ok("settled_on_answer"),
        // SPEC §10 (amended 2026-10-17): the engine's complete error answer
        // does not show it stopped; its idle reading after the answer does.
        (ReadOn::Ended(Err(AdapterError::EngineError { .. })), Some((authority, lease))) => {
            match authority.answer_cut(lease).await {
                Ok(true) => Ok("settles_on_idle_reading"),
                Ok(false) => Err("lease_not_cancelling"),
                Err(error) => {
                    stays_charged(lease, &error, ids);
                    Err("answer_record_failed")
                }
            }
        }
        (ReadOn::Ended(_), _) => Err("answer_not_completion"),
        (ReadOn::Silent, _) => Err("engine_silent"),
    };
    // Without a durable ledger the slot is the only record: freed only on
    // the answer, else held for the life of the process (as today).
    if let (true, Some(guard)) = (completed, guard) {
        guard.release();
    }
    let (outcome, reason) = match settled {
        Ok(outcome) => (outcome, None),
        Err(reason) => ("held", Some(reason)),
    };
    let mut event = serde_json::json!({
        "event": "router_embedding_cut",
        "deployment": deployment,
        "generation": generation,
        "lease": held.as_ref().map(|(_, lease)| lease.clone()),
        "cut": cut.name(),
        "outcome": outcome,
        "reason": reason,
    });
    ids.log_fields(&mut event);
    capyctl_domain::role_log::event(event);
}

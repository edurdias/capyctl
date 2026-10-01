//! SSE streaming dispatch (F1 design §5, T17 groundwork): the router
//! forwards engine chunks as SSE `data:` frames, ends with `data: [DONE]`,
//! and releases the in-flight guard only on verified backend completion.
//! A client hang-up closes the engine connection; accounting stays charged until the engine reports quiescence (SPEC §10).

use std::convert::Infallible;
use std::sync::Arc;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::stream::Stream;

use crate::admission::StaticStreamGuard;
use capyctl_adapters::traits::{AdapterError, ChatSink, DeliveryFailed, StreamEnded};

const MAX_CHUNK_BYTES: usize = 64 * 1024;

/// SPEC §10: how long a relayed stream may go without backend progress. Found
/// live 2026-09-23 (matrix M30, vLLM): a fixed 300 s cap cut a stream that was
/// still producing and left its lease uncertain, so a park never armed. A
/// stream is now cut only when its first event misses the request's deadline
/// (activation included) or a later event misses the idle bound; a stream that
/// keeps producing runs as long as it produces.
#[derive(Clone, Copy, Debug)]
pub struct StreamBounds {
    /// When the first backend event must have arrived.
    pub first_event_by: tokio::time::Instant,
    /// The longest gap between backend events after the first.
    pub idle: std::time::Duration,
}

impl StreamBounds {
    /// Bounds for a request that arrived at `received`, from the router's
    /// queue limits (host policy `queue.request_deadline` and
    /// `queue.stream_idle_timeout`).
    pub fn for_request(received: tokio::time::Instant, limits: &crate::queue::WaitLimits) -> Self {
        Self {
            first_event_by: received + limits.deadline,
            idle: limits.stream_idle,
        }
    }
}

/// When the backend last produced an event, shared between the sink the
/// adapter writes to and the watchdog that bounds the stream.
#[derive(Clone)]
pub(crate) struct Progress(Arc<tokio::sync::watch::Sender<Option<tokio::time::Instant>>>);

impl Default for Progress {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::Sender::new(None)))
    }
}

impl Progress {
    fn mark(&self) {
        self.0.send_replace(Some(tokio::time::Instant::now()));
    }
}

/// Run `forward` until it ends, or until the backend stops progressing within
/// `bounds`; `Err(())` means the router cut it, which proves nothing about
/// what the engine accepted.
/// SPEC §10 (amended 2026-10-01): the lease closes on the backend's own
/// terminator or on proof the engine never saw the request. A stream whose
/// client hung up was cancelled upstream and stays charged as cancelling
/// until the engine reports quiescence. A stream the router cut for missing
/// its bounds stays uncertain.
pub fn stream_lease_end(
    result: &Result<Result<StreamEnded, AdapterError>, ()>,
) -> capyctl_controller::LeaseEnd {
    match result {
        Ok(Ok(StreamEnded::Completed)) => capyctl_controller::LeaseEnd::Completed,
        Ok(Ok(StreamEnded::Cancelled)) => capyctl_controller::LeaseEnd::Cancelling,
        Ok(Err(error)) => crate::chat::lease_end(Some(error)),
        _ => capyctl_controller::LeaseEnd::Uncertain,
    }
}

pub(crate) async fn bounded<F: std::future::Future>(
    forward: F,
    progress: &Progress,
    bounds: &StreamBounds,
) -> Result<F::Output, ()> {
    tokio::pin!(forward);
    let mut marks = progress.0.subscribe();
    loop {
        let due = match *marks.borrow_and_update() {
            None => bounds.first_event_by,
            Some(last) => last + bounds.idle,
        };
        tokio::select! {
            biased;
            output = &mut forward => return Ok(output),
            // New progress re-arms the bound.
            _ = marks.changed() => {}
            _ = tokio::time::sleep_until(due) => return Err(()),
        }
    }
}

/// SPEC §10: a sink that only records backend progress, for a collected
/// (non-streaming) response bounded like a stream.
pub(crate) struct ProgressOnly(pub(crate) Progress);

#[async_trait::async_trait]
impl ChatSink for ProgressOnly {
    fn progressed(&mut self) {
        self.0.mark();
    }

    async fn send(&mut self, _chunk: String) -> Result<(), DeliveryFailed> {
        self.0.mark();
        Ok(())
    }
}

struct ResponseSink {
    tx: tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
    failed: bool,
    /// SPEC §17 (M80): this request's clock; chunks mark it as they arrive.
    timing: crate::timing::RequestTiming,
    /// SPEC §10: backend progress, delivered or drained.
    progress: Progress,
}

#[async_trait::async_trait]
impl ChatSink for ResponseSink {
    fn progressed(&mut self) {
        self.progress.mark();
    }

    async fn send(&mut self, chunk: String) -> Result<(), DeliveryFailed> {
        self.progress.mark();
        if self.failed {
            return Err(DeliveryFailed);
        }
        // SPEC §17: the upstream chunk arrived now, whatever its delivery.
        let content = self.timing.phases().time_to_first_content.is_none()
            && crate::timing::carries_content(&chunk);
        self.timing.chunk(content);
        // Set failure before awaiting so cancellation by the adapter's delivery
        // deadline cannot later append a successful terminal to partial output.
        self.failed = true;
        if chunk.len() > MAX_CHUNK_BYTES {
            return Err(DeliveryFailed);
        }
        if !matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.tx.send(Ok(Event::default().data(chunk)))
            )
            .await,
            Ok(Ok(()))
        ) {
            return Err(DeliveryFailed);
        }
        self.failed = false;
        Ok(())
    }
}

pub fn stream_response(
    forward: Arc<dyn capyctl_adapters::traits::ChatForward>,
    body: serde_json::Value,
    guard: StaticStreamGuard,
    // SPEC §10: the durable lease this stream holds until the backend ends, with
    // the authority that closes it. `None` when the authority keeps no ledger.
    lease: Option<(
        Arc<dyn capyctl_controller::LifecyclePort>,
        capyctl_controller::RequestLease,
    )>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    stream_planned(
        crate::balance::Attempt::direct(forward, lease),
        None,
        body,
        guard,
    )
}

/// ADR 0013 §10 (I3): stream through `first`, failing over along `plan` only
/// while an offer is refused before anything reaches an engine. A refusal is
/// decided before the first chunk, so a client never receives output from two
/// engines, and an accepted stream is never replayed (SPEC §10, T38).
pub fn stream_planned(
    first: crate::balance::Attempt,
    plan: Option<crate::balance::Plan>,
    body: serde_json::Value,
    guard: StaticStreamGuard,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    stream_planned_timed(
        first,
        plan,
        body,
        guard,
        crate::timing::RequestTiming::untracked(),
        StreamBounds::for_request(tokio::time::Instant::now(), &Default::default()),
    )
}

/// As [`stream_planned`], timing the request (SPEC §17, M80). With the timing
/// header enabled, a completed stream carries its timings as one SSE comment
/// line before `data: [DONE]`. The stream is bounded by `bounds` (SPEC §10).
pub fn stream_planned_timed(
    first: crate::balance::Attempt,
    mut plan: Option<crate::balance::Plan>,
    body: serde_json::Value,
    guard: StaticStreamGuard,
    timing: crate::timing::RequestTiming,
    bounds: StreamBounds,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(16);
    let pump = tokio::spawn(async move {
        // Accounting was registered BEFORE stream_response (the caller
        // enforces the in-flight bound synchronously). SPEC §10 (amended
        // 2026-10-01): a client hang-up makes the forwarder close the engine
        // connection, but that is not proof the engine stopped, so the charge
        // stays until the engine reports quiescence (a cancelling lease) or,
        // without a durable ledger, for as long as the slot is held.
        let guard = guard.abandon();
        let progress = Progress::default();
        let mut sink = ResponseSink {
            tx,
            failed: false,
            timing,
            progress: progress.clone(),
        };
        let mut attempt = first;
        let mut durable;
        let (result, refusal) = loop {
            let generation = attempt.generation;
            sink.timing.forwarding(attempt.instance, generation);
            let result = bounded(
                attempt.forward.forward_chat_stream_async(&body, &mut sink),
                &progress,
                &bounds,
            )
            .await;
            let end = stream_lease_end(&result);
            durable = attempt.settle(end).await;
            // SPEC §10, T19: a deterministic refusal decided before sending is
            // answered in-band; another instance would refuse it the same way.
            if let Ok(Err(AdapterError::Rejected { message, .. })) = &result {
                // SPEC §10 (found live 2026-09-24): the engine's complete
                // invalid-request answer, relayed in-band; not retryable.
                let message = message.clone();
                break (result, Some(("engine_rejected".to_owned(), message)));
            }
            if let Ok(Err(error)) = &result {
                if crate::chat::refused_before_sending(error) {
                    let (_, Json(answer)) = crate::chat::adapter_refusal(error);
                    let code = answer["code"].as_str().unwrap_or("unsupported").to_owned();
                    let message = answer["message"].as_str().unwrap_or_default().to_owned();
                    break (result, Some((code, message)));
                }
            }
            let Ok(Err(AdapterError::NotAccepted(reason))) = &result else {
                break (result, None);
            };
            let Some(plan) = plan.as_mut() else {
                let code = if reason.contains("shutting down") {
                    "shutting_down"
                } else {
                    "unavailable"
                };
                let reason = reason.clone();
                break (result, Some((code.to_owned(), reason)));
            };
            plan.refused(generation, reason);
            let started = std::time::Instant::now();
            let next = plan.next().await;
            sink.timing.leased(started.elapsed());
            match next {
                Ok(next) => attempt = next,
                Err((_, Json(answer))) => {
                    let code = answer["code"].as_str().unwrap_or("unavailable").to_owned();
                    let message = answer["message"].as_str().unwrap_or_default().to_owned();
                    break (result, Some((code, message)));
                }
            }
        };
        if let Some((code, reason)) = refusal {
            // Nothing reached any engine; say so in-band, retryably.
            guard.release();
            let retryable = !matches!(
                code.as_str(),
                "invalid_request" | "unsupported" | "engine_rejected"
            );
            let refusal = serde_json::json!({"error": {
                "code": code, "message": reason, "retryable": retryable}});
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                sink.tx.send(Ok(Event::default().data(refusal.to_string()))),
            )
            .await;
            return;
        }
        if matches!(result, Ok(Ok(StreamEnded::Completed))) {
            // Backend completion and downstream delivery are separate facts.
            guard.release();
            sink.timing.finish();
            if !sink.failed && sink.timing.header_enabled() {
                // SPEC §17 (M80): an SSE comment, ignored by SSE clients.
                let line = format!(
                    "{} {}",
                    crate::timing::TIMING_HEADER,
                    sink.timing.header_value()
                );
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    sink.tx.send(Ok(Event::default().comment(line))),
                )
                .await;
            }
            if !sink.failed {
                // A full final queue does not silently lose the terminal. Bound
                // waiting for a stalled consumer after backend work has settled.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    sink.tx.send(Ok(Event::default().data("[DONE]"))),
                )
                .await;
            }
        }
        // SPEC §10, T17: an uncertain end — an error, a premature close, a cut
        // for missing the bounds, a hang-up — cannot establish backend quiescence. With a
        // durable lease that uncertainty stays charged in the ledger until
        // reconciled, so the per-process slot is released; left taken, every
        // uncertain stream would shrink the deployment's bound for the life of
        // the process. Without a durable ledger the slot is the only record
        // and stays taken. A panic retains it either way.
        else if durable {
            guard.release();
        }
        drop(sink);
    });
    let stream = async_stream::stream! {
        while let Some(ev) = rx.recv().await {
            yield ev;
        }
        let _ = pump.is_finished();
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

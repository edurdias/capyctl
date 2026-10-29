//! SSE streaming dispatch (F1 design §5, T17 groundwork): the router
//! forwards engine chunks as SSE `data:` frames, ends with `data: [DONE]`,
//! and releases the in-flight guard only on verified backend completion.
//! A client hang-up closes the engine connection; accounting stays charged until the engine reports quiescence (SPEC §10).
//! ADR 0032: on an SGLang lane with the per-request abort, a cut instead asks
//! the engine to abort the request and keeps reading its response, which
//! settles the lease when it ends with the request's own final output.

use std::convert::Infallible;
use std::future::Future;
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
/// keeps producing runs as long as it produces. A chunk that only opens the
/// reply is not an event here (`capyctl_adapters::forward::opens_reply_only`,
/// found live 2026-10-02): the deadline bounds a prefill that follows it.
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

    /// SPEC §10 (amended 2026-10-21): whether the backend produced output for
    /// this request: an event parsed from the engine that is backend progress
    /// (a content, reasoning, tool-call or finish chunk; not a chunk that only
    /// opens the reply, and not response headers). A request cancelled after
    /// it reached the engine's scheduler, so the engine's idle reading after
    /// the cancellation may settle it; one cancelled before may still be in
    /// transit inside the engine, and settles only at its verified exit.
    pub(crate) fn produced(&self) -> bool {
        self.0.borrow().is_some()
    }
}

/// Run `forward` until it ends, or until the backend stops progressing within
/// `bounds`; `Err(())` means the router cut it, which proves nothing about
/// what the engine accepted.
/// SPEC §10 (amended 2026-10-01): the lease closes on the backend's own
/// terminator or on proof the engine never saw the request. A stream whose
/// client hung up was cancelled upstream and stays charged as cancelling
/// until the engine reports quiescence. A stream the router cut for missing
/// its bounds was cancelled the same way: dropping the forward closed the
/// engine connection (found live 2026-10-02), so it is cancelling too.
/// Amended 2026-10-21: `answered` is `Progress::produced`; only a request
/// the engine answered before its cancellation settles on quiescence.
pub fn stream_lease_end(
    result: &Result<Result<StreamEnded, AdapterError>, ()>,
    answered: bool,
) -> capyctl_controller::LeaseEnd {
    match result {
        Ok(Ok(StreamEnded::Completed)) => capyctl_controller::LeaseEnd::Completed,
        Ok(Ok(StreamEnded::Cancelled)) | Err(()) => {
            capyctl_controller::LeaseEnd::Cancelling { answered }
        }
        Ok(Err(error)) => crate::chat::lease_end(Some(error)),
        Ok(Ok(StreamEnded::BackendClosed)) => capyctl_controller::LeaseEnd::Uncertain,
    }
}

/// ADR 0028 §11 (decided 2026-10-06): the first-token watch of one request
/// forwarded to a group's head. It starts with the forward and ends at the
/// backend's first event (the first generated token) or when the forward
/// ends (completed, cut, or its client gone); if neither comes within
/// `after`, the stall is reported once to the lifecycle authority, off the
/// request's own path. A request to a single-host instance has none.
pub(crate) struct StallWatch {
    pub(crate) authority: Arc<dyn capyctl_controller::LifecyclePort>,
    pub(crate) deployment: String,
    pub(crate) instance: u32,
    pub(crate) generation: i64,
    pub(crate) after: std::time::Duration,
}

impl StallWatch {
    /// Report the stall on its own task: the probe it starts may take up to
    /// its own bound, and the request is neither held nor answered by it.
    fn report(self) {
        tokio::spawn(async move {
            self.authority
                .report_group_stall(&self.deployment, self.instance, self.generation)
                .await;
        });
    }
}

pub(crate) async fn bounded<F: Future>(
    forward: F,
    progress: &Progress,
    bounds: &StreamBounds,
    stall: Option<StallWatch>,
) -> Result<F::Output, ()> {
    tokio::pin!(forward);
    let mut marks = progress.0.subscribe();
    // ADR 0028 §11: the watch observes the same progress the bounds do; it
    // adds a timer, never a buffer or a wait, to the forward.
    let mut stall = stall.map(|watch| (tokio::time::Instant::now() + watch.after, watch));
    loop {
        let last = *marks.borrow_and_update();
        let due = match last {
            None => bounds.first_event_by,
            Some(last) => last + bounds.idle,
        };
        if last.is_some() {
            // The first token arrived: the watch ends.
            stall = None;
        }
        let stall_at = stall.as_ref().map(|(at, _)| *at);
        tokio::select! {
            biased;
            output = &mut forward => return Ok(output),
            // New progress re-arms the bound.
            _ = marks.changed() => {}
            _ = tokio::time::sleep_until(due) => return Err(()),
            _ = tokio::time::sleep_until(stall_at.unwrap_or(due)), if stall_at.is_some() => {
                if let Some((_, watch)) = stall.take() {
                    watch.report();
                }
            }
        }
    }
}

/// SPEC §10: a sink that only records backend progress, for a collected
/// (non-streaming) response bounded like a stream. SPEC §17: it also notes
/// when the first generated text arrived, for the per-request metrics.
pub(crate) struct ProgressOnly {
    progress: Progress,
    pub(crate) generated: Option<std::time::Instant>,
    /// ADR 0032 §1: CapyCTL's request ID, on a capable lane only.
    request_id: Option<String>,
    /// ADR 0032 §3: set at a cut on a capable lane; from then on the response
    /// read after the abort is discarded and records nothing.
    discard: Arc<std::sync::atomic::AtomicBool>,
}

impl ProgressOnly {
    pub(crate) fn new(progress: Progress) -> Self {
        Self {
            progress,
            generated: None,
            request_id: None,
            discard: Arc::default(),
        }
    }

    /// ADR 0032 §1: name `request_id` to the forwarder (a capable lane's).
    pub(crate) fn carrying(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }

    /// ADR 0032 §3: the switch that puts this sink in discard mode, held by
    /// the router while the forward borrows the sink.
    pub(crate) fn discard_switch(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.discard.clone()
    }

    fn discarding(&self) -> bool {
        self.discard.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ChatSink for ProgressOnly {
    fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    fn progressed(&mut self) {
        if !self.discarding() {
            self.progress.mark();
        }
    }

    fn collected(&mut self, chunk: &str) {
        if !self.discarding() && self.generated.is_none() && crate::timing::carries_content(chunk) {
            self.generated = Some(std::time::Instant::now());
        }
    }

    async fn send(&mut self, chunk: String) -> Result<(), DeliveryFailed> {
        if !self.discarding() && !capyctl_adapters::forward::opens_reply_only(&chunk) {
            self.progress.mark();
        }
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
    /// SPEC §17 (owner decision 2026-10-09): what the engine reported about
    /// this request, read from the chunks that carry it.
    report: crate::engine_metrics::EngineReport,
    /// ADR 0032 §1: CapyCTL's request ID while the attempt's lane is capable,
    /// `None` elsewhere.
    request_id: Option<String>,
    /// ADR 0032 §3: set when a stream on a capable lane is cut, by the router
    /// or by this sink's own failed delivery. From then on nothing more
    /// reaches the client, and what the forward reads is discarded.
    cut: Arc<tokio::sync::watch::Sender<bool>>,
}

impl ResponseSink {
    fn discarding(&self) -> bool {
        self.request_id.is_some() && *self.cut.borrow()
    }

    /// Deliver one event within the 10 s bound. On a capable lane the
    /// delivery also ends at a cut, so a send waiting on a slow client never
    /// outlives the cut.
    async fn deliver(&self, event: Event) -> bool {
        let bound = std::time::Duration::from_secs(10);
        if self.request_id.is_none() {
            return matches!(
                tokio::time::timeout(bound, self.tx.send(Ok(event))).await,
                Ok(Ok(()))
            );
        }
        let mut cut = self.cut.subscribe();
        let delivery = async {
            tokio::select! {
                biased;
                _ = cut.wait_for(|cut| *cut) => false,
                sent = self.tx.send(Ok(event)) => sent.is_ok(),
            }
        };
        tokio::time::timeout(bound, delivery).await == Ok(true)
    }
}

#[async_trait::async_trait]
impl ChatSink for ResponseSink {
    fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    fn progressed(&mut self) {
        if !self.discarding() {
            self.progress.mark();
        }
    }

    async fn send(&mut self, chunk: String) -> Result<(), DeliveryFailed> {
        // ADR 0032 §3: after a cut on a capable lane the forward keeps
        // reading, and what it reads is discarded.
        if self.discarding() {
            return Ok(());
        }
        // SPEC §10 (found live 2026-10-02): a chunk that only opens the reply is
        // relayed, but until the first output the request deadline bounds the
        // stream, not the idle bound.
        if !capyctl_adapters::forward::opens_reply_only(&chunk) {
            self.progress.mark();
        }
        if self.failed {
            return Err(DeliveryFailed);
        }
        // SPEC §17: the upstream chunk arrived now, whatever its delivery.
        let content = self.timing.phases().time_to_first_content.is_none()
            && crate::timing::carries_content(&chunk);
        self.timing.chunk(content);
        if crate::engine_metrics::EngineReport::may_carry(&chunk) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&chunk) {
                self.report.observe(&value);
            }
        }
        // Set failure before awaiting so cancellation by the adapter's delivery
        // deadline cannot later append a successful terminal to partial output.
        self.failed = true;
        let delivered =
            chunk.len() <= MAX_CHUNK_BYTES && self.deliver(Event::default().data(chunk)).await;
        if !delivered {
            // ADR 0032 §3: on a capable lane a failed, slow or oversized
            // delivery is a cut the router acts on (it aborts the request
            // and reads it to its end); the forward is not ended here.
            if self.request_id.is_some() {
                self.cut.send_replace(true);
                return Ok(());
            }
            return Err(DeliveryFailed);
        }
        self.failed = false;
        Ok(())
    }
}

/// ADR 0032 §3–§4: reading a request cut on a capable lane to its own end.
/// The lease is already cancelling and the client already answered; nothing
/// read here reaches it.
pub(crate) struct AfterCut {
    pub(crate) authority: Arc<dyn capyctl_controller::LifecyclePort>,
    pub(crate) deployment: String,
    pub(crate) generation: i64,
    /// CapyCTL's request ID, the engine's `rid`.
    pub(crate) request_id: String,
    /// The cut attempt's own lease.
    pub(crate) lease_id: String,
    pub(crate) ids: Arc<crate::request_id::RequestIds>,
}

/// ADR 0032 §3: what cut a request on a capable lane, for its role-log line.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CutBy {
    /// The client went away (its stream body or its answer's receiver).
    HangUp,
    /// A failed, slow or oversized delivery to the client.
    Delivery,
    /// No output by the request's deadline.
    Deadline,
    /// No output within the stream idle bound after the last.
    Idle,
}

impl CutBy {
    /// A bound cut: before any output it is the deadline's, after it the
    /// idle bound's (`bounded` arms them so).
    pub(crate) fn bound(answered: bool) -> Self {
        if answered {
            Self::Idle
        } else {
            Self::Deadline
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::HangUp => "hang_up",
            Self::Delivery => "delivery",
            Self::Deadline => "deadline",
            Self::Idle => "idle",
        }
    }
}

impl AfterCut {
    /// Ask the engine to abort the request once, keep polling the same
    /// `forward` meanwhile, and bound the rest of the read by `idle` (the
    /// stream idle bound) counted from the abort, never re-armed by progress.
    /// If the forward ends with the request's final output read whole
    /// (`final_output`) within that bound, the lease is acknowledged by its
    /// own evidence. Anything else (the abort not sent, the bound passed, an
    /// error or a truncated end) returns at once, and the caller drops the
    /// forward, which closes the engine connection; the lease then settles
    /// by today's rule (SPEC §10). Both steps are role-logged with the
    /// request's IDs and the lease (`router_request_abort`,
    /// `router_abort_settled`).
    pub(crate) async fn read<F: Future>(
        self,
        cut: CutBy,
        mut forward: std::pin::Pin<&mut F>,
        idle: std::time::Duration,
        final_output: impl Fn(&F::Output) -> bool,
    ) {
        let asked = tokio::time::Instant::now();
        let abort = tokio::time::timeout(
            idle,
            self.authority
                .abort_request(&self.deployment, self.generation, &self.request_id),
        );
        tokio::pin!(abort);
        // The response is read while the abort is asked, so the engine is
        // never held back by an unread connection.
        let mut ended = None;
        let sent = loop {
            tokio::select! {
                biased;
                sent = &mut abort => break sent.unwrap_or(false),
                output = forward.as_mut(), if ended.is_none() => ended = Some(output),
            }
        };
        self.log(serde_json::json!({
            "event": "router_request_abort",
            "cut": cut.name(),
            "sent": sent,
        }));
        let outcome = self.settle(sent, ended, forward, idle, final_output).await;
        let (settled, reason) = match outcome {
            Ok(()) => ("acknowledged", None),
            Err(reason) => ("fallback", Some(reason)),
        };
        self.log(serde_json::json!({
            "event": "router_abort_settled",
            "outcome": settled,
            "reason": reason,
            "elapsed_ms": u64::try_from(asked.elapsed().as_millis()).unwrap_or(u64::MAX),
        }));
    }

    /// The rest of [`AfterCut::read`] once the abort was asked: `Ok` when the
    /// lease was acknowledged, else why it was left to today's rule.
    async fn settle<F: Future>(
        &self,
        sent: bool,
        ended: Option<F::Output>,
        forward: std::pin::Pin<&mut F>,
        idle: std::time::Duration,
        final_output: impl Fn(&F::Output) -> bool,
    ) -> Result<(), &'static str> {
        if !sent {
            return Err("abort_not_sent");
        }
        let output = match ended {
            Some(output) => output,
            None => tokio::time::timeout(idle, forward)
                .await
                .map_err(|_| "bound_passed")?,
        };
        if !final_output(&output) {
            return Err("no_final_output");
        }
        match self
            .authority
            .acknowledge_cancelled_request(&self.lease_id)
            .await
        {
            Ok(true) => Ok(()),
            Ok(false) => Err("lease_not_cancelling"),
            // A failed write leaves the lease charged, which is the
            // conservative outcome; it is reported as a failed close is.
            Err(error) => {
                capyctl_domain::role_log::notice(
                    capyctl_domain::role_log::Level::Warning,
                    &format!(
                        "request lease {} stays charged: {error}{}",
                        self.lease_id,
                        self.ids.log_suffix()
                    ),
                );
                Err("acknowledgement_failed")
            }
        }
    }

    /// One role-log line about this request, with its IDs and lease.
    fn log(&self, mut event: serde_json::Value) {
        if let Some(fields) = event.as_object_mut() {
            fields.insert("deployment".into(), self.deployment.clone().into());
            fields.insert("generation".into(), self.generation.into());
            fields.insert("lease".into(), self.lease_id.clone().into());
        }
        self.ids.log_fields(&mut event);
        capyctl_domain::role_log::event(event);
    }
}

pub fn stream_response(
    forward: Arc<dyn capyctl_adapters::traits::ChatForward>,
    body: capyctl_adapters::request::InferenceRequest,
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
    body: capyctl_adapters::request::InferenceRequest,
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
/// line before `data: [DONE]`; every completed stream carries its engine
/// metrics as another ([`crate::engine_metrics`]). The stream is bounded by
/// `bounds` (SPEC §10).
pub fn stream_planned_timed(
    first: crate::balance::Attempt,
    mut plan: Option<crate::balance::Plan>,
    body: capyctl_adapters::request::InferenceRequest,
    guard: StaticStreamGuard,
    timing: crate::timing::RequestTiming,
    bounds: StreamBounds,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(16);
    // ADR 0032 §3: a cut on a capable lane ends the client's stream at once
    // (after what is already queued), while the forward goes on being read.
    let cut = Arc::new(tokio::sync::watch::Sender::new(false));
    let mut client_cut = cut.subscribe();
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
            report: Default::default(),
            request_id: None,
            cut: cut.clone(),
        };
        let mut attempt = first;
        let mut durable = false;
        // SPEC §10 (owner decision 2026-10-18): a completed stream's attempt,
        // whose lease is closed only after the client has the whole stream.
        let mut completed = None;
        let (result, refusal) = loop {
            let generation = attempt.generation;
            sink.timing.forwarding(attempt.instance, generation);
            // ADR 0032 §1: on a capable lane the sink names CapyCTL's request
            // ID, which the forwarder sends to the engine.
            sink.request_id = attempt.request_id();
            let capable = sink.request_id.is_some();
            let stall = attempt.stall.take();
            // SPEC §10 (found live 2026-10-22): the client's hang-up is
            // noticed when its response body is dropped, not at the next
            // chunk the sink delivers. Before the first token there is none
            // to deliver, and live on SGLang a queued stream was cancelled
            // only once the engine started it. Dropping the forward closes
            // the engine connection; a forward whose end is ready at the
            // same moment still ends with that end.
            let gone = sink.tx.clone();
            let mut delivery_cut = cut.subscribe();
            let result = {
                let forwarder = attempt.forward.clone();
                let mut forward =
                    std::pin::pin!(forwarder.forward_chat_stream_async(&body, &mut sink));
                // `cut_by`: the router cut the forward, which has not ended.
                let (result, cut_by) = tokio::select! {
                    biased;
                    result = bounded(forward.as_mut(), &progress, &bounds, stall) => {
                        let cut_by = result.is_err().then(|| CutBy::bound(progress.produced()));
                        (result, cut_by)
                    }
                    () = gone.closed() => (Ok(Ok(StreamEnded::Cancelled)), Some(CutBy::HangUp)),
                    // ADR 0032 §3: on a capable lane a failed or slow
                    // delivery leaves the forward reading; the sink marks
                    // the cut instead.
                    _ = delivery_cut.wait_for(|cut| *cut), if capable => {
                        (Ok(Ok(StreamEnded::Cancelled)), Some(CutBy::Delivery))
                    }
                };
                drop(gone);
                // ADR 0032 §3: a cut on a capable lane (a hang-up, a failed or
                // slow delivery, the first-output deadline, the idle bound)
                // settles the lease as below and ends the client's stream,
                // then reads the same response to its own end, discarding
                // it, after asking the engine to abort the request.
                if let Some((cut_by, after_cut)) =
                    cut_by.and_then(|cut_by| Some((cut_by, attempt.after_cut()?)))
                {
                    let end = stream_lease_end(&result, progress.produced());
                    let durable = attempt.settle(end).await;
                    cut.send_replace(true);
                    if durable {
                        guard.release();
                    }
                    after_cut
                        .read(cut_by, forward, bounds.idle, |ended| {
                            matches!(ended, Ok(StreamEnded::Completed))
                        })
                        .await;
                    return;
                }
                result
            };
            if matches!(result, Ok(Ok(StreamEnded::Completed))) {
                completed = Some(attempt);
                break (result, None);
            }
            let end = stream_lease_end(&result, progress.produced());
            durable = attempt.settle(end).await;
            // SPEC §10, T19: a deterministic refusal decided before sending is
            // answered in-band; another instance would refuse it the same way.
            if let Ok(Err(AdapterError::Rejected { message, .. })) = &result {
                // SPEC §10 (found live 2026-09-24): the engine's complete
                // invalid-request answer, relayed in-band; not retryable.
                let message = message.clone();
                break (result, Some(("engine_rejected".to_owned(), message)));
            }
            // SPEC §10 (amended 2026-10-17, found live): the engine's error
            // answer before the stream started, relayed in-band; not
            // retryable, and never offered to another instance. Its lease is
            // cancelling.
            if let Ok(Err(AdapterError::EngineError { message, .. })) = &result {
                let message = message.clone();
                break (result, Some(("engine_error".to_owned(), message)));
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
            // Nothing reached any engine, or the engine answered completely;
            // say so in-band. An engine's error answer leaves its lease
            // cancelling, so the slot is released only as for an uncertain end.
            if code != "engine_error" || durable {
                guard.release();
            }
            let retryable = !matches!(
                code.as_str(),
                "invalid_request" | "unsupported" | "engine_rejected" | "engine_error"
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
            // SPEC §10 (owner decision 2026-10-18): the stream's last lines go
            // out without waiting for its lease close, which commits beside
            // them; the slot is released once the close is written, never
            // before, and a stalled client does not hold it.
            if let Some(attempt) = completed {
                tokio::spawn(async move {
                    attempt
                        .settle(capyctl_controller::LeaseEnd::Completed)
                        .await;
                    guard.release();
                });
            }
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
                // SPEC §17 (owner decision 2026-10-09): the request's engine
                // metrics, on every completed stream, as an SSE comment.
                let line = format!(
                    "{} {}",
                    crate::engine_metrics::METRICS_COMMENT,
                    crate::engine_metrics::metrics(&sink.report, &sink.timing)
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
        // durable lease it stays charged in the ledger (cancelling or uncertain)
        // until settled, so the per-process slot is released; left taken, every
        // uncertain stream would shrink the deployment's bound for the life of
        // the process. Without a durable ledger the slot is the only record
        // and stays taken. A panic retains it either way.
        else if durable {
            guard.release();
        }
        drop(sink);
    });
    let stream = async_stream::stream! {
        // ADR 0032 §3: at a cut on a capable lane the channel takes nothing
        // more; what was already queued is still delivered, as when a cut
        // dropped the sink. The cut is also read once the pump has ended,
        // when the channel has no sender left anyway.
        let mut open = true;
        loop {
            let next = if open {
                tokio::select! {
                    biased;
                    _ = client_cut.wait_for(|cut| *cut) => None,
                    event = rx.recv() => Some(event),
                }
            } else {
                Some(rx.recv().await)
            };
            match next {
                None => {
                    rx.close();
                    open = false;
                }
                Some(Some(event)) => yield event,
                Some(None) => break,
            }
        }
        let _ = pump.is_finished();
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

//! ADR 0037 §8: spans leave through a bounded, memory-only queue drained by
//! one background task, as OTLP over HTTP with protobuf encoding, never on the
//! request path. A full queue drops and counts; an export failure is retried
//! a bounded number of times and then dropped. Neither ever delays, fails or
//! changes a request, a readiness check or a lifecycle step.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SpanData, SpanProcessor};
use opentelemetry_sdk::Resource;
use tokio::sync::Notify;
use tokio::time::Instant;

/// ADR 0037 §8 (the OpenTelemetry SDK defaults): at most 2048 queued spans.
pub const QUEUE_CAPACITY: usize = 2048;
/// At most 512 spans per export request.
pub const BATCH_SIZE: usize = 512;
/// The queue is drained every 5 seconds.
pub const EXPORT_INTERVAL: Duration = Duration::from_secs(5);
/// A stopping role flushes for at most 2 seconds.
pub const SHUTDOWN_FLUSH: Duration = Duration::from_secs(2);
/// One export request's bound (the OTLP exporter default).
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
/// OTLP/HTTP retryable answers are retried this many times, backing off.
const RETRIES: u32 = 3;
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
/// The longest `Retry-After` honoured before giving the batch up.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
/// ADR 0037 §8: dropped spans and export failures are reported at most once
/// a minute.
pub const NOTICE_INTERVAL: Duration = Duration::from_secs(60);

/// What status shows about a role's export (ADR 0037 §13).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TraceStats {
    /// Spans dropped since the role started: queue full, or export given up.
    pub dropped: u64,
    /// Export requests that failed since the start.
    pub export_failures: u64,
    /// The last export error, as a short class (`HTTP 503`, `timeout`,
    /// `connection failed`); never a collector's response body or a header.
    pub last_export_error: Option<String>,
}

/// The bounded queue between span ends and the export task.
#[derive(Debug)]
pub(crate) struct Queue {
    spans: Mutex<VecDeque<SpanData>>,
    capacity: usize,
    batch: usize,
    dropped: AtomicU64,
    failures: AtomicU64,
    last_error: Mutex<Option<String>>,
    wake: Notify,
    stopping: AtomicBool,
    stop: Notify,
}

impl Queue {
    pub(crate) fn new(capacity: usize, batch: usize) -> Arc<Queue> {
        Arc::new(Queue {
            spans: Mutex::new(VecDeque::new()),
            capacity,
            batch,
            dropped: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            last_error: Mutex::new(None),
            wake: Notify::new(),
            stopping: AtomicBool::new(false),
            stop: Notify::new(),
        })
    }

    /// Never blocks beyond a short lock and never fails: a full queue drops
    /// the span and counts it (ADR 0037 §8).
    pub(crate) fn push(&self, span: SpanData) {
        let len = {
            let mut spans = self.spans.lock().unwrap_or_else(|e| e.into_inner());
            if spans.len() >= self.capacity {
                drop(spans);
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            spans.push_back(span);
            spans.len()
        };
        if len >= self.batch {
            self.wake.notify_one();
        }
    }

    fn take_batch(&self) -> Vec<SpanData> {
        let mut spans = self.spans.lock().unwrap_or_else(|e| e.into_inner());
        let n = spans.len().min(self.batch);
        spans.drain(..n).collect()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.spans.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn failed(&self, spans: usize, error: String) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        self.dropped.fetch_add(spans as u64, Ordering::Relaxed);
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
    }

    pub(crate) fn stats(&self) -> TraceStats {
        TraceStats {
            dropped: self.dropped.load(Ordering::Relaxed),
            export_failures: self.failures.load(Ordering::Relaxed),
            last_export_error: self
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    pub(crate) fn request_stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.stop.notify_one();
    }
}

/// The span processor: a sampled span that ends goes to the queue; nothing
/// else happens on the caller's thread.
#[derive(Debug)]
pub(crate) struct QueueProcessor(pub(crate) Arc<Queue>);

impl SpanProcessor for QueueProcessor {
    fn on_start(&self, _span: &mut opentelemetry_sdk::trace::Span, _cx: &opentelemetry::Context) {}

    fn on_end(&self, span: SpanData) {
        if span.span_context.is_sampled() {
            self.0.push(span);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

/// The collector endpoint and its headers, built from CapyCTL's settings
/// only (ADR 0037 §8: no ambient `OTEL_*` configuration).
#[derive(Clone)]
pub(crate) struct OtlpHttp {
    pub(crate) client: reqwest::Client,
    pub(crate) endpoint: reqwest::Url,
    pub(crate) headers: reqwest::header::HeaderMap,
}

/// Why one export request did not succeed.
enum Failure {
    Retryable {
        error: String,
        after: Option<Duration>,
    },
    Final(String),
}

impl OtlpHttp {
    async fn post(&self, body: Vec<u8>) -> Result<(), Failure> {
        let response = self
            .client
            .post(self.endpoint.clone())
            .headers(self.headers.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf")
            .timeout(EXPORT_TIMEOUT)
            .body(body)
            .send()
            .await
            .map_err(|e| Failure::Retryable {
                error: error_class(&e),
                after: None,
            })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let error = format!("HTTP {}", status.as_u16());
        // OTLP/HTTP: 429, 502, 503 and 504 are retryable, honouring
        // `Retry-After` in seconds; anything else is final.
        if matches!(status.as_u16(), 429 | 502 | 503 | 504) {
            let after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            Err(Failure::Retryable { error, after })
        } else {
            Err(Failure::Final(error))
        }
    }
}

/// A short, fixed description: no URL query, header or body text.
fn error_class(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timeout".to_owned()
    } else if error.is_connect() {
        "connection failed".to_owned()
    } else {
        "request failed".to_owned()
    }
}

/// Encode one batch as an OTLP `ExportTraceServiceRequest`.
pub(crate) fn encode(batch: Vec<SpanData>, resource: &Resource) -> Vec<u8> {
    use prost::Message;
    let resource = ResourceAttributesWithSchema::from(resource);
    ExportTraceServiceRequest {
        resource_spans: group_spans_by_resource_and_scope(batch, &resource),
    }
    .encode_to_vec()
}

/// The background export task: one per role (ADR 0037 §8).
pub(crate) async fn run(queue: Arc<Queue>, http: OtlpHttp, resource: Resource) {
    let mut notices = Notices::default();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(EXPORT_INTERVAL) => {}
            _ = queue.wake.notified() => {}
            _ = queue.stop.notified() => {}
        }
        loop {
            let batch = queue.take_batch();
            if batch.is_empty() {
                break;
            }
            let retry = !queue.stopping.load(Ordering::SeqCst);
            export(&queue, &http, &resource, batch, retry).await;
        }
        notices.maybe_report(&queue.stats(), Instant::now());
        if queue.stopping.load(Ordering::SeqCst) {
            return;
        }
    }
}

async fn export(
    queue: &Queue,
    http: &OtlpHttp,
    resource: &Resource,
    batch: Vec<SpanData>,
    retry: bool,
) {
    let count = batch.len();
    let body = encode(batch, resource);
    let mut backoff = FIRST_BACKOFF;
    let mut attempt = 0;
    loop {
        match http.post(body.clone()).await {
            Ok(()) => return,
            Err(Failure::Final(error)) => return queue.failed(count, error),
            Err(Failure::Retryable { error, after }) => {
                let wait = after.unwrap_or(backoff);
                if !retry || attempt >= RETRIES || wait > MAX_RETRY_AFTER {
                    return queue.failed(count, error);
                }
                attempt += 1;
                // A stop request ends the backoff early: shutdown flushes for
                // at most 2 seconds (ADR 0037 §8).
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = queue.stop.notified() => return queue.failed(count, error),
                }
                backoff *= 2;
            }
        }
    }
}

/// ADR 0037 §8: a role log notice for dropped spans and export failures, at
/// most once a minute.
#[derive(Default)]
pub(crate) struct Notices {
    last: Option<Instant>,
    reported: TraceStats,
}

impl Notices {
    /// The notice text when one is due, recording it as sent.
    pub(crate) fn due(&mut self, stats: &TraceStats, now: Instant) -> Option<String> {
        let changed = stats.dropped > self.reported.dropped
            || stats.export_failures > self.reported.export_failures;
        let quiet = self
            .last
            .is_none_or(|last| now.duration_since(last) >= NOTICE_INTERVAL);
        if !changed || !quiet {
            return None;
        }
        self.last = Some(now);
        self.reported = stats.clone();
        Some(format!(
            "tracing: {} spans dropped and {} export failures since the start; last export error: {}",
            stats.dropped,
            stats.export_failures,
            stats.last_export_error.as_deref().unwrap_or("none"),
        ))
    }

    fn maybe_report(&mut self, stats: &TraceStats, now: Instant) {
        if let Some(message) = self.due(stats, now) {
            capyctl_domain::role_log::notice(capyctl_domain::role_log::Level::Warning, &message);
        }
    }
}

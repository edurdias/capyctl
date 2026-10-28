//! ADR 0037 implementation plan step 2: a test exporter. A [`Tracing`] whose
//! sampled spans are kept in memory as they end, for tests in every crate
//! that records spans (T48).

use std::sync::{Arc, Mutex};

use opentelemetry::trace::{Status, TracerProvider as _};
use opentelemetry::Value;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SpanData, SpanProcessor};

use crate::{export, CallerMode, Inner, Kind, Service, SpanCx, Tracing};

/// A finished span as a test reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub name: String,
    pub kind: Kind,
    pub cx: SpanCx,
    /// The parent span ID, when the span has a parent.
    pub parent: Option<[u8; 8]>,
    pub links: Vec<SpanCx>,
    /// Keys and values (values as text).
    pub attributes: Vec<(String, String)>,
    pub events: Vec<String>,
    /// An error span's status description.
    pub error: Option<String>,
}

impl Recorded {
    /// One attribute's value as text.
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// The spans a test [`Tracing`] recorded.
#[derive(Clone, Debug, Default)]
pub struct Capture {
    pub(crate) spans: Arc<Mutex<Vec<SpanData>>>,
    service: Option<Service>,
}

#[derive(Debug)]
struct CaptureProcessor(Arc<Mutex<Vec<SpanData>>>);

impl SpanProcessor for CaptureProcessor {
    fn on_start(&self, _span: &mut opentelemetry_sdk::trace::Span, _cx: &opentelemetry::Context) {}

    fn on_end(&self, span: SpanData) {
        if span.span_context.is_sampled() {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).push(span);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: std::time::Duration) -> OTelSdkResult {
        Ok(())
    }
}

/// A [`Tracing`] that records in memory: the same sampler, resource and span
/// API as a role's, without an exporter or a task.
pub fn capture(sample_ratio: f64, caller_mode: CallerMode, service: Service) -> (Tracing, Capture) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let provider = crate::provider(
        sample_ratio,
        crate::resource(service),
        CaptureProcessor(spans.clone()),
    );
    let tracing = Tracing {
        inner: Some(Arc::new(Inner {
            tracer: provider.tracer("capyctl"),
            _provider: provider,
            caller_mode,
            endpoint: None,
            queue: None,
            task: Mutex::new(None),
        })),
    };
    (
        tracing,
        Capture {
            spans,
            service: Some(service),
        },
    )
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.as_str().to_owned(),
        other => other.to_string(),
    }
}

impl Capture {
    /// Every recorded span, in the order they ended.
    pub fn spans(&self) -> Vec<Recorded> {
        self.spans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|s| Recorded {
                name: s.name.to_string(),
                kind: match s.span_kind {
                    opentelemetry::trace::SpanKind::Server => Kind::Server,
                    opentelemetry::trace::SpanKind::Client => Kind::Client,
                    _ => Kind::Internal,
                },
                cx: SpanCx {
                    trace_id: s.span_context.trace_id().to_bytes(),
                    span_id: s.span_context.span_id().to_bytes(),
                    flags: s.span_context.trace_flags().to_u8(),
                },
                parent: (s.parent_span_id != opentelemetry::trace::SpanId::INVALID)
                    .then(|| s.parent_span_id.to_bytes()),
                links: s
                    .links
                    .iter()
                    .map(|l| SpanCx {
                        trace_id: l.span_context.trace_id().to_bytes(),
                        span_id: l.span_context.span_id().to_bytes(),
                        flags: l.span_context.trace_flags().to_u8(),
                    })
                    .collect(),
                attributes: s
                    .attributes
                    .iter()
                    .map(|kv| (kv.key.as_str().to_owned(), text(&kv.value)))
                    .collect(),
                events: s.events.iter().map(|e| e.name.to_string()).collect(),
                error: match &s.status {
                    Status::Error { description } => Some(description.to_string()),
                    _ => None,
                },
            })
            .collect()
    }

    /// The recorded spans with one name.
    pub fn named(&self, name: &str) -> Vec<Recorded> {
        self.spans()
            .into_iter()
            .filter(|s| s.name == name)
            .collect()
    }

    /// Every recorded span serialized exactly as the exporter would send it
    /// (OTLP protobuf, with the role's resource), for sentinel searches.
    pub fn encoded(&self) -> Vec<u8> {
        let spans = self.spans.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let service = self.service.unwrap_or(Service::Standalone);
        export::encode(spans, &crate::resource(service))
    }

    /// Every attribute key outside ADR 0037 §5's allow-list (T48).
    pub fn disallowed_keys(&self) -> Vec<String> {
        self.spans()
            .into_iter()
            .flat_map(|s| s.attributes.into_iter().map(|(k, _)| k))
            .filter(|k| !crate::ALLOWED_KEYS.contains(&k.as_str()))
            .collect()
    }

    /// Forget what was recorded.
    pub fn clear(&self) {
        self.spans.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// Whether `needle` occurs anywhere in `haystack` (sentinel searches).
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

//! ADR 0037: CapyCTL's part in a distributed trace. W3C Trace Context in and
//! out, a fixed set of spans with an allow-listed set of attributes, and an
//! opt-in OTLP/HTTP export that is never on the request path.
//!
//! [`Tracing::off`] is the default and costs nothing: no header is read, no
//! exporter is built, no task runs and every [`Span`] is empty (ADR 0037 §1).

pub mod attr;
pub mod export;
pub mod testing;
pub mod w3c;

use std::sync::{Arc, Mutex};

use opentelemetry::trace::{
    Link, Span as _, SpanContext, SpanId, SpanKind, Status, TraceContextExt, TraceFlags, TraceId,
    TraceState, Tracer as _, TracerProvider as _,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{
    RandomIdGenerator, Sampler, SamplingDecision, SamplingResult, SdkTracerProvider, ShouldSample,
    SpanLimits, SpanProcessor,
};
use opentelemetry_sdk::Resource;

pub use attr::{ActivationKind, Attr, CancelCause, Endpoint, Outcome, ALLOWED_KEYS, RESOURCE_KEYS};
pub use export::TraceStats;
pub use w3c::{Incoming, SpanCx, TRACEPARENT, TRACESTATE};

/// What the router does with a caller's valid context (ADR 0037 §2,
/// `observability.tracing.caller_context`, owner decision 2026-10-27 q3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CallerMode {
    /// The caller's context is the request span's parent (the default).
    #[default]
    Parent,
    /// A new trace whose request span links to the caller's context.
    Link,
    /// Neither header is read.
    Ignore,
}

/// The role a [`Tracing`] belongs to: the resource's `service.name` and
/// `capyctl.role` (ADR 0037 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Server,
    Host,
    Standalone,
}

impl Service {
    fn service_name(self) -> &'static str {
        match self {
            Service::Server => "capyctl-server",
            Service::Host => "capyctl-host",
            Service::Standalone => "capyctl-standalone",
        }
    }

    fn role(self) -> &'static str {
        match self {
            Service::Server => "server",
            Service::Host => "host",
            Service::Standalone => "standalone",
        }
    }
}

/// A role's tracing settings, already validated by `capyctl-config` (ADR
/// 0037 §12). Nothing here is read from the environment (§8).
#[derive(Debug, Clone)]
pub struct Settings {
    /// The OTLP/HTTP traces URL, used as given (for example
    /// `http://127.0.0.1:4318/v1/traces`).
    pub endpoint: String,
    /// Headers from `headers_file`; values are never logged or shown.
    pub headers: Vec<(String, String)>,
    /// The root sampling ratio, 0.0 to 1.0 (§9).
    pub sample_ratio: f64,
    pub caller_mode: CallerMode,
    pub service: Service,
}

/// Why tracing could not start (the role refuses its start).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StartError {
    #[error("observability.tracing.endpoint is not an http or https URL")]
    Endpoint,
    #[error("observability.tracing.headers_file: header {0} is not a valid HTTP header")]
    Header(usize),
    #[error("observability.tracing: the HTTP client could not be built")]
    Client,
}

/// The fixed span names (ADR 0037 §3, §4) and their kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Name {
    Request,
    Admission,
    ActivationWait,
    QueueWait,
    Dispatch,
    Forward,
    Settle,
    Ingress,
    Activation,
    Drain,
    Release,
    Reserve,
    Launch,
    Wake,
    Readiness,
    Canary,
    HostLaunch,
    HostWake,
    HostPark,
    HostStop,
    HostVerifyRelease,
}

impl Name {
    pub fn as_str(self) -> &'static str {
        match self {
            Name::Request => "capyctl.request",
            Name::Admission => "capyctl.admission",
            Name::ActivationWait => "capyctl.activation_wait",
            Name::QueueWait => "capyctl.queue_wait",
            Name::Dispatch => "capyctl.dispatch",
            Name::Forward => "capyctl.forward",
            Name::Settle => "capyctl.settle",
            Name::Ingress => "capyctl.ingress",
            Name::Activation => "capyctl.activation",
            Name::Drain => "capyctl.drain",
            Name::Release => "capyctl.release",
            Name::Reserve => "capyctl.reserve",
            Name::Launch => "capyctl.launch",
            Name::Wake => "capyctl.wake",
            Name::Readiness => "capyctl.readiness",
            Name::Canary => "capyctl.canary",
            Name::HostLaunch => "capyctl.host.launch",
            Name::HostWake => "capyctl.host.wake",
            Name::HostPark => "capyctl.host.park",
            Name::HostStop => "capyctl.host.stop",
            Name::HostVerifyRelease => "capyctl.host.verify_release",
        }
    }

    /// ADR 0037 §3: `capyctl.request` and `capyctl.ingress` are server
    /// spans, `capyctl.forward` a client span, everything else internal.
    pub fn kind(self) -> Kind {
        match self {
            Name::Request | Name::Ingress => Kind::Server,
            Name::Forward => Kind::Client,
            _ => Kind::Internal,
        }
    }
}

/// A span's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Server,
    Client,
    Internal,
}

impl Kind {
    fn otel(self) -> SpanKind {
        match self {
            Kind::Server => SpanKind::Server,
            Kind::Client => SpanKind::Client,
            Kind::Internal => SpanKind::Internal,
        }
    }
}

/// Where a new span hangs.
#[derive(Debug, Clone, Copy)]
pub enum Parent<'a> {
    /// A new trace.
    Root,
    /// A span of this role that is still open.
    Span(&'a Span),
    /// A span of this role known by its stored context (a `capyctl.settle`
    /// after its request span ended, ADR 0037 §3).
    Cx(SpanCx),
    /// Another role's or the caller's span, with its `tracestate`.
    Remote(&'a Incoming),
}

/// The trace headers for the next hop, with a span of this role as parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    pub traceparent: String,
    pub tracestate: Option<String>,
}

impl Outbound {
    /// The context these headers carry, as the next hop reads it.
    pub fn incoming(&self) -> Option<Incoming> {
        Some(Incoming {
            parent: SpanCx::parse_traceparent(&self.traceparent)?,
            tracestate: self.tracestate.clone(),
        })
    }
}

/// A role's tracing handle: `off` unless the role's document names an
/// endpoint (ADR 0037 §1). Cheap to clone.
#[derive(Clone, Default)]
pub struct Tracing {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    tracer: opentelemetry_sdk::trace::Tracer,
    // Held so the tracer's processors live as long as the handle.
    _provider: SdkTracerProvider,
    caller_mode: CallerMode,
    endpoint: Option<String>,
    queue: Option<Arc<export::Queue>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// ADR 0037 §3: `capyctl.settle` spans of cancelled requests, open until
    /// their cancellation settles, keyed by lease ID. Memory only: a role
    /// restart loses them, and those settlements are then not traced.
    pending: Mutex<std::collections::HashMap<String, Span>>,
}

/// At most this many cancelled requests' settlements are held open; past it a
/// settlement span ends at once, as `cancelling`.
pub const MAX_PENDING_SETTLES: usize = 4096;

impl std::fmt::Debug for Tracing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            None => f.write_str("Tracing(off)"),
            Some(inner) => write!(f, "Tracing(on, {:?})", inner.endpoint),
        }
    }
}

/// ADR 0037 §9: `ParentBased(root = TraceIdRatioBased(ratio))`, except that
/// an activation root is always recorded (§4: activation traces ignore the
/// ratio).
#[derive(Debug, Clone)]
struct CapySampler {
    requests: Sampler,
}

impl ShouldSample for CapySampler {
    fn should_sample(
        &self,
        parent_context: Option<&Context>,
        trace_id: TraceId,
        name: &str,
        span_kind: &SpanKind,
        attributes: &[KeyValue],
        links: &[Link],
    ) -> SamplingResult {
        let has_parent = parent_context.is_some_and(|cx| cx.has_active_span());
        if !has_parent && name == Name::Activation.as_str() {
            return SamplingResult {
                decision: SamplingDecision::RecordAndSample,
                attributes: Vec::new(),
                trace_state: TraceState::default(),
            };
        }
        self.requests
            .should_sample(parent_context, trace_id, name, span_kind, attributes, links)
    }
}

impl Tracing {
    /// Tracing off: nothing is read, built, started or sent.
    pub fn off() -> Tracing {
        Tracing { inner: None }
    }

    /// Start a role's tracing: the exporter and its one background task.
    /// Must run inside a tokio runtime.
    pub fn start(settings: Settings) -> Result<Tracing, StartError> {
        let endpoint = reqwest::Url::parse(&settings.endpoint).map_err(|_| StartError::Endpoint)?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host().is_none() {
            return Err(StartError::Endpoint);
        }
        let mut headers = reqwest::header::HeaderMap::new();
        for (i, (name, value)) in settings.headers.iter().enumerate() {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| StartError::Header(i + 1))?;
            let mut value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| StartError::Header(i + 1))?;
            // ADR 0037 §8: the values are credentials.
            value.set_sensitive(true);
            headers.append(name, value);
        }
        let client = reqwest::Client::builder()
            .build()
            .map_err(|_| StartError::Client)?;
        let queue = export::Queue::new(export::QUEUE_CAPACITY, export::BATCH_SIZE);
        let resource = resource(settings.service);
        let provider = provider(
            settings.sample_ratio,
            resource.clone(),
            export::QueueProcessor(queue.clone()),
        );
        let http = export::OtlpHttp {
            client,
            endpoint,
            headers,
        };
        let task = tokio::spawn(export::run(queue.clone(), http, resource));
        Ok(Tracing {
            inner: Some(Arc::new(Inner {
                tracer: provider.tracer("capyctl"),
                _provider: provider,
                caller_mode: settings.caller_mode,
                endpoint: Some(settings.endpoint),
                queue: Some(queue),
                task: Mutex::new(Some(task)),
                pending: Mutex::default(),
            })),
        })
    }

    /// Whether tracing is on for this role.
    pub fn is_on(&self) -> bool {
        self.inner.is_some()
    }

    /// The configured endpoint, for status (ADR 0037 §13).
    pub fn endpoint(&self) -> Option<&str> {
        self.inner.as_ref()?.endpoint.as_deref()
    }

    /// Spans dropped and the last export error since the start (§13).
    pub fn stats(&self) -> Option<TraceStats> {
        Some(self.inner.as_ref()?.queue.as_ref()?.stats())
    }

    /// ADR 0037 §8: a stopping role flushes the queue for at most 2 seconds.
    pub async fn shutdown(&self) {
        let Some(inner) = &self.inner else { return };
        let Some(queue) = &inner.queue else { return };
        let task = inner.task.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(mut task) = task else { return };
        queue.request_stop();
        if tokio::time::timeout(export::SHUTDOWN_FLUSH, &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }

    /// ADR 0037 §3: keep a cancelled request's open `capyctl.settle` span
    /// until its cancellation settles ([`Tracing::settled`]).
    pub fn hold(&self, lease: &str, mut span: Span) {
        let Some(inner) = &self.inner else { return };
        let mut pending = inner.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.len() >= MAX_PENDING_SETTLES {
            drop(pending);
            span.set(Attr::Outcome(Outcome::Cancelling));
            span.end();
            return;
        }
        pending.insert(lease.to_owned(), span);
    }

    /// ADR 0037 §3: the cancellation of the request holding `lease` settled
    /// (its final output, the engine's idle reading, or the instance's
    /// verified exit): its `capyctl.settle` span ends now. Nothing when no
    /// span is held for it.
    pub fn settled(&self, lease: &str, outcome: Outcome) {
        let Some(inner) = &self.inner else { return };
        let span = inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(lease);
        if let Some(mut span) = span {
            span.set(Attr::Outcome(outcome));
            span.end();
        }
    }

    /// The caller's context as ADR 0037 §2 says to read it: only when
    /// tracing is on and `caller_context` is not `ignore`; malformed headers
    /// are `None`, never an error. The router calls this only after the
    /// request authenticated.
    pub fn caller(&self, headers: &http::HeaderMap) -> Option<Incoming> {
        let inner = self.inner.as_ref()?;
        if inner.caller_mode == CallerMode::Ignore {
            return None;
        }
        Incoming::read(headers)
    }

    /// The request span (`capyctl.request`, server kind) for an
    /// authenticated request: the caller's context is its parent (`parent`)
    /// or a link of a new trace (`link`), §2.
    pub fn request(&self, caller: Option<&Incoming>) -> Span {
        let Some(inner) = &self.inner else {
            return Span::none();
        };
        match (inner.caller_mode, caller) {
            (CallerMode::Parent, Some(incoming)) => {
                self.span(Name::Request, Parent::Remote(incoming), &[])
            }
            (CallerMode::Link, Some(incoming)) => {
                self.span(Name::Request, Parent::Root, &[incoming.parent])
            }
            _ => self.span(Name::Request, Parent::Root, &[]),
        }
    }

    /// Start a span. When tracing is off the span is empty.
    pub fn span(&self, name: Name, parent: Parent<'_>, links: &[SpanCx]) -> Span {
        let Some(inner) = &self.inner else {
            return Span::none();
        };
        let parent_cx = match parent {
            Parent::Root => None,
            Parent::Span(span) => span.inner.as_ref().map(|s| s.span_context().clone()),
            Parent::Cx(cx) => Some(span_context(cx, false, TraceState::default())),
            Parent::Remote(incoming) => Some(span_context(
                incoming.parent,
                true,
                incoming
                    .tracestate
                    .as_deref()
                    .and_then(|s| s.parse::<TraceState>().ok())
                    .unwrap_or_default(),
            )),
        };
        let cx = match parent_cx {
            Some(sc) if sc.is_valid() => Context::new().with_remote_span_context(sc),
            _ => Context::new(),
        };
        let links = links
            .iter()
            .map(|l| Link::with_context(span_context(*l, true, TraceState::default())))
            .collect();
        let span = inner
            .tracer
            .span_builder(name.as_str())
            .with_kind(name.kind().otel())
            .with_links(links)
            .start_with_context(&inner.tracer, &cx);
        Span { inner: Some(span) }
    }
}

fn span_context(cx: SpanCx, remote: bool, state: TraceState) -> SpanContext {
    SpanContext::new(
        TraceId::from_bytes(cx.trace_id),
        SpanId::from_bytes(cx.span_id),
        TraceFlags::new(cx.flags),
        remote,
        state,
    )
}

/// ADR 0037 §5: `service.name`, `service.version` and `capyctl.role`; an
/// empty base, so no environment detector adds a host or process attribute
/// and `OTEL_SERVICE_NAME`/`OTEL_RESOURCE_ATTRIBUTES` change nothing (§8).
fn resource(service: Service) -> Resource {
    Resource::builder_empty()
        .with_attributes([
            KeyValue::new("service.name", service.service_name()),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("capyctl.role", service.role()),
        ])
        .build()
}

/// A provider built from settings only: an explicit sampler, limits, ID
/// generator and resource replace everything the SDK's defaults read from
/// `OTEL_*` (ADR 0037 §8).
fn provider(
    ratio: f64,
    resource: Resource,
    processor: impl SpanProcessor + 'static,
) -> SdkTracerProvider {
    SdkTracerProvider::builder()
        .with_sampler(CapySampler {
            requests: Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio))),
        })
        .with_span_limits(SpanLimits::default())
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource)
        .with_span_processor(processor)
        .build()
}

/// One span; empty when tracing is off. Ends when [`Span::end`] is called or
/// when dropped.
#[derive(Debug, Default)]
pub struct Span {
    inner: Option<opentelemetry_sdk::trace::Span>,
}

impl Span {
    /// An empty span: every call is a no-op.
    pub fn none() -> Span {
        Span { inner: None }
    }

    /// Set one allow-listed attribute (ADR 0037 §5).
    pub fn set(&mut self, attr: Attr) {
        if let Some(span) = &mut self.inner {
            if span.is_recording() {
                span.set_attribute(attr.into_key_value());
            }
        }
    }

    /// ADR 0037 §3: the first backend-progress chunk, with no attributes.
    pub fn first_progress(&mut self) {
        if let Some(span) = &mut self.inner {
            span.add_event("first_progress", Vec::new());
        }
    }

    /// As [`Span::first_progress`], at the time the chunk arrived.
    pub fn first_progress_at(&mut self, at: std::time::SystemTime) {
        if let Some(span) = &mut self.inner {
            span.add_event_with_timestamp("first_progress", at, Vec::new());
        }
    }

    /// Link this span to another trace's span (ADR 0037 §4).
    pub fn link(&mut self, cx: SpanCx) {
        if let Some(span) = &mut self.inner {
            span.add_link(span_context(cx, false, TraceState::default()), Vec::new());
        }
    }

    /// ADR 0037 §5: an error span's status description is its CapyCTL code,
    /// which is also its `capyctl.code`.
    pub fn fail(&mut self, code: &str) {
        let code = attr::code(code);
        if let Some(span) = &mut self.inner {
            span.set_status(Status::error(code.clone()));
        }
        self.set(Attr::Code(code));
    }

    /// End the span now.
    pub fn end(mut self) {
        if let Some(mut span) = self.inner.take() {
            span.end();
        }
    }

    /// This span's context, also for an unsampled span (its flags clear),
    /// so it can be propagated (ADR 0037 §9).
    pub fn cx(&self) -> Option<SpanCx> {
        let sc = self.inner.as_ref()?.span_context();
        if !sc.is_valid() {
            return None;
        }
        Some(SpanCx {
            trace_id: sc.trace_id().to_bytes(),
            span_id: sc.span_id().to_bytes(),
            flags: sc.trace_flags().to_u8() & SpanCx::SAMPLED,
        })
    }

    /// The headers that make this span the next hop's parent, with the
    /// trace's `tracestate` (ADR 0037 §6).
    pub fn outbound(&self) -> Option<Outbound> {
        let cx = self.cx()?;
        let state = self.inner.as_ref()?.span_context().trace_state().header();
        Some(Outbound {
            traceparent: cx.traceparent(),
            tracestate: (!state.is_empty()).then_some(state),
        })
    }

    /// Whether this span records (tracing on and sampled).
    pub fn is_recording(&self) -> bool {
        self.inner.as_ref().is_some_and(|s| s.is_recording())
    }
}

#[cfg(test)]
mod tests;

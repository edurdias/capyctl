//! SPEC §§10,13.3: a generation-bound, inference-only forwarding boundary.
//! Credentials and request bodies never enter the host command journal.
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use futures::StreamExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use mllm_domain::latency::Histogram;
use subtle::ConstantTimeEq;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressScope {
    pub host_id: String,
    pub deployment_id: String,
    pub binding_id: String,
    pub incarnation: String,
    pub member_id: String,
    pub generation: i64,
    pub revision: i64,
    /// ADR 0013 §5: the instance this launch realizes. Two instances of one
    /// deployment on one host hold separate gates, so a later instance never
    /// displaces a Ready sibling's entry. Absent (instance 0) in a credential
    /// bundle written before instances were keyed.
    #[serde(default)]
    pub instance_index: u32,
}
/// ADR 0013 §5: an entry is keyed by deployment, instance and member.
type EntryKey = (String, u32, String);
fn key(scope: &IngressScope) -> EntryKey {
    (
        scope.deployment_id.clone(),
        scope.instance_index,
        scope.member_id.clone(),
    )
}
#[derive(Debug, thiserror::Error)]
#[error("inference ingress authorization failed")]
pub struct IngressError;
struct Entry {
    scope: IngressScope,
    target: SocketAddr,
    model: String,
    gate: [u8; 32],
    native: [u8; 32],
    open: AtomicBool,
    current: AtomicUsize,
    /// The launch this entry forwards to (its launch command id), bound by the
    /// launch path so load samples name the owned handle the controller holds.
    handle: Mutex<Option<String>>,
    /// SPEC §17 (M80): what this entry's forwards took since the load reporter
    /// last drained them. Bounded by the mllm bucket layout.
    timings: Mutex<IngressTimings>,
}
/// SPEC §17 (M80): the host ingress's own clock for one entry, measured from
/// the moment the request reached the ingress handler. Durations only; no
/// request or response content.
struct IngressTimings {
    headers: Histogram,
    first_byte: Histogram,
    last_byte: Histogram,
}
impl Default for IngressTimings {
    fn default() -> Self {
        Self {
            headers: Histogram::mllm(),
            first_byte: Histogram::mllm(),
            last_byte: Histogram::mllm(),
        }
    }
}
#[derive(Clone, Copy)]
enum Mark {
    Headers,
    FirstByte,
    LastByte,
}
impl Entry {
    fn record(&self, mark: Mark, received: Instant) {
        let seconds = received.elapsed().as_secs_f64();
        let mut timings = self.timings.lock().unwrap_or_else(|p| p.into_inner());
        match mark {
            Mark::Headers => timings.headers.observe(seconds),
            Mark::FirstByte => timings.first_byte.observe(seconds),
            Mark::LastByte => timings.last_byte.observe(seconds),
        }
    }
}
/// SPEC §10, D9: one open scope the host load reporter scrapes on loopback.
/// The native key authenticates the scrape only; it never leaves the host.
#[derive(Clone)]
pub struct LoadTarget {
    pub scope: IngressScope,
    pub owned_handle: String,
    pub target: SocketAddr,
    pub native: [u8; 32],
    pub in_flight: usize,
}
#[derive(Default)]
struct Registry {
    entries: BTreeMap<EntryKey, Arc<Entry>>,
    /// Gate keys spent by any registration: never accepted for another scope
    /// while remembered.
    used: BTreeSet<[u8; 32]>,
    /// SPEC §§6, 13.3: spent keys of retired or replaced entries, oldest first.
    /// Only these are forgotten, and only to make room, so the spent set stays
    /// bounded without ever forgetting a key a live entry holds.
    retired: std::collections::VecDeque<[u8; 32]>,
}

/// The most spent gate keys remembered.
const MAX_SPENT_GATES: usize = 4096;

impl Registry {
    /// Remember `gate` as spent, forgetting the oldest retired keys to make
    /// room. Refused when it was already spent or nothing retired can go.
    fn spend(&mut self, gate: [u8; 32]) -> bool {
        if self.used.contains(&gate) {
            return false;
        }
        while self.used.len() >= MAX_SPENT_GATES {
            let Some(oldest) = self.retired.pop_front() else {
                return false;
            };
            if !self.entries.values().any(|entry| entry.gate == oldest) {
                self.used.remove(&oldest);
            }
        }
        self.used.insert(gate)
    }
}
pub struct Ingress {
    registry: Mutex<Registry>,
    client: reqwest::Client,
    capacity: Arc<Semaphore>,
}
impl Ingress {
    pub fn new() -> Result<Arc<Self>, IngressError> {
        Ok(Arc::new(Self {
            registry: Mutex::new(Registry::default()),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .build()
                .map_err(|_| IngressError)?,
            capacity: Arc::new(Semaphore::new(64)),
        }))
    }
    pub fn register(
        &self,
        scope: IngressScope,
        target: SocketAddr,
        served_model: String,
        gate: [u8; 32],
        native: [u8; 32],
    ) -> Result<(), IngressError> {
        // Trusted local composition supplies the native loopback destination;
        // neither an inference request nor a redirect can choose a destination.
        if !target.ip().is_loopback()
            || target.port() == 0
            || gate == [0; 32]
            || native == [0; 32]
            || gate == native
            || scope.generation <= 0
            || scope.revision <= 0
            || [
                &scope.host_id,
                &scope.deployment_id,
                &scope.binding_id,
                &scope.incarnation,
                &scope.member_id,
                &served_model,
            ]
            .iter()
            .any(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(IngressError);
        }
        let key = key(&scope);
        let mut registry = self.registry.lock().map_err(|_| IngressError)?;
        if registry.entries.values().any(|entry| entry.scope.host_id != scope.host_id) {
            return Err(IngressError);
        }
        if let Some(old) = registry.entries.get(&key) {
            if old.scope == scope
                && old.target == target
                && old.model == served_model
                && old.gate == gate
                && old.native == native
            {
                return Ok(());
            }
            if old.scope.host_id != scope.host_id
                || old.scope.generation >= scope.generation
                || old.scope.revision > scope.revision
                || old.current.load(Ordering::SeqCst) != 0
            {
                return Err(IngressError);
            }
        } else if registry.entries.len() >= 128 {
            return Err(IngressError);
        }
        if !registry.spend(gate) {
            return Err(IngressError);
        }
        if let Some(old) = registry.entries.get(&key) {
            old.open.store(false, Ordering::SeqCst);
            let replaced = old.gate;
            registry.retired.push_back(replaced);
        }
        registry.entries.insert(
            key,
            Arc::new(Entry {
                scope,
                target,
                model: served_model,
                gate,
                native,
                open: AtomicBool::new(false),
                current: AtomicUsize::new(0),
                handle: Mutex::new(None),
                timings: Mutex::new(IngressTimings::default()),
            }),
        );
        Ok(())
    }
    fn entry(&self, scope: &IngressScope) -> Result<Arc<Entry>, IngressError> {
        self.registry
            .lock()
            .map_err(|_| IngressError)?
            .entries
            .get(&key(scope))
            .filter(|entry| entry.scope == *scope)
            .cloned()
            .ok_or(IngressError)
    }
    pub fn open(&self, scope: &IngressScope) -> Result<(), IngressError> {
        self.set_open(scope,true)
    }
    pub fn close(&self, scope: &IngressScope) -> Result<(), IngressError> {
        self.set_open(scope,false)
    }
    fn set_open(&self,scope:&IngressScope,open:bool)->Result<(),IngressError> {
        // Registration and gate changes share the same lock: an old open call
        // cannot reopen a retired entry after a new generation replaces it.
        let registry=self.registry.lock().map_err(|_|IngressError)?;
        let entry=registry.entries.get(&key(scope))
            .filter(|entry|entry.scope==*scope).ok_or(IngressError)?;
        entry.open.store(open,Ordering::SeqCst);
        Ok(())
    }
    /// SPEC §§6, 13.3: a launch whose owned processes are proven gone has no
    /// native destination left. Retire its exact, closed, idle entry so the next
    /// launch of the same deployment member can register; a failed launch keeps
    /// its generation, so without this the retry is refused as stale. A
    /// different scope, an open gate or a request in flight keeps the entry.
    /// Its gate key stays spent. Returns whether the entry was retired.
    pub fn retire(&self, scope: &IngressScope) -> Result<bool, IngressError> {
        let mut registry = self.registry.lock().map_err(|_| IngressError)?;
        let key = key(scope);
        let retirable = registry.entries.get(&key).is_some_and(|entry| {
            entry.scope == *scope
                && !entry.open.load(Ordering::SeqCst)
                && entry.current.load(Ordering::SeqCst) == 0
        });
        if retirable {
            if let Some(entry) = registry.entries.remove(&key) {
                registry.retired.push_back(entry.gate);
            }
        }
        Ok(retirable)
    }
    /// SPEC §13: loss of controller session authority closes forwarding without
    /// claiming native quiescence or releasing any in-flight accounting.
    pub fn close_all(&self) -> Result<(), IngressError> {
        let registry = self.registry.lock().map_err(|_| IngressError)?;
        for entry in registry.entries.values() { entry.open.store(false, Ordering::SeqCst); }
        Ok(())
    }
    /// Bind the owned handle (launch command id) of the launch behind this
    /// exact scope. Load samples are reported only for bound, open entries.
    pub fn bind_handle(&self, scope: &IngressScope, owned_handle: &str) -> Result<(), IngressError> {
        if owned_handle.trim().is_empty() || owned_handle.len() > 4096 {
            return Err(IngressError);
        }
        let entry = self.entry(scope)?;
        *entry.handle.lock().map_err(|_| IngressError)? = Some(owned_handle.to_owned());
        Ok(())
    }
    /// SPEC §10, D9: every open, handle-bound scope with its loopback target
    /// and current forwarding count. A closed gate is not Ready and reports nothing.
    pub fn load_targets(&self) -> Result<Vec<LoadTarget>, IngressError> {
        let registry = self.registry.lock().map_err(|_| IngressError)?;
        let mut targets = Vec::new();
        for entry in registry.entries.values() {
            if !entry.open.load(Ordering::SeqCst) {
                continue;
            }
            let Some(owned_handle) = entry.handle.lock().map_err(|_| IngressError)?.clone() else {
                continue;
            };
            targets.push(LoadTarget {
                scope: entry.scope.clone(),
                owned_handle,
                target: entry.target,
                native: entry.native,
                in_flight: entry.current.load(Ordering::SeqCst),
            });
        }
        Ok(targets)
    }
    /// SPEC §17 (M80): take the timings this exact scope's forwards recorded
    /// since the last call, as `(series, histogram)` deltas with at least one
    /// observation each. Series names are from
    /// `mllm_protocol::reports::HOST_LATENCY_SERIES`.
    pub fn drain_latency(&self, scope: &IngressScope) -> Result<Vec<(String, Histogram)>, IngressError> {
        let entry = self.entry(scope)?;
        let taken = std::mem::take(&mut *entry.timings.lock().map_err(|_| IngressError)?);
        Ok([
            ("ingress_time_to_headers", taken.headers),
            ("ingress_time_to_first_byte", taken.first_byte),
            ("ingress_time_to_last_byte", taken.last_byte),
        ]
        .into_iter()
        .filter(|(_, h)| !h.is_empty())
        .map(|(name, h)| (name.to_owned(), h))
        .collect())
    }
    /// Forwarding count only: cancellation is not native quiescence evidence.
    pub fn current_requests(&self, scope: &IngressScope) -> Result<usize, IngressError> {
        Ok(self.entry(scope)?.current.load(Ordering::SeqCst))
    }
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/v1/chat/completions", post(forward))
            .fallback(|| async { failure(StatusCode::NOT_FOUND) })
            .method_not_allowed_fallback(|| async { failure(StatusCode::METHOD_NOT_ALLOWED) })
            .with_state(self)
    }
    fn authorize(&self, request: &Request) -> Result<Arc<Entry>, IngressError> {
        if request.uri().path() != "/v1/chat/completions"
            || request.uri().query().is_some()
            || request.headers().contains_key(header::CONTENT_ENCODING)
        {
            return Err(IngressError);
        }
        let mut values = request.headers().get_all(header::AUTHORIZATION).iter();
        let value = values
            .next()
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or(IngressError)?;
        if values.next().is_some() || value.len() != 64 {
            return Err(IngressError);
        }
        let bytes: [u8; 32] = hex::decode(value)
            .map_err(|_| IngressError)?
            .try_into()
            .map_err(|_| IngressError)?;
        let registry = self.registry.lock().map_err(|_| IngressError)?;
        let mut found = None;
        for entry in registry.entries.values() {
            if bool::from(entry.gate.ct_eq(&bytes)) && entry.open.load(Ordering::SeqCst) {
                found = Some(entry.clone());
            }
        }
        let entry = found.ok_or(IngressError)?;
        entry.current.fetch_add(1, Ordering::SeqCst);
        Ok(entry)
    }
}
struct InFlight {
    entry: Arc<Entry>,
    _capacity: OwnedSemaphorePermit,
}
impl Drop for InFlight {
    fn drop(&mut self) {
        self.entry.current.fetch_sub(1, Ordering::SeqCst);
    }
}
fn failure(status: StatusCode) -> Response {
    (status,[(header::CACHE_CONTROL,"no-store")],Json(serde_json::json!({"error":{"code":"inference_unavailable","message":"Inference request is not authorized or could not complete"}}))).into_response()
}
async fn forward(State(ingress): State<Arc<Ingress>>, request: Request) -> Response {
    // SPEC §17 (M80): the ingress clock starts when the request reaches it.
    let received = Instant::now();
    let Ok(capacity) = ingress.capacity.clone().try_acquire_owned() else {
        return failure(StatusCode::TOO_MANY_REQUESTS);
    };
    let Ok(entry) = ingress.authorize(&request) else {
        return failure(StatusCode::FORBIDDEN);
    };
    let guard = InFlight {
        entry: entry.clone(),
        _capacity: capacity,
    };
    let content = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    if !matches!(
        content,
        Some("application/json" | "application/json; charset=utf-8")
    ) {
        return failure(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let body = match tokio::time::timeout(
        Duration::from_secs(15),
        to_bytes(request.into_body(), 8 * 1024 * 1024),
    )
    .await
    {
        Ok(Ok(body)) => body,
        _ => return failure(StatusCode::BAD_REQUEST),
    };
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return failure(StatusCode::BAD_REQUEST),
    };
    if value.get("model").and_then(serde_json::Value::as_str) != Some(entry.model.as_str()) {
        return failure(StatusCode::BAD_REQUEST);
    }
    // SPEC §10 / T19 T21: the supported chat payload only; an engine-internal
    // field (request id, adapter path, hidden states, custom logit processor,
    // disaggregation bootstrap, KV transfer, engine extras, priority) is
    // refused before anything reaches the engine.
    if !mllm_adapters::forward::chat_request_allowed(&value) {
        return failure(StatusCode::BAD_REQUEST);
    }
    if !entry.open.load(Ordering::SeqCst) {
        return failure(StatusCode::FORBIDDEN);
    }
    // Rebuild every upstream header: no end-user credential, routing override,
    // forwarded host, native admin secret or hop-by-hop field crosses this boundary.
    // SPEC §10: only the engine's response head is bounded here. A request
    // timeout would also cover the body and cut a stream that is still
    // producing at a fixed wall time; the router bounds a relayed stream by
    // its deadline and the idle gap between events, and dropping it closes
    // this body.
    let response = match tokio::time::timeout(
        Duration::from_secs(900),
        ingress
            .client
            .post(format!("http://{}/v1/chat/completions", entry.target))
            .bearer_auth(hex::encode(entry.native))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .send(),
    )
    .await
    {
        Ok(Ok(response)) if response.status().is_success() => response,
        _ => return failure(StatusCode::BAD_GATEWAY),
    };
    let content = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !(content.starts_with("application/json") || content.starts_with("text/event-stream")) {
        return failure(StatusCode::BAD_GATEWAY);
    }
    entry.record(Mark::Headers, received);
    let builder = Response::builder()
        .status(response.status())
        .header(header::CONTENT_TYPE, content)
        .header(header::CACHE_CONTROL, "no-store");
    // SPEC §17 (M80): first and last engine body byte, on the ingress clock. A
    // stream that fails or is dropped before its end records no last byte.
    let stream = futures::stream::unfold(
        (response.bytes_stream(), guard, false),
        move |(mut stream, guard, mut seen)| async move {
            let item = stream.next().await;
            match &item {
                Some(Ok(_)) if !seen => {
                    seen = true;
                    guard.entry.record(Mark::FirstByte, received);
                }
                None => guard.entry.record(Mark::LastByte, received),
                _ => {}
            }
            item.map(|item| (item, (stream, guard, seen)))
        },
    );
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| failure(StatusCode::BAD_GATEWAY))
}

//! Bounded durable replay; event notifications are not required for continuity.
use crate::{AppState, StoreSnapshotSource, error};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response, Sse, sse::Event},
};
use mllm_store::events::{EventPage, EventReadError, ManagementEvent};
use serde_json::{Map, Value};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::mpsc;

/// Providers perform bounded durable reads only. Store replay may prune event
/// retention transactionally, but must not modify lifecycle state.
pub trait EventSource: Send + Sync + 'static {
    fn events_after(&self, after: Option<&str>, limit: usize) -> Result<EventPage, EventReadError>;
}
impl EventSource for StoreSnapshotSource {
    fn events_after(&self, after: Option<&str>, limit: usize) -> Result<EventPage, EventReadError> {
        self.store
            .lock()
            .map_err(|_| EventReadError::InvalidLimit)?
            .events_after(after, limit)
    }
}

/// Upper bounds are enforced at router construction. A finite lifetime forces
/// fresh authentication on reconnect; fixed credentials do not support revocation.
#[derive(Clone)]
pub struct EventStreamOptions {
    pub max_streams: usize,
    pub page_size: usize,
    pub page_bytes: usize,
    pub channel_capacity: usize,
    pub poll_interval: Duration,
    pub heartbeat_interval: Duration,
    pub send_timeout: Duration,
    pub lifetime: Duration,
}
impl Default for EventStreamOptions {
    fn default() -> Self {
        Self {
            max_streams: 16,
            page_size: 64,
            page_bytes: 256 * 1024,
            channel_capacity: 8,
            poll_interval: Duration::from_millis(250),
            heartbeat_interval: Duration::from_secs(15),
            send_timeout: Duration::from_secs(5),
            lifetime: Duration::from_secs(300),
        }
    }
}
impl EventStreamOptions {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.max_streams == 0
            || self.max_streams > 16
            || self.page_size == 0
            || self.page_size > 64
            || self.page_bytes == 0
            || self.page_bytes > 256 * 1024
            || self.channel_capacity == 0
            || self.channel_capacity > 8
            || self.poll_interval.is_zero()
            || self.poll_interval > Duration::from_secs(15)
            || self.heartbeat_interval.is_zero()
            || self.heartbeat_interval > Duration::from_secs(60)
            || self.send_timeout.is_zero()
            || self.send_timeout > Duration::from_secs(5)
            || self.lifetime.is_zero()
            || self.lifetime > Duration::from_secs(300)
        {
            return Err("invalid event stream limits");
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Failure {
    Invalid,
    Expired,
    Internal,
    Busy,
}
impl Failure {
    fn code(self) -> &'static str {
        match self {
            Self::Invalid => "invalid_cursor",
            Self::Expired => "cursor_expired",
            Self::Internal => "internal",
            Self::Busy => "queue_full",
        }
    }
    fn response(self) -> Response {
        error(
            match self {
                Self::Invalid => StatusCode::BAD_REQUEST,
                Self::Expired => StatusCode::GONE,
                Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
                Self::Busy => StatusCode::TOO_MANY_REQUESTS,
            },
            self.code(),
            matches!(self, Self::Busy),
        )
    }
    fn terminal(self) -> Event {
        Event::default().event("management_stream_error").data(serde_json::json!({"api_version":"1","error":{"code":self.code(),"resnapshot_required":matches!(self, Self::Expired),"retryable":matches!(self, Self::Busy)}}).to_string())
    }
}
impl From<EventReadError> for Failure {
    fn from(value: EventReadError) -> Self {
        match value {
            EventReadError::MalformedCursor | EventReadError::FutureCursor => Self::Invalid,
            EventReadError::ExpiredCursor | EventReadError::WrongIncarnation => Self::Expired,
            _ => Self::Internal,
        }
    }
}

fn cursor(value: &str) -> Result<(String, i64), Failure> {
    if value.len() > 46 {
        return Err(Failure::Invalid);
    }
    let (incarnation, sequence) = value.split_once(':').ok_or(Failure::Invalid)?;
    if incarnation.len() != 26
        || incarnation.parse::<ulid::Ulid>().is_err()
        || sequence.is_empty()
        || !sequence.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(Failure::Invalid);
    }
    let sequence = sequence.parse::<i64>().map_err(|_| Failure::Invalid)?;
    Ok((incarnation.to_owned(), sequence))
}

// Decode percent escapes strictly; '+' is not part of the cursor grammar.
fn decode(value: &str) -> Result<String, Failure> {
    if value.len() > 138 {
        return Err(Failure::Invalid);
    }
    let mut bytes = Vec::with_capacity(value.len());
    let mut input = value.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let high = input
                .next()
                .and_then(|v| (v as char).to_digit(16))
                .ok_or(Failure::Invalid)?;
            let low = input
                .next()
                .and_then(|v| (v as char).to_digit(16))
                .ok_or(Failure::Invalid)?;
            bytes.push((high * 16 + low) as u8);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).map_err(|_| Failure::Invalid)
}
fn requested_cursor(request: &Request) -> Result<Option<String>, Failure> {
    let query = match request.uri().query() {
        None => None,
        Some(query) => {
            let (key, value) = query.split_once('=').ok_or(Failure::Invalid)?;
            if query.contains('&') || decode(key)? != "after" {
                return Err(Failure::Invalid);
            }
            Some(decode(value)?)
        }
    };
    let mut headers = request.headers().get_all("last-event-id").iter();
    let header = headers
        .next()
        .map(|h| h.to_str().map(str::to_owned).map_err(|_| Failure::Invalid))
        .transpose()?;
    if headers.next().is_some() {
        return Err(Failure::Invalid);
    }
    if let (Some(query), Some(header)) = (&query, &header) {
        if query != header {
            return Err(Failure::Invalid);
        }
    }
    let result = query.or(header);
    if let Some(value) = &result {
        cursor(value)?;
    }
    Ok(result)
}

struct Page {
    events: Vec<(String, Event)>,
    empty_cursor: String,
}
async fn read(state: &Arc<AppState>, after: Option<String>) -> Result<Page, Failure> {
    let permit = state
        .reads
        .clone()
        .try_acquire_owned()
        .map_err(|_| Failure::Busy)?;
    let source = state.events.clone().ok_or(Failure::Internal)?;
    let options = state.event_options.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let page = source
            .events_after(after.as_deref(), options.page_size)
            .map_err(Failure::from)?;
        project_page(page, after.as_deref(), &options)
    })
    .await
    .map_err(|_| Failure::Internal)?
}

pub(crate) async fn subscribe(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let after = match requested_cursor(&request) {
        Ok(v) => v,
        Err(e) => return e.response(),
    };
    let stream_permit = match state.streams.clone().try_acquire_owned() {
        Ok(v) => v,
        Err(_) => return Failure::Busy.response(),
    };
    let options = state.event_options.clone();
    let deadline = tokio::time::Instant::now() + options.lifetime;
    let first = match tokio::time::timeout_at(deadline, read(&state, after)).await {
        Ok(Ok(page)) => page,
        Ok(Err(e)) => return e.response(),
        Err(_) => return Failure::Busy.response(),
    };
    let (tx, rx) = mpsc::channel(options.channel_capacity);
    tokio::spawn(async move {
        // Client drop wakes closed(); expiry/slow sends also free stream capacity.
        // Blocking workers independently retain read capacity until completion.
        let _stream_permit = stream_permit;
        let work = follow(state, first, &tx);
        tokio::select! {
            _ = tx.closed() => {},
            _ = tokio::time::sleep_until(deadline) => {},
            _ = work => {},
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|event| (Ok::<_, Infallible>(event), rx))
    });
    Sse::new(stream).into_response()
}

async fn send(tx: &mpsc::Sender<Event>, event: Event, timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, tx.send(event)).await,
        Ok(Ok(()))
    )
}
async fn follow(state: Arc<AppState>, mut page: Page, tx: &mpsc::Sender<Event>) {
    let options = &state.event_options;
    let mut heartbeat = tokio::time::Instant::now() + options.heartbeat_interval;
    loop {
        let was_empty = page.events.is_empty();
        let mut after = page.empty_cursor;
        for (id, event) in page.events {
            if !send(tx, event, options.send_timeout).await {
                return;
            }
            after = id;
        }
        if was_empty {
            tokio::time::sleep(options.poll_interval).await;
            if tokio::time::Instant::now() >= heartbeat {
                if !send(
                    tx,
                    Event::default().comment("heartbeat"),
                    options.send_timeout,
                )
                .await
                {
                    return;
                }
                heartbeat = tokio::time::Instant::now() + options.heartbeat_interval;
            }
        }
        match read(&state, Some(after)).await {
            Ok(next) => page = next,
            Err(error) => {
                send(tx, error.terminal(), options.send_timeout).await;
                return;
            }
        }
    }
}

fn project_page(
    page: EventPage,
    after: Option<&str>,
    options: &EventStreamOptions,
) -> Result<Page, Failure> {
    if page.events.len() > options.page_size {
        return Err(Failure::Internal);
    }
    let high = page.high_water.to_string();
    cursor(&high).map_err(|_| Failure::Internal)?;
    let mut previous = match after {
        Some(after) => {
            let (incarnation, sequence) = cursor(after)?;
            if incarnation != page.high_water.incarnation || sequence > page.high_water.sequence {
                return Err(Failure::Internal);
            }
            sequence
        }
        None => 0,
    };
    let mut bytes = 0usize;
    let mut events = Vec::with_capacity(page.events.len());
    for event in page.events {
        if event.cursor.incarnation != page.high_water.incarnation
            || event.cursor.sequence <= previous
            || event.cursor.sequence > page.high_water.sequence
        {
            return Err(Failure::Internal);
        }
        previous = event.cursor.sequence;
        let data = project(&event)?;
        let id = event.cursor.to_string();
        bytes = bytes
            .checked_add(data.len() + id.len() + event.kind.len() + 32)
            .ok_or(Failure::Internal)?;
        if bytes > options.page_bytes {
            return Err(Failure::Internal);
        }
        events.push((
            id.clone(),
            Event::default().id(id).event(event.kind).data(data),
        ));
    }
    Ok(Page {
        events,
        empty_cursor: high,
    })
}

// Value alone accepts repeated keys by overwriting earlier values. Reject those
// corrupt records explicitly before applying the closed, scalar field schema.
fn unique_object(input: &str) -> Result<Map<String, Value>, Failure> {
    struct Object;
    impl<'de> serde::de::Visitor<'de> for Object {
        type Value = Map<String, Value>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("object with unique fields")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut access: A,
        ) -> Result<Self::Value, A::Error> {
            let mut result = Map::new();
            while let Some((key, value)) = access.next_entry::<String, Value>()? {
                if result.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate field"));
                }
            }
            Ok(result)
        }
    }
    let mut decoder = serde_json::Deserializer::from_str(input);
    let object = serde::Deserializer::deserialize_map(&mut decoder, Object)
        .map_err(|_| Failure::Internal)?;
    decoder.end().map_err(|_| Failure::Internal)?;
    Ok(object)
}
fn project(event: &ManagementEvent) -> Result<String, Failure> {
    if event.payload_json.len() > 16 * 1024 || event.recorded_at_ms < 0 {
        return Err(Failure::Internal);
    }
    let input = unique_object(&event.payload_json)?;
    if input.get("version").and_then(Value::as_str) != Some("1") {
        return Err(Failure::Internal);
    }
    let fields: &[&str] = match event.kind.as_str() {
        "coordinator_session_started" => &["session_epoch"],
        "managed_configuration_accepted" => &[
            "operation_id",
            "deployment_id",
            "revision",
            "generation",
            "session_epoch",
        ],
        "candidate_initialize_armed" | "candidate_initialize_accepted" => &[
            "operation_id",
            "deployment_id",
            "run_id",
            "step_id",
            "revision",
            "generation",
            "session_epoch",
        ],
        "candidate_run_accepted" => &[
            "operation_id",
            "deployment_id",
            "run_id",
            "revision",
            "generation",
            "resource_policy_revision",
            "qualification_policy_revision",
            "session_epoch",
        ],
        "host_resource_policy_bootstrapped" => &["revision", "ledger_epoch", "session_epoch"],
        "host_resource_policy_updated" => &[
            "operation_id",
            "previous_revision",
            "current_revision",
            "ledger_epoch",
            "session_epoch",
        ],
        "host_qualification_policy_changed" => &[
            "change_kind",
            "previous_revision",
            "current_revision",
            "session_epoch",
        ],
        "candidate_qualification_finished"
        | "candidate_owned_launch_associated"
        | "candidate_ready_completed"
        | "candidate_park_completed"
        | "candidate_cleanup_accepted"
        | "candidate_cleanup_armed"
        | "candidate_cleanup_completed"
        | "qualified_initialize_accepted"
        | "qualified_initialize_armed"
        | "qualified_owned_launch_associated"
        | "qualified_ready_committed"
        | "qualified_initialize_uncertain"
        | "qualified_initialize_expired_unarmed"
        | "ordinary_cleanup_accepted"
        | "ordinary_cleanup_armed"
        | "ordinary_cleanup_completed" => &[
            "transition",
            "operation_id",
            "deployment_id",
            "step_id",
            "session_epoch",
            "committed_epoch",
        ],
        _ => return Err(Failure::Internal),
    };
    if input.len() != fields.len() + 1 {
        return Err(Failure::Internal);
    }
    let qualified_transition = match event.kind.as_str() {
        "qualified_initialize_accepted" => Some("accepted"),
        "qualified_initialize_armed" => Some("armed"),
        "qualified_owned_launch_associated" => Some("owned_launch_associated"),
        "qualified_ready_committed" => Some("ready"),
        "qualified_initialize_uncertain" => Some("uncertain"),
        "qualified_initialize_expired_unarmed" => Some("expired_unarmed"),
        "ordinary_cleanup_accepted" => Some("cleanup_accepted"),
        "ordinary_cleanup_armed" => Some("cleanup_armed"),
        "ordinary_cleanup_completed" => Some("cleanup_completed"),
        _ => None,
    };
    if let Some(transition) = qualified_transition {
        let epoch = input.get("committed_epoch").ok_or(Failure::Internal)?;
        if matches!(transition, "ready" | "cleanup_completed") == epoch.is_null()
            || epoch.as_u64() == Some(0)
        {
            return Err(Failure::Internal);
        }
    }
    let mut payload = Map::new();
    for &field in fields {
        let value = input.get(field).ok_or(Failure::Internal)?;
        let projected = if field.ends_with("_id") {
            let id = value.as_str().ok_or(Failure::Internal)?;
            if id.len() != 26 || id.parse::<ulid::Ulid>().is_err() {
                return Err(Failure::Internal);
            }
            value.clone()
        } else if field == "transition" {
            if value.as_str()
                != qualified_transition.or_else(|| event.kind.strip_prefix("candidate_"))
            {
                return Err(Failure::Internal);
            }
            value.clone()
        } else if field == "change_kind" {
            if !matches!(
                value.as_str(),
                Some("imported" | "updated" | "removed" | "readded")
            ) {
                return Err(Failure::Internal);
            }
            value.clone()
        } else if value.is_null()
            && (field == "committed_epoch"
                || (field == "previous_revision"
                    && event.kind == "host_qualification_policy_changed"))
        {
            Value::Null
        } else {
            let number = value.as_u64().ok_or(Failure::Internal)?;
            if !matches!(field, "committed_epoch" | "ledger_epoch") && number > i64::MAX as u64 {
                return Err(Failure::Internal);
            }
            Value::String(number.to_string())
        };
        payload.insert(field.to_owned(), projected);
    }
    for (field, column) in [
        ("deployment_id", &event.deployment_id),
        ("operation_id", &event.operation_id),
    ] {
        if input.get(field).and_then(Value::as_str) != column.as_deref() {
            return Err(Failure::Internal);
        }
    }
    Ok(serde_json::json!({"api_version":"1", "recorded_at_ms":event.recorded_at_ms.to_string(), "deployment_id":event.deployment_id, "operation_id":event.operation_id, "payload":payload}).to_string())
}

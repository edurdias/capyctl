//! Bounded durable replay; event notifications are not required for continuity.
use crate::{error, AppState, StoreSnapshotSource};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    response::{sse::Event, IntoResponse, Response, Sse},
};
use capyctl_store::events::{EventPage, EventReadError, ManagementEvent};
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
/// How one projected payload field is validated and rendered.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    /// A ULID identifier, rendered as is.
    Id,
    /// A non-negative integer, rendered as a decimal string.
    Number,
    /// `committed_epoch`: null or a positive integer, per the transition.
    Epoch,
    /// The step transition, which must match the event kind.
    Transition,
    /// The switch phase, which must match the event kind.
    Phase,
    /// A bounded token (host name, profile name, digest, victim).
    Token,
    /// A bounded token or null.
    OptionalToken,
    /// A bounded list of tokens.
    Tokens,
    /// Bounded free text.
    Text,
}

const STEP_FIELDS: &[(&str, Field)] = &[
    ("transition", Field::Transition),
    ("operation_id", Field::Id),
    ("deployment_id", Field::Id),
    ("step_id", Field::Id),
    ("session_epoch", Field::Number),
    ("committed_epoch", Field::Epoch),
];

/// The transition a step kind carries, and whether it commits an epoch.
fn step_transition(kind: &str) -> Option<(&'static str, bool)> {
    Some(match kind {
        "initialize_accepted" => ("accepted", false),
        "initialize_armed" => ("armed", false),
        "owned_launch_associated" => ("owned_launch_associated", false),
        "ready_committed" => ("ready", true),
        "initialize_uncertain" => ("uncertain", false),
        "initialize_expired_unarmed" => ("expired_unarmed", false),
        // SPEC §6: a failed launch released against gone evidence (G1 remote too).
        "initialize_failed_released" => ("launch_failed", true),
        "ordinary_cleanup_accepted" => ("cleanup_accepted", false),
        "ordinary_cleanup_armed" => ("cleanup_armed", false),
        "ordinary_cleanup_completed" => ("cleanup_completed", true),
        "ordinary_cleanup_expired_unarmed" => ("cleanup_expired_unarmed", false),
        "ordinary_unarmed_stop_accepted" => ("unarmed_stop_accepted", false),
        "ordinary_unarmed_stop_completed" => ("unarmed_stop_completed", false),
        // SPEC §§6.1, 6.3, 9.1 (W5): park and restore steps. A completion or a
        // refusal commits the ledger epoch; every other stage commits nothing.
        "park_accepted" => ("park_accepted", false),
        "park_armed" => ("park_armed", false),
        "parked_committed" => ("parked", true),
        "park_refused" => ("park_refused", true),
        "park_uncertain" => ("park_uncertain", false),
        "restore_accepted" => ("restore_accepted", false),
        "restore_armed" => ("restore_armed", false),
        "restored_committed" => ("restored", true),
        "restore_refused" => ("restore_refused", true),
        "restore_uncertain" => ("restore_uncertain", false),
        "residency_cancelled" => ("cancelled", false),
        _ => return None,
    })
}

fn token(value: &Value) -> Result<Value, Failure> {
    let text = value.as_str().ok_or(Failure::Internal)?;
    if text.is_empty()
        || text.len() > 256
        || !text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
    {
        return Err(Failure::Internal);
    }
    Ok(value.clone())
}

// SPEC §14: events are a required management operation. Every kind the store
// journals (`capyctl_store::events::EventMetadata`) has a closed field schema
// here; a kind without one is corruption, and the stream reports it as such.
fn project(event: &ManagementEvent) -> Result<String, Failure> {
    if event.payload_json.len() > 16 * 1024 || event.recorded_at_ms < 0 {
        return Err(Failure::Internal);
    }
    let input = unique_object(&event.payload_json)?;
    if input.get("version").and_then(Value::as_str) != Some("1") {
        return Err(Failure::Internal);
    }
    use Field::{Id, Number, OptionalToken, Phase, Text, Token, Tokens};
    let step = step_transition(event.kind.as_str());
    let switch_phase = event.kind.strip_prefix("switch_").filter(|phase| {
        matches!(
            *phase,
            "planned" | "admission_closed" | "released" | "completed" | "failed"
        )
    });
    let fields: &[(&str, Field)] = match event.kind.as_str() {
        _ if step.is_some() => STEP_FIELDS,
        _ if switch_phase.is_some() => &[
            ("phase", Phase),
            ("switch_id", Token),
            ("target_deployment", Id),
            ("host", OptionalToken),
            ("victims", Tokens),
            ("detail", Text),
        ],
        "coordinator_session_started" => &[("session_epoch", Number)],
        "managed_configuration_accepted" => &[
            ("operation_id", Id),
            ("deployment_id", Id),
            ("revision", Number),
            ("generation", Number),
            ("session_epoch", Number),
        ],
        // SPEC §6.3 (W6): routes and instances removed after verified cleanup.
        "deployment_deleted" => &[
            ("operation_id", Id),
            ("deployment_id", Id),
            ("revision", Number),
            ("session_epoch", Number),
        ],
        "host_resource_policy_bootstrapped" => &[
            ("revision", Number),
            ("ledger_epoch", Number),
            ("session_epoch", Number),
        ],
        "host_resource_policy_updated" => &[
            ("operation_id", Id),
            ("previous_revision", Number),
            ("current_revision", Number),
            ("ledger_epoch", Number),
            ("session_epoch", Number),
        ],
        // ADR 0019: a generated policy replaced for a changed machine shape.
        "host_resource_policy_migrated" => &[
            ("previous_revision", Number),
            ("current_revision", Number),
            ("ledger_epoch", Number),
            ("session_epoch", Number),
        ],
        // ADR 0008: drift evidence a host reported; bounded tokens only.
        "installation_drift_flagged" => &[
            ("host_id", Token),
            ("installation", Token),
            ("registered_digest", Token),
            ("observed_digest", Token),
        ],
        // SPEC §§4.1, 13.3: an administrator revoked a host identity.
        "host_revoked" => &[("host_id", Token), ("host_name", Token)],
        // ADR 0016: a revoked host was invited to, and did, re-enroll under
        // its same identity.
        "host_recovery_invited" => &[
            ("host_id", Token),
            ("host_name", Token),
            ("expires_unix", Number),
        ],
        "host_recovered" => &[("host_id", Token), ("host_name", Token)],
        // SPEC §4.3: an abandoned drain intent expired after its deadline.
        "host_drain_intent_expired" => &[
            ("host_id", Token),
            ("drain_key", Token),
            ("deadline_ms", Number),
        ],
        _ => return Err(Failure::Internal),
    };
    if input.len() != fields.len() + 1 {
        return Err(Failure::Internal);
    }
    let mut payload = Map::new();
    for &(field, kind) in fields {
        let value = input.get(field).ok_or(Failure::Internal)?;
        let projected = match kind {
            Field::Id => {
                let id = value.as_str().ok_or(Failure::Internal)?;
                if id.len() != 26 || id.parse::<ulid::Ulid>().is_err() {
                    return Err(Failure::Internal);
                }
                value.clone()
            }
            Field::Transition => {
                if value.as_str() != step.map(|(transition, _)| transition) {
                    return Err(Failure::Internal);
                }
                value.clone()
            }
            Field::Phase => {
                if value.as_str() != switch_phase {
                    return Err(Failure::Internal);
                }
                value.clone()
            }
            Field::Epoch => {
                let committed = step.is_some_and(|(_, committed)| committed);
                if committed == value.is_null() || value.as_u64() == Some(0) {
                    return Err(Failure::Internal);
                }
                if value.is_null() {
                    Value::Null
                } else {
                    Value::String(value.as_u64().ok_or(Failure::Internal)?.to_string())
                }
            }
            Field::Number => {
                let number = value.as_u64().ok_or(Failure::Internal)?;
                if field != "ledger_epoch" && number > i64::MAX as u64 {
                    return Err(Failure::Internal);
                }
                Value::String(number.to_string())
            }
            Field::Token => token(value)?,
            Field::OptionalToken if value.is_null() => Value::Null,
            Field::OptionalToken => token(value)?,
            Field::Tokens => {
                let items = value.as_array().ok_or(Failure::Internal)?;
                if items.len() > 256 {
                    return Err(Failure::Internal);
                }
                Value::Array(items.iter().map(token).collect::<Result<_, _>>()?)
            }
            Field::Text => {
                let text = value.as_str().ok_or(Failure::Internal)?;
                if text.chars().count() > 512 {
                    return Err(Failure::Internal);
                }
                value.clone()
            }
        };
        payload.insert(field.to_owned(), projected);
    }
    // The indexed columns must agree with the payload they were written from.
    let deployment_field = if switch_phase.is_some() {
        "target_deployment"
    } else {
        "deployment_id"
    };
    for (field, column) in [
        (deployment_field, &event.deployment_id),
        ("operation_id", &event.operation_id),
    ] {
        if input.get(field).and_then(Value::as_str) != column.as_deref() {
            return Err(Failure::Internal);
        }
    }
    Ok(serde_json::json!({"api_version":"1", "recorded_at_ms":event.recorded_at_ms.to_string(), "deployment_id":event.deployment_id, "operation_id":event.operation_id, "payload":payload}).to_string())
}

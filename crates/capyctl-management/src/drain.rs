//! SPEC §4.3: explicit draining of a host, distinct from an ordinary restart.
//!
//! Owner decision P3 (2026-09-22): signalling a role restarts it and leaves its
//! engines running. Terminating a host's engines is this separate, authenticated
//! operator action: every deployment holding a runtime on the named host is
//! stopped through the ordinary Stop, whose cleanup completes only on evidence
//! that the recorded processes are gone, and each deployment stays eligible for
//! on-demand activation (SPEC §6.3: this is not an administrative stop).
//!
//! `POST /management/v1/hosts/{host}/drain` names an enrolled host by id or
//! name; a standalone role also answers for its embedded host, by its published
//! name or by the alias `standalone`. The
//! request carries an `idempotency-key`, so a retried drain returns the same
//! operations instead of issuing new Stops.
//!
//! Owner decision 4 (2026-09-22): a drain of an offline host is accepted at
//! once. Its Stops are durable and wait, unarmed, for the host to reconnect;
//! they then complete through the ordinary cleanup path on the host's gone
//! evidence. The answer says `host_state: offline` and `stops: pending`. Every
//! drain of an enrolled host that issued a Stop records a durable marker, and
//! the host is not a placement candidate while any of those Stops is unsettled
//! (`Store::host_drain_pending`).
use crate::ManagementCredentials;
use crate::{actions::OwnedActionSource, configuration::ConfigurationFailure, error};
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Json, Router,
};
use capyctl_store::host_drain::{DrainCandidate, DrainHost};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};

const MAX_BODY: usize = 4096;
const BODY_TIMEOUT: Duration = Duration::from_secs(5);

struct DrainState {
    credentials: ManagementCredentials,
    source: Arc<OwnedActionSource>,
    /// The names this router answers for its embedded host; empty on a server,
    /// whose hosts are all enrolled remote hosts.
    embedded_host: Vec<String>,
    /// Whether an enrolled host is connected now.
    presence: HostPresence,
    commands_in_flight: Arc<tokio::sync::Semaphore>,
}

/// Owner decision 4: whether an enrolled host, by id, has a live reconciled
/// session now. Presence only shapes the answer; it never decides a Stop.
pub type HostPresence = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DrainCommand {
    deadline_ms: i64,
}

/// The drain router of a role whose hosts are all reachable in process (the
/// standalone role's embedded host).
pub fn drain_router(
    credentials: ManagementCredentials,
    source: Arc<OwnedActionSource>,
    embedded_host: Vec<String>,
) -> Router {
    drain_router_with_presence(credentials, source, embedded_host, Arc::new(|_: &str| true))
}

/// The drain router of a server, which learns whether each enrolled host is
/// connected from `presence`.
pub fn drain_router_with_presence(
    credentials: ManagementCredentials,
    source: Arc<OwnedActionSource>,
    embedded_host: Vec<String>,
    presence: HostPresence,
) -> Router {
    let state = Arc::new(DrainState {
        credentials,
        source,
        embedded_host,
        presence,
        commands_in_flight: Arc::new(tokio::sync::Semaphore::new(1)),
    });
    Router::new()
        .route(
            "/management/v1/hosts/{host}/drain",
            axum::routing::post(drain),
        )
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

async fn authenticate(
    State(state): State<Arc<DrainState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = if !state.credentials.accepts(&request) {
        error(StatusCode::UNAUTHORIZED, "unauthenticated", false)
    } else if request.uri().query().is_some() {
        error(StatusCode::BAD_REQUEST, "invalid_request", false)
    } else {
        next.run(request).await
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
}

fn host_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"api_version":"1","error":{"code":"host_not_found","message":"Host not found","retryable":false,"operation_id":null,"details":{}}})),
    )
        .into_response()
}

async fn drain(State(state): State<Arc<DrainState>>, request: Request) -> Response {
    match drain_inner(state, request).await {
        Ok(response) => response,
        Err(failure) => failure.response(),
    }
}

async fn drain_inner(
    state: Arc<DrainState>,
    request: Request,
) -> Result<Response, ConfigurationFailure> {
    use ConfigurationFailure::*;
    let host = request
        .uri()
        .path()
        .strip_prefix("/management/v1/hosts/")
        .and_then(|rest| rest.strip_suffix("/drain"))
        .filter(|host| valid_key(host))
        .ok_or(InvalidRequest)?
        .to_owned();
    let key = request
        .headers()
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|key| valid_key(key))
        .ok_or(InvalidRequest)?
        .to_owned();
    let json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !json {
        return Err(InvalidRequest);
    }
    let body = tokio::time::timeout(
        BODY_TIMEOUT,
        axum::body::to_bytes(request.into_body(), MAX_BODY),
    )
    .await
    .map_err(|_| InvalidRequest)?
    .map_err(|_| BodyTooLarge)?;
    let command: DrainCommand = serde_json::from_slice(&body).map_err(|_| InvalidRequest)?;
    if command.deadline_ms < 1 {
        return Err(InvalidRequest);
    }
    let permit = state
        .commands_in_flight
        .clone()
        .try_acquire_owned()
        .map_err(|_| QueueFull)?;
    let worker = state.clone();
    let answer = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        worker.drain_blocking(&host, &key, command.deadline_ms)
    })
    .await
    .map_err(|_| Internal)??;
    Ok(answer)
}

impl DrainState {
    fn drain_blocking(
        &self,
        host: &str,
        key: &str,
        deadline_ms: i64,
    ) -> Result<Response, ConfigurationFailure> {
        let commands = self.source.commands();
        let enrolled = commands
            .read(|store| store.enrolled_hosts())
            .map_err(|_| ConfigurationFailure::ReconciliationRequired)?;
        let (target, host_id) = match enrolled
            .iter()
            .find(|enrolled| enrolled.host_id == host || enrolled.host_name == host)
        {
            Some(enrolled) => (
                DrainHost::Remote(enrolled.host_id.as_str()),
                enrolled.host_id.clone(),
            ),
            None if self.embedded_host.iter().any(|name| name == host) => {
                (DrainHost::Embedded, host.to_owned())
            }
            None => return Ok(host_not_found()),
        };
        let online = match target {
            DrainHost::Remote(id) => (self.presence)(id),
            DrainHost::Embedded => true,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        // Router review item 14: the first enumeration of an enrolled host
        // writes the drain intent in the same transaction, so the host is out
        // of placement before any Stop is issued. Later enumerations are the
        // safety net for an instance placed before the intent committed.
        let mut opened = false;
        let rounds = drain_rounds(
            &mut || {
                let first = !std::mem::replace(&mut opened, true);
                commands
                    .read(|store| match target {
                        // SPEC §4.3: the intent carries the drain's deadline,
                        // so an abandoned one expires instead of holding the
                        // host forever.
                        DrainHost::Remote(id) if first => {
                            store.begin_host_drain_until(id, key, now, Some(deadline_ms))
                        }
                        _ => store.drain_candidates(target),
                    })
                    .map_err(|_| ConfigurationFailure::ReconciliationRequired)
            },
            // One key per deployment under the drain's own key: a retried drain
            // replays each Stop's receipt instead of issuing another.
            &mut |candidate| {
                self.source.drain_stop(
                    &candidate.deployment_id,
                    candidate.instance,
                    candidate.revision,
                    &format!("drain:{key}:{}", candidate.deployment_id),
                    deadline_ms,
                )
            },
            // Owner decision 4: the durable drain marker. While any of these
            // Stops is unsettled the host takes no new placements, even after
            // it reconnects. A marker that cannot be recorded fails the answer,
            // so the operator retries (the same key replays the same Stops) and
            // the intent keeps the host held meanwhile.
            &mut |issued| match target {
                DrainHost::Remote(id) => commands
                    .read(|store| store.record_host_drain(id, issued, now))
                    .map_err(|_| ConfigurationFailure::ReconciliationRequired),
                DrainHost::Embedded => Ok(()),
            },
        )?;
        // Router review item 14: every Stop is issued and marked, so the intent
        // is complete; the markers hold the host until the Stops settle. A
        // completion that cannot be recorded fails the answer and leaves the
        // host held (fail closed) until a retry completes it.
        if let DrainHost::Remote(id) = target {
            commands
                .read(|store| store.complete_host_drain(id, key, now))
                .map_err(|_| ConfigurationFailure::ReconciliationRequired)?;
        }
        let (operations, refused, issued) = (rounds.operations, rounds.refused, rounds.issued);
        Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "api_version": "1",
                "host": host_id,
                "host_state": if online { "online" } else { "offline" },
                // Owner decision 4: an offline host's Stops wait for it to
                // reconnect and complete then, on its gone evidence.
                "stops": if !online && !issued.is_empty() { "pending" } else { "issued" },
                "operations": operations,
                "refused": refused,
            })),
        )
            .into_response())
    }
}

/// What a drain issued, across its rounds.
pub(crate) struct Rounds {
    pub(crate) operations: Vec<serde_json::Value>,
    pub(crate) refused: Vec<serde_json::Value>,
    pub(crate) issued: Vec<String>,
}

/// The most enumeration rounds one drain runs. A remote host takes no new
/// placements from the moment the first enumeration commits its drain intent,
/// so a later round is a safety net that should find nothing new.
const MAX_ROUNDS: usize = 4;

pub(crate) type Enumerate<'a> =
    dyn FnMut() -> Result<Vec<DrainCandidate>, ConfigurationFailure> + 'a;
pub(crate) type StopOne<'a> = dyn FnMut(&DrainCandidate) -> Result<Option<crate::actions::ActionReceipt>, ConfigurationFailure>
    + 'a;
pub(crate) type Mark<'a> = dyn FnMut(&[String]) -> Result<(), ConfigurationFailure> + 'a;

/// SPEC §4.3, owner decision 4: stop every instance holding a runtime on the
/// host, recording the durable marker as soon as a Stop exists, then enumerate
/// again. The first `enumerate` of an enrolled host also writes the drain
/// intent (router review item 14), so the host is held before the first Stop;
/// the repeated enumeration stays as a safety net. Each later round stops only
/// instances no earlier round named, and the marker is extended with their
/// Stops.
pub(crate) fn drain_rounds(
    enumerate: &mut Enumerate<'_>,
    stop: &mut StopOne<'_>,
    mark: &mut Mark<'_>,
) -> Result<Rounds, ConfigurationFailure> {
    let mut rounds = Rounds {
        operations: Vec::new(),
        refused: Vec::new(),
        issued: Vec::new(),
    };
    let mut named = std::collections::BTreeSet::new();
    for _ in 0..MAX_ROUNDS {
        let fresh: Vec<DrainCandidate> = enumerate()?
            .into_iter()
            .filter(|c| named.insert((c.deployment_id.clone(), c.instance)))
            .collect();
        if fresh.is_empty() {
            break;
        }
        let before = rounds.issued.len();
        for candidate in fresh {
            match stop(&candidate) {
                Ok(Some(receipt)) => {
                    rounds.issued.push(receipt.operation_id.clone());
                    rounds.operations.push(serde_json::json!({
                        "deployment_id": receipt.deployment_id,
                        "instance": candidate.instance,
                        "operation_id": receipt.operation_id,
                        "revision": receipt.revision.to_string(),
                    }));
                }
                // The instance released its runtime since it was named.
                Ok(None) => {}
                Err(failure) => rounds.refused.push(serde_json::json!({
                    "deployment_id": candidate.deployment_id,
                    "reason": format!("{failure:?}"),
                })),
            }
        }
        if rounds.issued.len() > before {
            mark(&rounds.issued)?;
        }
    }
    Ok(rounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ActionReceipt;

    fn candidate(deployment: &str, instance: u32) -> DrainCandidate {
        DrainCandidate {
            deployment_id: deployment.into(),
            revision: 1,
            instance,
        }
    }

    /// SPEC §4.3 (owner decision 4): an instance placed on the host while the
    /// first round's Stops were issued, before the marker held the host, is
    /// found by the next enumeration and stopped too; the marker is recorded
    /// after the first Stop and extended with the late one. Nothing is stopped
    /// twice.
    // T30 T32
    #[test]
    fn an_instance_placed_before_the_marker_is_drained_by_the_next_round() {
        let mut listings = vec![
            vec![candidate("a", 0)],
            vec![candidate("a", 0), candidate("b", 0)],
            vec![candidate("a", 0), candidate("b", 0)],
        ]
        .into_iter();
        let mut stopped = Vec::new();
        let mut marks: Vec<Vec<String>> = Vec::new();
        let rounds = drain_rounds(
            &mut || Ok(listings.next().unwrap_or_default()),
            &mut |c| {
                stopped.push(c.deployment_id.clone());
                Ok(Some(ActionReceipt {
                    operation_id: format!("op-{}", c.deployment_id),
                    deployment_id: c.deployment_id.clone(),
                    revision: 1,
                    joined: false,
                }))
            },
            &mut |issued| {
                marks.push(issued.to_vec());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(stopped, ["a", "b"]);
        assert_eq!(rounds.issued, ["op-a", "op-b"]);
        assert_eq!(
            marks,
            [vec!["op-a".to_string()], vec!["op-a".into(), "op-b".into()]]
        );
    }

    /// Router review item 14: the enumeration that writes the drain intent runs
    /// before any Stop is issued, so the host is out of placement first.
    // T10 T33
    #[test]
    fn the_intent_enumeration_precedes_every_stop() {
        let events = std::cell::RefCell::new(Vec::<String>::new());
        drain_rounds(
            &mut || {
                events.borrow_mut().push("enumerate".into());
                Ok(vec![candidate("a", 0), candidate("b", 0)])
            },
            &mut |c| {
                events
                    .borrow_mut()
                    .push(format!("stop {}", c.deployment_id));
                Ok(Some(ActionReceipt {
                    operation_id: format!("op-{}", c.deployment_id),
                    deployment_id: c.deployment_id.clone(),
                    revision: 1,
                    joined: false,
                }))
            },
            &mut |_| {
                events.borrow_mut().push("mark".into());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            events.into_inner(),
            ["enumerate", "stop a", "stop b", "mark", "enumerate"]
        );
    }

    /// A marker that cannot be recorded fails the drain (the operator retries
    /// with the same key, which replays the same Stops).
    // T32
    #[test]
    fn a_marker_failure_fails_the_drain() {
        let result = drain_rounds(
            &mut || Ok(vec![candidate("a", 0)]),
            &mut |c| {
                Ok(Some(ActionReceipt {
                    operation_id: "op".into(),
                    deployment_id: c.deployment_id.clone(),
                    revision: 1,
                    joined: false,
                }))
            },
            &mut |_| Err(ConfigurationFailure::ReconciliationRequired),
        );
        assert!(matches!(
            result,
            Err(ConfigurationFailure::ReconciliationRequired)
        ));
    }
}

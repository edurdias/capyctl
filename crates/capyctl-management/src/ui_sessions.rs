//! ADR 0040 §§2, 4–7: the role's one session verifier and the browser session
//! routes.
//!
//! The verifier holds the admin credential, the in-memory session table and
//! the revocation broadcast. The table is loaded from the store when the role
//! starts and every change is written through to it, so resolving a session
//! never reads the store (§2). Only SHA-256 digests of the 256-bit tokens are
//! kept, in memory as in SQLite; a token reaches the browser once, in the
//! sign-in link or the cookie.
use crate::{error, ManagementCredentials};
use axum::{
    body::{to_bytes, Body},
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use capyctl_store::{
    ui_sessions::{Digest, SigninOutcome, UiSession, LAST_SEEN_INTERVAL_MS, UI_SESSION_LABEL_MAX},
    Store, StoreError,
};
use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use sha2::Sha256;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use tokio::sync::broadcast;

/// ADR 0040 §4: the session cookie's name.
pub const SESSION_COOKIE: &str = "capyctl_session";
/// ADR 0040 §5: a sign-in link is valid for 10 minutes.
pub const SIGN_IN_LINK_LIFETIME_MS: i64 = 10 * 60 * 1000;
/// ADR 0040 §4: the cookie's `Max-Age`, 400 days, the browsers' limit.
const COOKIE_MAX_AGE_SECS: i64 = 400 * 24 * 60 * 60;
/// ADR 0040 §4: the cookie is re-issued on use once a day has passed since
/// its last refresh.
const COOKIE_REFRESH_MS: i64 = 24 * 60 * 60 * 1000;
const BODY_LIMIT: usize = 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(5);

/// Who a management request acts as (ADR 0040 §2). The composition resolves
/// it once and stores it in the request's extensions; every route reads it
/// from there.
#[derive(Clone)]
pub enum Principal {
    /// The admin bearer token, from the box (§3).
    Admin,
    /// A browser session. It has the admin token's authority (§4).
    Session(SessionPrincipal),
}

#[derive(Clone)]
pub struct SessionPrincipal {
    id: String,
    verifier: Arc<SessionVerifier>,
}

impl Principal {
    /// The session's id, or `None` for the admin token.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Principal::Admin => None,
            Principal::Session(session) => Some(&session.id),
        }
    }

    /// ADR 0040 §6: resolves once this principal's session is revoked, so a
    /// stream or wait it owns ends then. Never resolves for the admin token.
    pub async fn revoked(&self) {
        let Principal::Session(session) = self else {
            return std::future::pending().await;
        };
        // Subscribed before the table is read, so a revocation between the
        // two is still seen.
        let mut revocations = session.verifier.revocations.subscribe();
        loop {
            if !session.verifier.holds(&session.id) {
                return;
            }
            match revocations.recv().await {
                Ok(Revocation::All) => return,
                Ok(Revocation::Session(id)) if id == session.id => return,
                Ok(Revocation::Session(_)) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

#[derive(Clone)]
enum Revocation {
    Session(String),
    All,
}

/// ADR 0040 §2: the role's one session verifier, shared by every router of
/// its management listener.
pub struct SessionVerifier {
    admin: ManagementCredentials,
    /// ADR 0040 §7: the fingerprint of the admin token's digest.
    fingerprint: Digest,
    store: Arc<Mutex<Store>>,
    table: RwLock<HashMap<Digest, UiSession>>,
    revocations: broadcast::Sender<Revocation>,
}

/// A session the composition resolved from a cookie.
pub(crate) struct Resolved {
    pub(crate) principal: Principal,
    /// Whether the cookie is due to be re-issued (ADR 0040 §4).
    pub(crate) refresh: bool,
}

impl SessionVerifier {
    /// ADR 0040 §§2, 7: called when the role starts, with its own connection
    /// to the role's store. Deletes every session and sign-in link issued
    /// under another admin token, then loads the session table.
    pub fn open(admin: ManagementCredentials, store: Store) -> Result<Arc<Self>, StoreError> {
        let fingerprint: Digest = sha256(&admin.digest);
        store.sweep_ui_credentials(&fingerprint)?;
        let table = store.ui_session_digests()?.into_iter().collect();
        Ok(Arc::new(Self {
            admin,
            fingerprint,
            store: Arc::new(Mutex::new(store)),
            table: RwLock::new(table),
            revocations: broadcast::channel(64).0,
        }))
    }

    pub(crate) fn accepts_admin(&self, request: &Request) -> bool {
        self.admin.accepts(request)
    }

    /// The session a cookie token names, and whether its cookie is due to be
    /// re-issued. Records last-seen at most once a minute and the refresh,
    /// both written through without holding up the request.
    pub(crate) fn resolve(self: &Arc<Self>, token: &str, now_ms: i64) -> Option<Resolved> {
        let digest = token_digest(token)?;
        let (id, seen, refresh) = {
            let mut table = self.table.write().unwrap_or_else(|p| p.into_inner());
            let session = table.get_mut(&digest)?;
            let seen = now_ms - session.last_seen_at_ms >= LAST_SEEN_INTERVAL_MS;
            if seen {
                session.last_seen_at_ms = now_ms;
            }
            let refresh = now_ms - session.cookie_refreshed_at_ms >= COOKIE_REFRESH_MS;
            if refresh {
                session.cookie_refreshed_at_ms = now_ms;
            }
            (session.id.clone(), seen, refresh)
        };
        if seen || refresh {
            let (store, id) = (self.store.clone(), id.clone());
            tokio::task::spawn_blocking(move || {
                let Ok(store) = store.lock() else { return };
                if seen {
                    let _ = store.record_ui_session_seen(&id, now_ms);
                }
                if refresh {
                    let _ = store.record_ui_session_cookie_refresh(&id, now_ms);
                }
            });
        }
        Some(Resolved {
            principal: Principal::Session(SessionPrincipal {
                id,
                verifier: self.clone(),
            }),
            refresh,
        })
    }

    fn holds(&self, id: &str) -> bool {
        self.table
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .any(|session| session.id == id)
    }

    fn sessions(&self) -> Vec<UiSession> {
        let mut sessions: Vec<UiSession> = self
            .table
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect();
        sessions.sort_by(|a, b| (a.created_at_ms, &a.id).cmp(&(b.created_at_ms, &b.id)));
        sessions
    }

    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, ()> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || work(&*store.lock().map_err(|_| ())?).map_err(|_| ()))
            .await
            .map_err(|_| ())?
    }

    /// ADR 0040 §5: a new sign-in link token and its expiry. Expired links
    /// are pruned first, so spent links do not pile up.
    async fn issue_link(&self) -> Result<(String, i64), ()> {
        let token = random_token()?;
        let digest = sha256(token.as_bytes());
        let fingerprint = self.fingerprint;
        let now = now_ms();
        let expires = now + SIGN_IN_LINK_LIFETIME_MS;
        self.blocking(move |store| {
            store.prune_ui_signin_links(now)?;
            store.issue_ui_signin_link(&digest, &fingerprint, expires)
        })
        .await?;
        Ok((token, expires))
    }

    /// ADR 0040 §5: spend a sign-in link and create a session, returning its
    /// cookie token. `None` when the link is unknown, used or expired.
    async fn sign_in(&self, link: &str, label: String) -> Result<Option<(String, UiSession)>, ()> {
        let Some(link) = token_digest(link) else {
            return Ok(None);
        };
        let token = random_token()?;
        let digest = sha256(token.as_bytes());
        let fingerprint = self.fingerprint;
        let now = now_ms();
        let stored_label = label.clone();
        let outcome = self
            .blocking(move |store| {
                store.consume_ui_signin_link(&link, &digest, &fingerprint, &stored_label, now)
            })
            .await?;
        let SigninOutcome::SignedIn { session_id } = outcome else {
            return Ok(None);
        };
        let session = UiSession {
            id: session_id,
            label,
            created_at_ms: now,
            last_seen_at_ms: now,
            cookie_refreshed_at_ms: now,
        };
        self.table
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(digest, session.clone());
        Ok(Some((token, session)))
    }

    /// ADR 0040 §6: delete session `id` and end what it holds open.
    async fn revoke(&self, id: String) -> Result<bool, ()> {
        let stored = id.clone();
        let existed = self
            .blocking(move |store| store.revoke_ui_session(&stored))
            .await?;
        self.table
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|_, session| session.id != id);
        let _ = self.revocations.send(Revocation::Session(id));
        Ok(existed)
    }

    /// ADR 0040 §6: delete every session and end what they hold open.
    async fn revoke_all(&self) -> Result<usize, ()> {
        let count = self
            .blocking(|store| store.revoke_all_ui_sessions())
            .await?;
        self.table
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        let _ = self.revocations.send(Revocation::All);
        Ok(count)
    }
}

/// ADR 0040 §§5, 6: the session routes. The composition resolves the
/// principal first; only the exchange runs without one.
pub(crate) fn router(verifier: Arc<SessionVerifier>) -> Router {
    Router::new()
        .route("/management/v1/ui/sign-in-links", post(issue_link))
        .route("/management/v1/ui/sessions", post(exchange).get(list))
        .route("/management/v1/ui/session", get(current))
        .route("/management/v1/ui/sessions/{id}/revoke", post(revoke))
        .route("/management/v1/ui/sessions/revoke-all", post(revoke_all))
        .route("/management/v1/ui/sign-out", post(sign_out))
        .with_state(verifier)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoFields {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Exchange {
    token: String,
}

/// ADR 0040 §5: link issuing requires the admin principal.
async fn issue_link(State(verifier): State<Arc<SessionVerifier>>, request: Request) -> Response {
    if request.uri().query().is_some() {
        return error(StatusCode::BAD_REQUEST, "invalid_request", false);
    }
    if !matches!(principal(&request), Some(Principal::Admin)) {
        return error(StatusCode::FORBIDDEN, "admin_required", false);
    }
    // An empty body or `{}`.
    let Ok(body) = body(request).await else {
        return error(StatusCode::BAD_REQUEST, "invalid_request", false);
    };
    if !body.is_empty() && serde_json::from_slice::<NoFields>(&body).is_err() {
        return error(StatusCode::BAD_REQUEST, "invalid_request", false);
    }
    match verifier.issue_link().await {
        Ok((token, expires_at_ms)) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "api_version": "1",
                "token": token,
                "expires_at_ms": expires_at_ms,
            })),
        )
            .into_response(),
        Err(()) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
    }
}

/// ADR 0040 §5: the unauthenticated exchange. It ignores any cookie it is
/// sent and sets a fresh one; the composition already applied §4's checks.
async fn exchange(State(verifier): State<Arc<SessionVerifier>>, request: Request) -> Response {
    if request.uri().query().is_some() {
        return error(StatusCode::BAD_REQUEST, "invalid_request", false);
    }
    let label = label(request.headers());
    let Ok(body) = body(request).await else {
        return error(StatusCode::BAD_REQUEST, "invalid_request", false);
    };
    let Ok(Exchange { token }) = serde_json::from_slice(&body) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request", false);
    };
    match verifier.sign_in(&token, label).await {
        Ok(Some((cookie, session))) => {
            let mut response = (
                StatusCode::CREATED,
                Json(serde_json::json!({"api_version": "1", "session": view(&session, true)})),
            )
                .into_response();
            set_cookie(&mut response, &cookie);
            response
        }
        // ADR 0040 §5 (AE1): a used, expired or unknown link signs nobody in.
        Ok(None) => error(StatusCode::UNAUTHORIZED, "sign_in_link_refused", false),
        Err(()) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
    }
}

/// ADR 0040 §6: the current principal's session; `null` for the admin token.
async fn current(State(verifier): State<Arc<SessionVerifier>>, request: Request) -> Response {
    if request.uri().query().is_some() {
        return error(StatusCode::BAD_REQUEST, "invalid_request", false);
    }
    let Some(principal) = principal(&request) else {
        return error(StatusCode::UNAUTHORIZED, "unauthenticated", false);
    };
    let session = principal.session_id().and_then(|id| {
        verifier
            .sessions()
            .into_iter()
            .find(|session| session.id == id)
    });
    Json(serde_json::json!({
        "api_version": "1",
        "principal": if session.is_some() { "session" } else { "admin" },
        "session": session.map(|session| view(&session, true)),
    }))
    .into_response()
}

/// ADR 0040 §6: every session, oldest first, marking the caller's own.
async fn list(State(verifier): State<Arc<SessionVerifier>>, request: Request) -> Response {
    if request.uri().query().is_some() {
        return error(StatusCode::BAD_REQUEST, "invalid_request", false);
    }
    let Some(principal) = principal(&request) else {
        return error(StatusCode::UNAUTHORIZED, "unauthenticated", false);
    };
    let own = principal.session_id();
    let sessions: Vec<_> = verifier
        .sessions()
        .iter()
        .map(|session| view(session, Some(session.id.as_str()) == own))
        .collect();
    Json(serde_json::json!({"api_version": "1", "sessions": sessions})).into_response()
}

async fn revoke(
    State(verifier): State<Arc<SessionVerifier>>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    request: Request,
) -> Response {
    let Ok(Path(id)) = path else {
        return error(StatusCode::BAD_REQUEST, "invalid_request", false);
    };
    let principal = match command(request).await {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let own = principal.session_id() == Some(id.as_str());
    match verifier.revoke(id).await {
        Ok(true) => revoked(
            serde_json::json!({"api_version": "1", "revoked": true}),
            own,
        ),
        Ok(false) => error(StatusCode::NOT_FOUND, "not_found", false),
        Err(()) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
    }
}

async fn revoke_all(State(verifier): State<Arc<SessionVerifier>>, request: Request) -> Response {
    let principal = match command(request).await {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    match verifier.revoke_all().await {
        Ok(count) => revoked(
            serde_json::json!({"api_version": "1", "revoked": count}),
            principal.session_id().is_some(),
        ),
        Err(()) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
    }
}

/// ADR 0040 §§4, 6: ends the caller's session and clears its cookie.
async fn sign_out(State(verifier): State<Arc<SessionVerifier>>, request: Request) -> Response {
    let principal = match command(request).await {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let signed_out = match principal.session_id() {
        Some(id) => match verifier.revoke(id.to_owned()).await {
            Ok(existed) => existed,
            Err(()) => return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
        },
        None => false,
    };
    revoked(
        serde_json::json!({"api_version": "1", "signed_out": signed_out}),
        true,
    )
}

/// A bodiless session command: no query, an empty body, a principal.
async fn command(request: Request) -> Result<Principal, Box<Response>> {
    if request.uri().query().is_some() {
        return Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            false,
        )));
    }
    let Some(principal) = principal(&request).cloned() else {
        return Err(Box::new(error(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            false,
        )));
    };
    match body(request).await {
        Ok(body) if body.is_empty() => Ok(principal),
        _ => Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            false,
        ))),
    }
}

fn revoked(body: serde_json::Value, clear: bool) -> Response {
    let mut response = Json(body).into_response();
    if clear {
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_str(&format!(
                "{SESSION_COOKIE}=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict"
            ))
            .expect("cookie header"),
        );
    }
    response
}

/// ADR 0040 §4: `HttpOnly`, `SameSite=Strict`, `Path=/`, no `Secure` (plain
/// HTTP, §1) and no `Domain`, for 400 days.
pub(crate) fn set_cookie(response: &mut Response, token: &str) {
    if let Ok(value) = HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Max-Age={COOKIE_MAX_AGE_SECS}; Path=/; HttpOnly; SameSite=Strict"
    )) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

fn view(session: &UiSession, current: bool) -> serde_json::Value {
    serde_json::json!({
        "id": session.id,
        "label": session.label,
        "created_at_ms": session.created_at_ms,
        "last_seen_at_ms": session.last_seen_at_ms,
        "current": current,
    })
}

fn principal(request: &Request) -> Option<&Principal> {
    request.extensions().get::<Principal>()
}

async fn body(request: Request) -> Result<axum::body::Bytes, ()> {
    let body: Body = request.into_body();
    match tokio::time::timeout(BODY_TIMEOUT, to_bytes(body, BODY_LIMIT)).await {
        Ok(Ok(bytes)) => Ok(bytes),
        _ => Err(()),
    }
}

/// ADR 0040 §4: the session's label is the browser's user agent, shortened to
/// fit the store's limit.
fn label(headers: &HeaderMap) -> String {
    let agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|agent| !agent.is_empty())
        .unwrap_or("Unknown browser");
    agent
        .chars()
        .filter(|c| !c.is_control())
        .take(UI_SESSION_LABEL_MAX)
        .collect()
}

/// ADR 0040 §4: a 256-bit token from the operating system's generator, as 64
/// hex characters.
fn random_token() -> Result<String, ()> {
    let mut bytes = [0_u8; 32];
    OsRng.try_fill_bytes(&mut bytes).map_err(|_| ())?;
    Ok(hex::encode(bytes))
}

/// The digest of a well-formed token; `None` for anything else.
fn token_digest(token: &str) -> Option<Digest> {
    (token.len() == 64
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    .then(|| sha256(token.as_bytes()))
}

fn sha256(bytes: &[u8]) -> Digest {
    use sha2::Digest as _;
    Sha256::digest(bytes).into()
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.as_millis() as i64)
        .unwrap_or(0)
}

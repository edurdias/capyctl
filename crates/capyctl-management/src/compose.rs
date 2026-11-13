//! ADR 0040 §§2–4: one composition point authorizes every management route.
//!
//! [`compose`] merges a role's management routers under one outer layer. The
//! layer resolves the principal, the admin token or a browser session, and
//! stores it in the request's extensions, where every router's own
//! `authenticate` layer reads it. It refuses unauthenticated requests, except
//! the session exchange. [`standalone`] and [`server`] are the two roles'
//! compositions, so tests build exactly what the roles serve.
use crate::{
    actions::OwnedActionSource,
    drain::HostPresence,
    engine_log::EngineLogSource,
    error,
    hosts::StandaloneHost,
    inference_listener::InferenceListenerView,
    metrics::{LatencySource, LoadSource},
    ui_sessions::{self, Principal, SessionVerifier, SESSION_COOKIE},
};
use axum::{
    extract::{
        connect_info::{Connected, MockConnectInfo},
        ConnectInfo, Request, State,
    },
    http::{header, HeaderMap, Method},
    middleware::{self, Next},
    response::Response,
    serve::{IncomingStream, Listener},
    Router,
};
use capyctl_controller::{
    agent_sessions::AgentSessions, enrollment::EnrollmentAuthority,
    installation_gate::EmbeddedInstallations, ownership::SharedCoordinatorState,
};
use std::{net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;

/// ADR 0040 §3: the two ends of a management connection, known for each
/// request when the composed router is served on a [`RoleListener`] with
/// `into_make_service_with_connect_info::<ManagementPeer>()`.
#[derive(Clone, Copy, Debug)]
pub struct ManagementPeer {
    /// The client's address.
    pub peer: SocketAddr,
    /// The address the connection reached on this machine.
    pub local: Option<SocketAddr>,
}

impl Connected<IncomingStream<'_, RoleListener>> for ManagementPeer {
    fn connect_info(stream: IncomingStream<'_, RoleListener>) -> Self {
        Self {
            peer: *stream.remote_addr(),
            local: stream.io().local_addr().ok(),
        }
    }
}

/// A role listener's socket. SPEC §17: every accepted connection has
/// Nagle's algorithm off, so a small write never waits for a delayed ACK
/// (axum 0.8 no longer sets `TCP_NODELAY` itself); a socket that refuses the
/// option is served as it is.
pub struct RoleListener(TcpListener);

impl RoleListener {
    pub fn new(listener: TcpListener) -> Self {
        Self(listener)
    }
}

impl Listener for RoleListener {
    type Io = tokio::net::TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, address) = Listener::accept(&mut self.0).await;
        let _ = stream.set_nodelay(true);
        (stream, address)
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

struct Access {
    verifier: Arc<SessionVerifier>,
    /// ADR 0040 §1: the configured `public_address` origin, accepted as an
    /// `Origin` besides `http://` plus the request's authority (§4).
    public_origin: Option<String>,
}

/// ADR 0040 §2: merge `routers` and the session routes under the layer that
/// resolves the principal. `public_address` is the role's configured origin.
pub fn compose(
    verifier: Arc<SessionVerifier>,
    public_address: Option<&str>,
    routers: impl IntoIterator<Item = Router>,
) -> Router {
    let access = Arc::new(Access {
        verifier: verifier.clone(),
        public_origin: public_address.map(|origin| origin.trim_end_matches('/').to_owned()),
    });
    routers
        .into_iter()
        .fold(ui_sessions::router(verifier), Router::merge)
        .layer(middleware::from_fn_with_state(access, resolve))
}

/// What a standalone role's management listener serves.
pub struct StandaloneRoutes {
    pub actions: Arc<OwnedActionSource>,
    pub check_sources: Arc<dyn crate::check::SourceProvider>,
    /// The names the embedded host answers to for a drain.
    pub host_names: Vec<String>,
    pub installations: Arc<EmbeddedInstallations>,
    pub engine_logs: Arc<dyn EngineLogSource>,
    pub owner: SharedCoordinatorState,
    pub host: StandaloneHost,
    pub latency: Arc<dyn LatencySource>,
    pub load: Arc<dyn LoadSource>,
    pub inference_listener: Arc<InferenceListenerView>,
}

/// The standalone role's composed management router.
pub fn standalone(
    verifier: Arc<SessionVerifier>,
    public_address: Option<&str>,
    routes: StandaloneRoutes,
) -> Router {
    compose(
        verifier,
        public_address,
        [
            crate::lifecycle_router(routes.actions.clone()),
            crate::check::router(routes.actions.configuration().clone(), routes.check_sources),
            // SPEC §4.3: the explicit drain of the embedded host, by its
            // published name or the `standalone` alias the CLI uses.
            crate::drain::drain_router(routes.actions, routes.host_names),
            // ADR 0008: the registered installation, shown by standalone status.
            crate::installation::installation_router(routes.installations),
            // SPEC §4.2: the same host and engine inventory a server serves.
            crate::hosts::standalone_hosts_router(routes.owner, routes.host),
            // SPEC §13.3 / T21: an instance's engine log.
            crate::engine_log::engine_log_router(routes.engine_logs),
            // SPEC §17 (M80) and §§10, 17: latency and live load.
            crate::metrics::latency_router(routes.latency),
            crate::metrics::load_router(routes.load),
            // Design §9: the inference listener's bind and authentication.
            crate::inference_listener::inference_listener_router(routes.inference_listener),
        ],
    )
}

/// What a server's management listener serves.
pub struct ServerRoutes {
    pub actions: Arc<OwnedActionSource>,
    /// Whether an enrolled host is connected now (drain answers).
    pub presence: HostPresence,
    pub enrollment: Arc<EnrollmentAuthority>,
    pub bootstrap_address: String,
    pub control_address: String,
    pub engine_logs: Arc<dyn EngineLogSource>,
    pub owner: SharedCoordinatorState,
    pub sessions: Arc<AgentSessions>,
    pub latency: Arc<dyn LatencySource>,
    pub load: Arc<dyn LoadSource>,
    pub inference_listener: Arc<InferenceListenerView>,
}

/// The server role's composed management router. Refuses an invalid
/// bootstrap or control address.
pub fn server(
    verifier: Arc<SessionVerifier>,
    public_address: Option<&str>,
    routes: ServerRoutes,
) -> Result<Router, &'static str> {
    Ok(compose(
        verifier,
        public_address,
        [
            crate::lifecycle_router(routes.actions.clone()),
            crate::check::router(
                routes.actions.configuration().clone(),
                Arc::new(crate::check::UnavailableSources),
            ),
            // SPEC §4.3: the explicit drain of an enrolled host.
            crate::drain::drain_router_with_presence(routes.actions, Vec::new(), routes.presence),
            crate::enrollment::enrollment_router(
                routes.enrollment,
                routes.bootstrap_address,
                routes.control_address,
            )?
            .reset_fallback(),
            // SPEC §13.3 / T21: an instance's engine log, read on its host.
            crate::engine_log::engine_log_router(routes.engine_logs),
            crate::hosts::hosts_router(routes.owner, routes.sessions),
            // SPEC §17 (M80) and §§10, 17: latency and live load.
            crate::metrics::latency_router(routes.latency),
            crate::metrics::load_router(routes.load),
            // Design §9: the inference listener's bind and authentication.
            crate::inference_listener::inference_listener_router(routes.inference_listener),
        ],
    ))
}

/// ADR 0040 §§2–4: resolve the principal, or refuse.
async fn resolve(State(access): State<Arc<Access>>, mut request: Request, next: Next) -> Response {
    let mut response = match principal(&access, &request) {
        Outcome::Admin => {
            request.extensions_mut().insert(Principal::Admin);
            next.run(request).await
        }
        Outcome::Session { principal, refresh } => {
            request.extensions_mut().insert(principal);
            let mut response = next.run(request).await;
            // ADR 0040 §4: re-issued on use once a day, unless the route
            // already answered with the cookie (sign-out, revocation).
            if let Some(token) = refresh {
                if !response.headers().contains_key(header::SET_COOKIE) {
                    ui_sessions::set_cookie(&mut response, &token);
                }
            }
            response
        }
        Outcome::Exchange => next.run(request).await,
        Outcome::Refused(response) => response,
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}

enum Outcome {
    Admin,
    Session {
        principal: Principal,
        /// The cookie token to re-issue, when due.
        refresh: Option<String>,
    },
    /// ADR 0040 §5: the unauthenticated session exchange, checks passed.
    Exchange,
    Refused(Response),
}

fn principal(access: &Access, request: &Request) -> Outcome {
    let unauthenticated = || {
        Outcome::Refused(error(
            axum::http::StatusCode::UNAUTHORIZED,
            "unauthenticated",
            false,
        ))
    };
    let refused = || {
        Outcome::Refused(error(
            axum::http::StatusCode::FORBIDDEN,
            "origin_refused",
            false,
        ))
    };
    // ADR 0040 §2: when an `Authorization` header is present it alone decides.
    if request.headers().contains_key(header::AUTHORIZATION) {
        // §3: the admin token only from the box, and never relayed.
        return if from_the_box(request)
            && !forwarded(request.headers())
            && access.verifier.accepts_admin(request)
        {
            Outcome::Admin
        } else {
            unauthenticated()
        };
    }
    // §5: the exchange ignores any cookie, under the same request checks.
    if request.method() == Method::POST && request.uri().path() == "/management/v1/ui/sessions" {
        return if browser_checks(request, access.public_origin.as_deref()) {
            Outcome::Exchange
        } else {
            refused()
        };
    }
    let Some(token) = session_cookie(request.headers()) else {
        return unauthenticated();
    };
    if !browser_checks(request, access.public_origin.as_deref()) {
        return refused();
    }
    match access.verifier.resolve(&token, ui_sessions::now_ms()) {
        Some(resolved) => Outcome::Session {
            principal: resolved.principal,
            refresh: resolved.refresh.then_some(token),
        },
        None => unauthenticated(),
    }
}

/// ADR 0040 §3: a loopback peer, or one whose address is the connection's
/// own local address (the CLI on the box reaching a specific bind).
fn from_the_box(request: &Request) -> bool {
    let extensions = request.extensions();
    let Some(connection) = extensions
        .get::<ConnectInfo<ManagementPeer>>()
        .map(|info| info.0)
        .or_else(|| {
            extensions
                .get::<MockConnectInfo<ManagementPeer>>()
                .map(|info| info.0)
        })
    else {
        return false;
    };
    let peer = connection.peer.ip().to_canonical();
    peer.is_loopback()
        || connection
            .local
            .is_some_and(|local| local.ip().to_canonical() == peer)
}

fn forwarded(headers: &HeaderMap) -> bool {
    headers.contains_key("x-forwarded-for") || headers.contains_key(header::FORWARDED)
}

/// The one `capyctl_session` cookie value; `None` without one, or with two.
fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        let value = value.to_str().ok()?;
        for pair in value.split(';') {
            let Some((name, token)) = pair.trim().split_once('=') else {
                continue;
            };
            if name == SESSION_COOKIE {
                if found.is_some() {
                    return None;
                }
                found = Some(token.to_owned());
            }
        }
    }
    found
}

/// ADR 0040 §4: the request checks every cookie-authenticated request, and
/// the exchange, must pass.
fn browser_checks(request: &Request, public_origin: Option<&str>) -> bool {
    let headers = request.headers();
    if !single(headers, "x-capyctl-ui").is_some_and(|value| value == "1") {
        return false;
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        let Some(origin) = single(headers, header::ORIGIN.as_str()) else {
            return false;
        };
        let authority = request
            .uri()
            .authority()
            .map(|authority| authority.as_str())
            .or_else(|| single(headers, header::HOST.as_str()));
        let own = authority
            .is_some_and(|authority| origin.eq_ignore_ascii_case(&format!("http://{authority}")));
        let public = public_origin.is_some_and(|public| origin.eq_ignore_ascii_case(public));
        if !own && !public {
            return false;
        }
        if headers.contains_key("sec-fetch-site")
            && single(headers, "sec-fetch-site") != Some("same-origin")
        {
            return false;
        }
    }
    let has_body = headers.contains_key(header::TRANSFER_ENCODING)
        || headers.contains_key(header::CONTENT_TYPE)
        || headers
            .get(header::CONTENT_LENGTH)
            .is_some_and(|length| length != "0");
    if has_body {
        let json = single(headers, header::CONTENT_TYPE.as_str()).is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
        });
        if !json {
            return false;
        }
    }
    true
}

fn single<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok()
}

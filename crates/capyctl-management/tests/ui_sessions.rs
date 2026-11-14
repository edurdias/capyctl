//! ADR 0040 §§2–7: the composed router's principal resolution, the browser
//! request checks, and the session routes. A role's management routers are
//! served through `compose`; here it wraps the snapshot router.
use axum::{
    body::{to_bytes, Body},
    extract::connect_info::MockConnectInfo,
    http::{Request, Response},
    Router,
};
use capyctl_management::{
    compose::{compose, ManagementPeer, RoleListener},
    snapshot_router,
    ui_sessions::SessionVerifier,
    ManagementCredentials, StoreSnapshotSource,
};
use capyctl_store::Store;
use serde_json::Value;
use std::future::Future;
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc};
use tower::ServiceExt;

const ADMIN: &str = "management-credential-012345678901234567890";
const ROTATED: &str = "management-credential-rotated-01234567890";
const INFERENCE: &str = "inference-credential-0123456789012345678901";
/// The authority the browser reached; `Origin` must be `http://` plus it.
const HOST: &str = "box.lan:7443";
/// ADR 0040 §1: the configured `public_address`, the origin a TLS proxy
/// that keeps the `Host` header serves the UI on.
const PUBLIC: &str = "https://box.tail1234.ts.net";
const LOOPBACK: &str = "127.0.0.1:50000";
const REMOTE: &str = "198.51.100.7:50000";

fn state_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A role started on `dir`'s store under the admin token `admin`: its one
/// verifier, its composed router, before any peer is known.
fn role(dir: &Path, admin: &str) -> Router {
    let verifier = SessionVerifier::open(
        ManagementCredentials::from_trusted_resolver(admin, INFERENCE).unwrap(),
        Store::open(&dir.join("srv.sqlite3")).unwrap(),
    )
    .unwrap();
    compose(
        verifier,
        Some(PUBLIC),
        [snapshot_router(Arc::new(StoreSnapshotSource::new(
            Store::open_in_memory().unwrap(),
        )))],
    )
}

/// `router` as a connection from `peer` to a `0.0.0.0` bind on 192.0.2.1 sees it.
fn from(router: &Router, peer: &str) -> Router {
    router.clone().layer(MockConnectInfo(ManagementPeer {
        peer: peer.parse().unwrap(),
        local: Some("192.0.2.1:7443".parse().unwrap()),
    }))
}

async fn json(response: Response<Body>) -> Value {
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&body).unwrap_or(Value::Null)
}

async fn issue_link(router: &Router, admin: &str) -> String {
    let response = from(router, LOOPBACK)
        .oneshot(
            Request::post("/management/v1/ui/sign-in-links")
                .header("authorization", format!("Bearer {admin}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let value = json(response).await;
    assert!(value["expires_at_ms"].as_i64().unwrap() > 0);
    value["token"].as_str().unwrap().to_owned()
}

/// The page's exchange of `link`, as the browser on `HOST` sends it.
fn exchange_request(link: &str, cookie: Option<&str>) -> Request<Body> {
    let mut request = Request::post("/management/v1/ui/sessions")
        .header("host", HOST)
        .header("x-capyctl-ui", "1")
        .header("origin", format!("http://{HOST}"))
        .header("sec-fetch-site", "same-origin")
        .header("content-type", "application/json")
        .header("user-agent", "Test Browser");
    if let Some(cookie) = cookie {
        request = request.header("cookie", format!("capyctl_session={cookie}"));
    }
    request
        .body(Body::from(serde_json::json!({ "token": link }).to_string()))
        .unwrap()
}

struct Session {
    id: String,
    token: String,
}

async fn sign_in(router: &Router, admin: &str) -> Session {
    let link = issue_link(router, admin).await;
    let response = from(router, REMOTE)
        .oneshot(exchange_request(&link, None))
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let token = cookie_token(&response).unwrap();
    let value = json(response).await;
    Session {
        id: value["session"]["id"].as_str().unwrap().to_owned(),
        token,
    }
}

/// The `capyctl_session` value a response sets.
fn cookie_token(response: &Response<Body>) -> Option<String> {
    let cookie = response.headers().get("set-cookie")?.to_str().unwrap();
    let (pair, _) = cookie.split_once(';')?;
    Some(pair.strip_prefix("capyctl_session=")?.to_owned())
}

/// A request from the UI page on `HOST`, with every check satisfied.
fn browser(method: &str, uri: &str, token: &str) -> axum::http::request::Builder {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", HOST)
        .header("x-capyctl-ui", "1")
        .header("cookie", format!("theme=dark; capyctl_session={token}"));
    if method == "GET" {
        request
    } else {
        request
            .header("origin", format!("http://{HOST}"))
            .header("sec-fetch-site", "same-origin")
    }
}

async fn refused(router: &Router, request: Request<Body>) {
    let response = from(router, REMOTE).oneshot(request).await.unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let value = json(response).await;
    assert_eq!(value["api_version"], "1");
    assert_eq!(value["error"]["code"], "origin_refused");
}

/// A session POST that changes something: revoking a second session.
async fn revoke_other(router: &Router) -> (Session, String) {
    let other = sign_in(router, ADMIN).await;
    let uri = format!("/management/v1/ui/sessions/{}/revoke", other.id);
    (other, uri)
}

// ADR 0040 §4: every cookie request, GET included, carries `X-CapyCTL-UI: 1`.
#[tokio::test]
async fn session_get_without_the_ui_header_is_refused() {
    let dir = state_dir();
    let router = role(dir.path(), ADMIN);
    let session = sign_in(&router, ADMIN).await;
    let ok = from(&router, REMOTE)
        .oneshot(
            browser("GET", "/management/v1/snapshot", &session.token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let mut request = browser("GET", "/management/v1/snapshot", &session.token)
        .body(Body::empty())
        .unwrap();
    request.headers_mut().remove("x-capyctl-ui");
    refused(&router, request).await;
}

// ADR 0040 §4: a non-GET cookie request's `Origin` is exactly `http://` plus
// the authority, or the configured `public_address`; no `Origin`, a foreign
// one, a body that is not JSON, or a cross-site fetch is refused.
#[tokio::test]
async fn session_posts_pass_only_the_request_checks() {
    let dir = state_dir();
    let router = role(dir.path(), ADMIN);
    let session = sign_in(&router, ADMIN).await;

    let (_, uri) = revoke_other(&router).await;
    let mut request = browser("POST", &uri, &session.token)
        .body(Body::empty())
        .unwrap();
    request
        .headers_mut()
        .insert("origin", "http://evil.example".parse().unwrap());
    refused(&router, request).await;

    let mut request = browser("POST", &uri, &session.token)
        .body(Body::empty())
        .unwrap();
    request.headers_mut().remove("origin");
    refused(&router, request).await;

    let request = browser("POST", &uri, &session.token)
        .header("content-type", "text/plain")
        .body(Body::from("{}"))
        .unwrap();
    refused(&router, request).await;

    let mut request = browser("POST", &uri, &session.token)
        .body(Body::empty())
        .unwrap();
    request
        .headers_mut()
        .insert("sec-fetch-site", "same-site".parse().unwrap());
    refused(&router, request).await;

    // The same POST, with a matching Origin in either letter case.
    let mut request = browser("POST", &uri, &session.token)
        .body(Body::empty())
        .unwrap();
    request.headers_mut().insert(
        "origin",
        format!("HTTP://{}", HOST.to_uppercase()).parse().unwrap(),
    );
    let response = from(&router, REMOTE).oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(json(response).await["revoked"], true);

    // Behind a TLS proxy that keeps `Host`, the Origin is `public_address`.
    let (_, uri) = revoke_other(&router).await;
    let mut request = browser("POST", &uri, &session.token)
        .body(Body::empty())
        .unwrap();
    request
        .headers_mut()
        .insert("origin", PUBLIC.parse().unwrap());
    let response = from(&router, REMOTE).oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
}

// ADR 0040 §3: the admin token from a peer that is not on the box is refused,
// whatever else the request carries; `Authorization` alone decides.
#[tokio::test]
async fn remote_bearer_is_refused_even_with_a_valid_cookie() {
    let dir = state_dir();
    let router = role(dir.path(), ADMIN);
    let session = sign_in(&router, ADMIN).await;
    for cookie in [false, true] {
        let mut request = browser("GET", "/management/v1/snapshot", &session.token)
            .header("authorization", format!("Bearer {ADMIN}"));
        if !cookie {
            request = Request::get("/management/v1/snapshot")
                .header("authorization", format!("Bearer {ADMIN}"));
        }
        let response = from(&router, REMOTE)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        assert_eq!(json(response).await["error"]["code"], "unauthenticated");
    }
}

/// `router` served on a real socket bound to `bind`, as the roles serve it.
async fn serve(router: Router, bind: &str) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            RoleListener::new(listener),
            router.into_make_service_with_connect_info::<ManagementPeer>(),
        )
        .await
        .unwrap();
    });
    (address, server)
}

async fn bearer_status(address: std::net::SocketAddr, extra: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client
        .write_all(
            format!(
                "GET /management/v1/snapshot HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {ADMIN}\r\n{extra}Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        client.read_to_end(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    text.lines().next().unwrap().to_owned()
}

// ADR 0040 §3: on a real socket, the bearer token is accepted from loopback,
// and from a peer whose address is the connection's own local address, but
// never with `X-Forwarded-For` or `Forwarded`.
#[tokio::test]
async fn bearer_is_accepted_from_the_box_only_on_a_real_socket() {
    let dir = state_dir();
    let (address, server) = serve(role(dir.path(), ADMIN), "127.0.0.1:0").await;
    assert_eq!(bearer_status(address, "").await, "HTTP/1.1 200 OK");
    assert_eq!(
        bearer_status(address, "X-Forwarded-For: 203.0.113.9\r\n").await,
        "HTTP/1.1 401 Unauthorized"
    );
    assert_eq!(
        bearer_status(address, "Forwarded: for=203.0.113.9\r\n").await,
        "HTTP/1.1 401 Unauthorized"
    );
    server.abort();

    // The CLI on the box reaching a `0.0.0.0` bind through the machine's own
    // non-loopback address: the peer is the connection's local address.
    let own = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| socket.connect("192.0.2.1:9").map(|_| socket))
        .and_then(|socket| socket.local_addr())
        .map(|address| address.ip());
    let Ok(own) = own else {
        // No non-loopback IPv4 address on this machine: nothing to reach.
        return;
    };
    let (bound, server) = serve(role(dir.path(), ADMIN), "0.0.0.0:0").await;
    let address = std::net::SocketAddr::new(own, bound.port());
    assert_eq!(bearer_status(address, "").await, "HTTP/1.1 200 OK");
    server.abort();
}

// ADR 0040 §3: a non-loopback peer that is not this machine is refused the
// bearer token on a `0.0.0.0` bind. One machine cannot open such a
// connection to itself, so its connection info is supplied.
#[tokio::test]
async fn bearer_from_a_remote_peer_is_refused() {
    let dir = state_dir();
    let router = role(dir.path(), ADMIN);
    let response = from(&router, REMOTE)
        .oneshot(
            Request::post("/management/v1/ui/sign-in-links")
                .header("authorization", format!("Bearer {ADMIN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

// ADR 0040 §§4–5 (AE1): the exchange sets a fresh cookie, whatever cookie it
// was sent; a link signs in once.
#[tokio::test]
async fn exchange_sets_a_fresh_cookie_and_a_link_signs_in_once() {
    let dir = state_dir();
    let router = role(dir.path(), ADMIN);
    let earlier = sign_in(&router, ADMIN).await;
    let link = issue_link(&router, ADMIN).await;
    let send =
        |link: String| from(&router, REMOTE).oneshot(exchange_request(&link, Some(&earlier.token)));
    let response = send(link.clone()).await.unwrap();
    assert_eq!(response.status(), 201);
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .to_owned();
    let attributes: Vec<&str> = cookie.split(';').map(str::trim).collect();
    for expected in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age=34560000"] {
        assert!(attributes.contains(&expected), "{cookie}");
    }
    assert!(!cookie.to_ascii_lowercase().contains("secure"), "{cookie}");
    assert!(!cookie.to_ascii_lowercase().contains("domain"), "{cookie}");
    let token = cookie_token(&response).unwrap();
    assert_ne!(token, earlier.token);
    let value = json(response).await;
    assert_eq!(value["session"]["label"], "Test Browser");
    assert_eq!(value["session"]["current"], true);

    let response = from(&router, REMOTE)
        .oneshot(
            browser("GET", "/management/v1/ui/session", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(json(response).await["principal"], "session");

    let response = send(link).await.unwrap();
    assert_eq!(response.status(), 401);
    assert!(response.headers().get("set-cookie").is_none());

    // The exchange is held to the same request checks.
    let link = issue_link(&router, ADMIN).await;
    let mut request = exchange_request(&link, None);
    request
        .headers_mut()
        .insert("origin", "http://evil.example".parse().unwrap());
    refused(&router, request).await;
}

// ADR 0040 §5: only the admin principal issues sign-in links.
#[tokio::test]
async fn a_session_cannot_issue_sign_in_links() {
    let dir = state_dir();
    let router = role(dir.path(), ADMIN);
    let session = sign_in(&router, ADMIN).await;
    let response = from(&router, REMOTE)
        .oneshot(
            browser("POST", "/management/v1/ui/sign-in-links", &session.token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(json(response).await["error"]["code"], "admin_required");
    issue_link(&router, ADMIN).await;
}

// ADR 0040 §§4, 6: sign-out clears the cookie and ends the session.
#[tokio::test]
async fn sign_out_clears_the_cookie_and_ends_the_session() {
    let dir = state_dir();
    let router = role(dir.path(), ADMIN);
    let session = sign_in(&router, ADMIN).await;
    let response = from(&router, REMOTE)
        .oneshot(
            browser("POST", "/management/v1/ui/sign-out", &session.token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let cookie = response.headers()["set-cookie"].to_str().unwrap();
    assert!(cookie.starts_with("capyctl_session=;"), "{cookie}");
    assert!(cookie.contains("Max-Age=0"), "{cookie}");
    assert_eq!(json(response).await["signed_out"], true);
    let response = from(&router, REMOTE)
        .oneshot(
            browser("GET", "/management/v1/ui/session", &session.token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

// ADR 0040 §6: listing marks the caller's own session; revoke-all ends all.
#[tokio::test]
async fn sessions_are_listed_and_revoked_together() {
    let dir = state_dir();
    let router = role(dir.path(), ADMIN);
    let first = sign_in(&router, ADMIN).await;
    let second = sign_in(&router, ADMIN).await;
    let response = from(&router, REMOTE)
        .oneshot(
            browser("GET", "/management/v1/ui/sessions", &second.token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let value = json(response).await;
    let sessions = value["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0]["id"], first.id.as_str());
    assert_eq!(sessions[0]["current"], false);
    assert_eq!(sessions[1]["current"], true);
    let response = from(&router, REMOTE)
        .oneshot(
            browser(
                "POST",
                "/management/v1/ui/sessions/revoke-all",
                &first.token,
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .contains("Max-Age=0"));
    assert_eq!(json(response).await["revoked"], 2);
    for token in [&first.token, &second.token] {
        let response = from(&router, REMOTE)
            .oneshot(
                browser("GET", "/management/v1/ui/session", token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }
}

// ADR 0040 §4: a cookie is re-issued on use once a day has passed since its
// last refresh, and not before.
#[tokio::test]
async fn a_day_old_cookie_is_re_issued_on_a_get() {
    let dir = state_dir();
    let router = role(dir.path(), ADMIN);
    let session = sign_in(&router, ADMIN).await;
    let get = |router: &Router| {
        from(router, REMOTE).oneshot(
            browser("GET", "/management/v1/ui/session", &session.token)
                .body(Body::empty())
                .unwrap(),
        )
    };
    let response = get(&router).await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers().get("set-cookie").is_none());
    drop(router);

    let two_days_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        - 2 * 24 * 60 * 60 * 1000;
    assert!(Store::open(&dir.path().join("srv.sqlite3"))
        .unwrap()
        .record_ui_session_cookie_refresh(&session.id, two_days_ago)
        .unwrap());
    let router = role(dir.path(), ADMIN);
    let response = get(&router).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(cookie_token(&response).unwrap(), session.token);
    assert!(response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .contains("Max-Age=34560000"));
    let response = get(&router).await.unwrap();
    assert!(response.headers().get("set-cookie").is_none());
}

// ADR 0040 §7 (AE2): sessions survive a restart under the same admin token,
// and a restart under another token ends every session and link.
#[tokio::test]
async fn rotating_the_admin_token_by_restart_ends_sessions_and_links() {
    let dir = state_dir();
    let router = role(dir.path(), ADMIN);
    let session = sign_in(&router, ADMIN).await;
    let link = issue_link(&router, ADMIN).await;
    drop(router);
    let session_status = |router: Router, token: String| async move {
        from(&router, REMOTE)
            .oneshot(
                browser("GET", "/management/v1/ui/session", &token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    };
    assert_eq!(
        session_status(role(dir.path(), ADMIN), session.token.clone()).await,
        200
    );

    let router = role(dir.path(), ROTATED);
    assert_eq!(session_status(router.clone(), session.token).await, 401);
    let response = from(&router, REMOTE)
        .oneshot(exchange_request(&link, None))
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(response.headers().get("set-cookie").is_none());
}

// ADR 0040 §3 / KTD7 / T16: a local forwarding hop cannot relay the admin token.
#[tokio::test]
async fn every_proxy_header_refuses_the_admin_token() {
    let dir = state_dir();
    let router = role(dir.path(), ADMIN);
    for name in [
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-forwarded-anything",
        "forwarded",
        "x-real-ip",
        "via",
        "cf-connecting-ip",
        "true-client-ip",
        "tailscale-user-login",
        "tailscale-anything",
    ] {
        let response = from(&router, LOOPBACK)
            .oneshot(
                Request::get("/management/v1/snapshot")
                    .header("authorization", format!("Bearer {ADMIN}"))
                    .header(name, "relay")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{name}");
    }
}

// ADR 0040 §6 / KTD7 / T16: revocation ends a host-admin wait while the
// already dispatched host operation retains ownership and finishes.
#[tokio::test]
async fn revoked_host_admin_wait_does_not_cancel_the_operation() {
    use capyctl_management::host_admin::{self, Failure, HostAdminSource, Operation};
    struct Blocking {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        finished: tokio::sync::Notify,
    }
    impl HostAdminSource for Blocking {
        fn execute(
            &self,
            _: String,
            _: Operation,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<Value, Failure>> + Send + '_>> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                self.finished.notify_one();
                Ok(serde_json::json!({}))
            })
        }
    }
    let verifier = SessionVerifier::open(
        ManagementCredentials::from_trusted_resolver(ADMIN, INFERENCE).unwrap(),
        Store::open_in_memory().unwrap(),
    )
    .unwrap();
    let source = Arc::new(Blocking {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        finished: tokio::sync::Notify::new(),
    });
    let router = compose(verifier, Some(PUBLIC), [host_admin::router(source.clone())]);
    let session = sign_in(&router, ADMIN).await;
    let pending = tokio::spawn(
        from(&router, REMOTE).oneshot(
            browser("GET", "/management/v1/hosts/host/config", &session.token)
                .body(Body::empty())
                .unwrap(),
        ),
    );
    source.entered.notified().await;
    let revoked = from(&router, LOOPBACK)
        .oneshot(
            Request::post(format!("/management/v1/ui/sessions/{}/revoke", session.id))
                .header("authorization", format!("Bearer {ADMIN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoked.status(), 200);
    let response = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), 401);
    source.release.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        source.finished.notified(),
    )
    .await
    .unwrap();
}

// ADR 0040 §6 / KTD7 / T16: revocation interrupts even the first store read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_interrupts_blocked_initial_event_read() {
    use capyctl_management::{
        events::{EventSource, EventStreamOptions},
        read_only_router_with_event_options, SnapshotSource, SnapshotUnavailable,
    };
    use capyctl_store::events::{EventPage, EventReadError};
    struct Blocked {
        entered: tokio::sync::Notify,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl SnapshotSource for Blocked {
        fn snapshot(&self) -> Result<capyctl_store::snapshot::Snapshot, SnapshotUnavailable> {
            Err(SnapshotUnavailable)
        }
    }
    impl EventSource for Blocked {
        fn events_after(&self, _: Option<&str>, _: usize) -> Result<EventPage, EventReadError> {
            self.entered.notify_one();
            self.release.lock().unwrap().recv().unwrap();
            Ok(EventPage {
                events: vec![],
                high_water: capyctl_store::events::EventCursor {
                    incarnation: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                    sequence: 0,
                },
            })
        }
    }
    let (release, receiver) = std::sync::mpsc::channel();
    let source = Arc::new(Blocked {
        entered: tokio::sync::Notify::new(),
        release: std::sync::Mutex::new(receiver),
    });
    let verifier = SessionVerifier::open(
        ManagementCredentials::from_trusted_resolver(ADMIN, INFERENCE).unwrap(),
        Store::open_in_memory().unwrap(),
    )
    .unwrap();
    let router = compose(
        verifier,
        Some(PUBLIC),
        [
            read_only_router_with_event_options(source.clone(), EventStreamOptions::default())
                .unwrap(),
        ],
    );
    let session = sign_in(&router, ADMIN).await;
    let pending = tokio::spawn(
        from(&router, REMOTE).oneshot(
            browser("GET", "/management/v1/events", &session.token)
                .body(Body::empty())
                .unwrap(),
        ),
    );
    source.entered.notified().await;
    let response = from(&router, LOOPBACK)
        .oneshot(
            Request::post(format!("/management/v1/ui/sessions/{}/revoke", session.id))
                .header("authorization", format!("Bearer {ADMIN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), pending).await;
    // Always release the blocking worker, including on a regression failure.
    release.send(()).unwrap();
    assert_eq!(result.unwrap().unwrap().unwrap().status(), 401);
}

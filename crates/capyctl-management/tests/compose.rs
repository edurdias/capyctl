//! ADR 0040 §§2–6: both roles' composed management routers, built through
//! `compose::standalone` and `compose::server` as the roles build them.
use axum::{
    body::{to_bytes, Body},
    extract::connect_info::MockConnectInfo,
    http::Request,
    Router,
};
use capyctl_controller::{
    agent_sessions::AgentSessions,
    coordinator::{CoordinatorError, CoordinatorOptions, OwnedCoordinator},
    enrollment::EnrollmentAuthority,
    installation_gate::EmbeddedInstallations,
    OwnedCoordinatorState,
};
use capyctl_management::{
    actions::OwnedActionSource,
    compose::{self, ManagementPeer, ServerRoutes, StandaloneRoutes},
    configuration::SharedConfigurationSource,
    engine_log::{RemoteEngineLogs, StandaloneEngineLogs},
    hosts::StandaloneHost,
    inference_listener::InferenceListenerView,
    metrics::{LatencySource, LoadRead, LoadSource},
    ui_sessions::SessionVerifier,
    ManagementCredentials,
};
use capyctl_store::Store;
use capyctl_testkit::fixture;
use futures::StreamExt;
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;

const ADMIN: &str = "management-credential-012345678901234567890";
const INFERENCE: &str = "inference-credential-0123456789012345678901";
const HOST: &str = "box.lan:7443";

struct Observations(Vec<capyctl_domain::resources::MemoryObservation>);
impl capyctl_controller::coordinator::ServiceObservation for Observations {
    fn observe(&self, _: String) -> capyctl_controller::coordinator::ObservationFuture {
        let values = self.0.clone();
        Box::pin(async move { Ok(values) })
    }
}

struct Roles {
    _dir: tempfile::TempDir,
    _worker: OwnedCoordinator,
    id: String,
    standalone: Router,
    server: Router,
}

fn verifier(dir: &std::path::Path, name: &str) -> Arc<SessionVerifier> {
    SessionVerifier::open(
        ManagementCredentials::from_trusted_resolver(ADMIN, INFERENCE).unwrap(),
        Store::open(&dir.join(name)).unwrap(),
    )
    .unwrap()
}

/// Both roles' composed routers over one owned coordinator.
async fn roles() -> Roles {
    let source = fixture::owned_source().await;
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let worker = capyctl_testkit::spawn_fake_coordinator(
        owner.clone(),
        Arc::new(Observations(source.observations.clone())),
        Arc::new(|| Ok::<_, CoordinatorError>(1900)),
        CoordinatorOptions {
            poll_interval: Duration::from_millis(10),
            ..Default::default()
        },
    )
    .unwrap();
    let document: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let host = document["input"]["host"].clone();
    let configuration =
        Arc::new(SharedConfigurationSource::new(owner.clone(), host.clone(), "owner").unwrap());
    let actions = Arc::new(OwnedActionSource::new(configuration, worker.commands()).unwrap());
    let latency: Arc<dyn LatencySource> = Arc::new(|_: Option<&str>| json!({"api_version": "1"}));
    let load: Arc<dyn LoadSource> =
        Arc::new(|_: Option<&str>| LoadRead::Report(json!({"api_version": "1"})));
    let installations = EmbeddedInstallations::new();
    let standalone = compose::standalone(
        verifier(dir.path(), "standalone-sessions.sqlite3"),
        Some("https://box.tail1234.ts.net"),
        StandaloneRoutes {
            ui: Router::new().route("/ui/", axum::routing::get(|| async { "sign in" })),
            actions: actions.clone(),
            check_sources: Arc::new(capyctl_management::check::UnavailableSources),
            host_names: vec!["lab".into(), "standalone".into()],
            installations: installations.clone(),
            engine_logs: StandaloneEngineLogs::new(owner.clone(), dir.path().join("logs")),
            owner: owner.clone(),
            host: StandaloneHost {
                host_id: "lab".into(),
                document: Arc::new(move || host.clone()),
                installations,
                domains: Arc::new(|| Box::pin(async { Vec::new() })),
                memory_warnings: Arc::new(std::sync::OnceLock::from(Vec::new())),
            },
            latency: latency.clone(),
            load: load.clone(),
            inference_listener: Arc::new(InferenceListenerView::default()),
        },
    );
    let authority = Arc::new(EnrollmentAuthority::new(
        owner.clone(),
        capyctl_agent::identity::CertificateAuthority::generate(100).unwrap(),
    ));
    let sessions = AgentSessions::new(authority.clone());
    let server = compose::server(
        verifier(dir.path(), "server-sessions.sqlite3"),
        None,
        ServerRoutes {
            ui: Router::new().route("/ui/", axum::routing::get(|| async { "sign in" })),
            actions,
            presence: Arc::new(|_: &str| true),
            enrollment: authority,
            bootstrap_address: "https://controller.example:7444".into(),
            control_address: "https://controller.example:7445".into(),
            engine_logs: RemoteEngineLogs::new(owner.clone(), sessions.clone(), "srv".into()),
            owner,
            sessions,
            latency,
            load,
            inference_listener: Arc::new(InferenceListenerView::default()),
        },
    )
    .unwrap();
    Roles {
        id: source.fence.deployment_id.clone(),
        _dir: dir,
        _worker: worker,
        standalone,
        server,
    }
}

fn from(router: &Router, peer: &str) -> Router {
    router.clone().layer(MockConnectInfo(ManagementPeer {
        peer: peer.parse().unwrap(),
        local: Some("192.0.2.1:7443".parse().unwrap()),
    }))
}

struct Session {
    id: String,
    token: String,
}

async fn sign_in(router: &Router) -> Session {
    let response = from(router, "127.0.0.1:50000")
        .oneshot(
            Request::post("/management/v1/ui/sign-in-links")
                .header("authorization", format!("Bearer {ADMIN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    let link: Value = serde_json::from_slice(&body).unwrap();
    let response = from(router, "198.51.100.7:50000")
        .oneshot(
            Request::post("/management/v1/ui/sessions")
                .header("host", HOST)
                .header("x-capyctl-ui", "1")
                .header("origin", format!("http://{HOST}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"token": link["token"]}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let cookie = response.headers()["set-cookie"].to_str().unwrap();
    let token = cookie
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("capyctl_session=")
        .unwrap()
        .to_owned();
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    Session {
        id: value["session"]["id"].as_str().unwrap().to_owned(),
        token,
    }
}

#[derive(Clone, Copy)]
enum As {
    Nobody,
    Admin,
    Session,
}

/// One route's request as `who` sends it: the CLI on the box with the admin
/// token, or the UI page from another machine with its cookie.
fn request(method: &str, uri: &str, body: Option<&Value>, who: As, token: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", HOST);
    match who {
        As::Nobody => {}
        As::Admin => request = request.header("authorization", format!("Bearer {ADMIN}")),
        As::Session => {
            request = request
                .header("x-capyctl-ui", "1")
                .header("cookie", format!("capyctl_session={token}"));
            if method != "GET" {
                request = request
                    .header("origin", format!("http://{HOST}"))
                    .header("sec-fetch-site", "same-origin");
            }
        }
    }
    match body {
        Some(body) => request
            .header("content-type", "application/json")
            .header("idempotency-key", "compose-table")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => request.body(Body::empty()).unwrap(),
    }
}

async fn status(router: &Router, request: Request<Body>, who: As) -> u16 {
    let peer = match who {
        As::Admin => "127.0.0.1:50000",
        As::Nobody | As::Session => "198.51.100.7:50000",
    };
    let response = from(router, peer).oneshot(request).await.unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    response.status().as_u16()
}

// ADR 0040 §2: every route of both roles' composed routers refuses a request
// with no credential and answers its usual status to a browser session, the
// same status the admin token gets. Only the session exchange is
// unauthenticated, and only link issuing needs the admin token.
#[tokio::test]
async fn every_route_of_both_roles_needs_a_principal() {
    let roles = roles().await;
    let id = &roles.id;
    // ADR 0040 §13: public assets do not weaken any management route below.
    for router in [&roles.standalone, &roles.server] {
        let response = router
            .clone()
            .oneshot(Request::get("/ui/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
    let invalid = json!({});
    let invitation = json!({"host_name": "host-a", "lifetime_seconds": 300});
    let common: Vec<(&str, String, Option<&Value>)> = vec![
        ("GET", "/management/v1/snapshot".into(), None),
        ("GET", "/management/v1/events".into(), None),
        (
            "POST",
            format!("/management/v1/deployments/{id}/actions"),
            Some(&invalid),
        ),
        (
            "POST",
            format!("/management/v1/deployments/{id}/instances/0/actions"),
            Some(&invalid),
        ),
        ("POST", "/management/v1/deployments".into(), Some(&invalid)),
        (
            "POST",
            "/management/v1/deployments/check".into(),
            Some(&invalid),
        ),
        (
            "PUT",
            format!("/management/v1/deployments/{id}"),
            Some(&invalid),
        ),
        (
            "GET",
            format!("/management/v1/deployments/{id}/effective-config"),
            None,
        ),
        ("GET", "/management/v1/model-sources".into(), None),
        (
            "POST",
            "/management/v1/hosts/lab/drain".into(),
            Some(&invalid),
        ),
        (
            "GET",
            format!("/management/v1/deployments/{id}/engine-log"),
            None,
        ),
        ("GET", "/management/v1/hosts".into(), None),
        ("GET", "/management/v1/engines".into(), None),
        ("GET", "/management/v1/metrics/latency".into(), None),
        ("GET", "/management/v1/metrics/load".into(), None),
        ("GET", "/management/v1/inference-listener".into(), None),
        ("GET", "/management/v1/ui/session".into(), None),
        ("GET", "/management/v1/ui/sessions".into(), None),
        (
            "POST",
            "/management/v1/ui/sessions/01J00000000000000000000000/revoke".into(),
            None,
        ),
        ("POST", "/management/v1/ui/sessions/revoke-all".into(), None),
        ("POST", "/management/v1/ui/sign-out".into(), None),
        ("POST", "/management/v1/ui/sign-in-links".into(), None),
        ("GET", "/management/v1/no-such-route".into(), None),
    ];
    let mut standalone = common.clone();
    standalone.push(("GET", "/management/v1/installation".into(), None));
    let mut server = common;
    server.push((
        "POST",
        "/management/v1/host-invitations".into(),
        Some(&invitation),
    ));
    server.push((
        "POST",
        "/management/v1/hosts/no-such-host/revoke".into(),
        None,
    ));

    for (router, routes) in [(&roles.standalone, standalone), (&roles.server, server)] {
        for (method, uri, body) in routes {
            let nobody = status(
                router,
                request(method, &uri, body, As::Nobody, ""),
                As::Nobody,
            )
            .await;
            assert_eq!(nobody, 401, "{method} {uri}");
            let admin = status(
                router,
                request(method, &uri, body, As::Admin, ""),
                As::Admin,
            )
            .await;
            // A fresh session per route: sign-out and revoke-all end theirs.
            let session = sign_in(router).await;
            let browser = status(
                router,
                request(method, &uri, body, As::Session, &session.token),
                As::Session,
            )
            .await;
            if uri.ends_with("/sign-in-links") {
                assert_eq!((admin, browser), (201, 403), "{method} {uri}");
            } else {
                assert!(admin != 401 && admin != 403, "{method} {uri}: {admin}");
                assert_eq!(browser, admin, "{method} {uri}");
            }
        }
        // The exchange runs without a principal, under the request checks.
        let exchange = status(
            router,
            request(
                "POST",
                "/management/v1/ui/sessions",
                Some(&invalid),
                As::Nobody,
                "",
            ),
            As::Nobody,
        )
        .await;
        assert_eq!(exchange, 403);
    }
}

async fn events(router: &Router, session: &Session) -> axum::body::BodyDataStream {
    let response = from(router, "198.51.100.7:50000")
        .oneshot(request(
            "GET",
            "/management/v1/events",
            None,
            As::Session,
            &session.token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.into_body().into_data_stream()
}

async fn drained(stream: &mut axum::body::BodyDataStream) {
    while let Some(chunk) = stream.next().await {
        chunk.unwrap();
    }
}

// ADR 0040 §6: revoking session S ends S's open event stream within a
// second, on both compositions, while another session's stream continues.
#[tokio::test]
async fn revoking_a_session_ends_its_event_stream() {
    let roles = roles().await;
    for router in [&roles.standalone, &roles.server] {
        let revoked = sign_in(router).await;
        let kept = sign_in(router).await;
        let mut revoked_stream = events(router, &revoked).await;
        let mut kept_stream = events(router, &kept).await;
        let response = from(router, "127.0.0.1:50000")
            .oneshot(request(
                "POST",
                &format!("/management/v1/ui/sessions/{}/revoke", revoked.id),
                None,
                As::Admin,
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        tokio::time::timeout(Duration::from_secs(1), drained(&mut revoked_stream))
            .await
            .expect("the revoked session's stream ends within a second");
        assert!(
            tokio::time::timeout(Duration::from_millis(1500), drained(&mut kept_stream))
                .await
                .is_err(),
            "the other session's stream continues"
        );
    }
}

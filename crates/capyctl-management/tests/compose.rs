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
    host_admin::UnavailableHostAdmin,
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
    roles_with_sources(Arc::new(capyctl_management::check::UnavailableSources)).await
}

async fn roles_with_sources(
    check_sources: Arc<dyn capyctl_management::check::SourceProvider>,
) -> Roles {
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
    {
        let owned = owner.lock().unwrap();
        owned
            .store()
            .create_stopped_managed_configuration(
                owned.session(),
                "owner",
                "compose-source",
                &json!({"config":{"name":"source-model","engine":"vllm","model":"/weights"}})
                    .to_string(),
                &host,
                1900,
            )
            .unwrap();
        owned.store().create_stopped_managed_configuration(
            owned.session(), "owner", "compose-secret-source",
            &json!({"config":{"name":"secret-source","engine":"vllm","model":{"source":{
                "type":"huggingface","repo":"org/model","revision":"0123456789abcdef0123456789abcdef01234567","token_ref":"secret://private-hub"
            }}}}).to_string(), &host, 1900,
        ).unwrap();
    }
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
            playground: capyctl_management::playground::router(Router::new()),
            actions: actions.clone(),
            check_sources,
            host_admin: Arc::new(UnavailableHostAdmin),
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
    let host_admin = capyctl_management::host_admin_remote::RemoteHostAdmin::new(
        owner.clone(),
        sessions.clone(),
    );
    let server = compose::server(
        verifier(dir.path(), "server-sessions.sqlite3"),
        None,
        ServerRoutes {
            ui: Router::new().route("/ui/", axum::routing::get(|| async { "sign in" })),
            playground: capyctl_management::playground::router(Router::new()),
            actions,
            check_sources: host_admin.clone(),
            host_admin,
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
    let must_exist = request.uri().path() != "/management/v1/no-such-route";
    let uri = request.uri().clone();
    let peer = match who {
        As::Admin => "127.0.0.1:50000",
        As::Nobody | As::Session => "198.51.100.7:50000",
    };
    let response = from(router, peer).oneshot(request).await.unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status().as_u16();
    if must_exist && status == 404 {
        // T21 / ADR 0040 §2: a matched handler may return 404 (for example
        // the empty inference fixture), but an unsupported method must still
        // find its route. Equal fallback 404s cannot satisfy this table.
        let probe = Request::builder()
            .method("OPTIONS")
            .uri(uri)
            .header("authorization", format!("Bearer {ADMIN}"))
            .body(Body::empty())
            .unwrap();
        let probe = from(router, "127.0.0.1:50000")
            .oneshot(probe)
            .await
            .unwrap();
        assert_eq!(probe.status(), 405, "missing route in authorization table");
    }
    status
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
            "POST",
            "/management/v1/playground/chat/completions".into(),
            Some(&invalid),
        ),
        (
            "POST",
            "/management/v1/playground/systemone".into(),
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
        (
            "GET",
            format!("/management/v1/deployments/{id}/source"),
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
        (
            "GET",
            "/management/v1/hosts/lab/engines/detected?path=%2Fopt%2Fengines".into(),
            None,
        ),
        (
            "POST",
            "/management/v1/hosts/lab/engines".into(),
            Some(&invalid),
        ),
        (
            "POST",
            "/management/v1/hosts/lab/engines/local/remove".into(),
            Some(&invalid),
        ),
        (
            "POST",
            "/management/v1/hosts/lab/custom-engines/preview".into(),
            Some(&invalid),
        ),
        (
            "POST",
            "/management/v1/hosts/lab/custom-engines".into(),
            Some(&invalid),
        ),
        ("GET", "/management/v1/hosts/lab/model-store".into(), None),
        (
            "POST",
            "/management/v1/hosts/lab/model-store/prune".into(),
            Some(&invalid),
        ),
        ("GET", "/management/v1/hosts/lab/config".into(), None),
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

// T21 / U7 / ADR 0040 §9: both roles expose accepted, editable source, not effective config.
#[tokio::test]
async fn accepted_source_is_editable_and_uses_a_numeric_revision_on_both_roles() {
    let roles = roles().await;
    for router in [&roles.standalone, &roles.server] {
        let response = from(router, "127.0.0.1:50000")
            .oneshot(request(
                "GET",
                "/management/v1/deployments/source-model/source",
                None,
                As::Admin,
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap())
                .unwrap();
        assert!(body["revision"].is_i64(), "{body}");
        assert_eq!(body["config"]["name"], "source-model");
        assert_eq!(body["config"]["model"]["path"], "/weights");
        assert!(body["config"].get("selected_devices").is_none(), "{body}");
        assert!(body["config"].get("host").is_none(), "{body}");
        assert_eq!(
            capyctl_config::parse_strict(
                capyctl_config::ConfigKind::Deployment,
                body["yaml"].as_str().unwrap()
            )
            .unwrap(),
            body["config"]
        );
        let response = from(router, "127.0.0.1:50000")
            .oneshot(request(
                "GET",
                "/management/v1/deployments/secret-source/source",
                None,
                As::Admin,
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("private-hub"));
        let redacted: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            redacted["config"]["model"]["source"]["token_ref"],
            "[redacted]"
        );
        let response = from(router, "127.0.0.1:50000")
            .oneshot(request(
                "GET",
                "/management/v1/deployments/missing/source",
                None,
                As::Admin,
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "deployment_not_found");
    }
}

// T21 / U7 / KTD7: revoke ends only the wait, not dispatched host work.
#[tokio::test]
async fn revocation_ends_check_wait_without_cancelling_host_work() {
    use capyctl_management::check::PinFuture;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let completed = Arc::new(tokio::sync::Notify::new());
    let signals = (entered.clone(), release.clone(), completed.clone());
    let sources = Arc::new(
        move |_: &capyctl_store::managed_configuration::HostTarget, _: Vec<String>| -> PinFuture {
            let (entered, release, completed) = signals.clone();
            Box::pin(async move {
                entered.notify_one();
                release.notified().await;
                completed.notify_one();
                Ok(Vec::new())
            })
        },
    );
    let roles = roles_with_sources(sources).await;
    let router = &roles.standalone;
    let session = sign_in(router).await;
    let body = json!({"config":{"name":"check","engine":"vllm","model":"/weights"}});
    let pending = tokio::spawn(from(router, "198.51.100.7:50000").oneshot(request(
        "POST",
        "/management/v1/deployments/check",
        Some(&body),
        As::Session,
        &session.token,
    )));
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    assert_eq!(
        status(
            router,
            request(
                "POST",
                &format!("/management/v1/ui/sessions/{}/revoke", session.id),
                None,
                As::Admin,
                ""
            ),
            As::Admin
        )
        .await,
        200
    );
    let response = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), 401);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), completed.notified())
        .await
        .unwrap();
}

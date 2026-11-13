//! U11 / ADR 0040 §§13–14: fixed assets, independent of the local web build.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    response::Response,
    Router,
};
use capyctl_webui::{router_with_assets, Assets};
use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

const INDEX: &[u8] = b"<!doctype html><title>Fixed CapyCTL UI</title>";
const SCRIPT: &[u8] = b"console.log('fixed bundle');";
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

struct Fixed;
impl Assets for Fixed {
    fn get(path: &str) -> Option<Cow<'static, [u8]>> {
        match path {
            "index.html" => Some(Cow::Borrowed(INDEX)),
            "assets/app-a1b2c3d4.js" => Some(Cow::Borrowed(SCRIPT)),
            "licenses.txt" => Some(Cow::Borrowed(b"Third-party notices")),
            _ => None,
        }
    }
}
struct Empty;
impl Assets for Empty {
    fn get(_: &str) -> Option<Cow<'static, [u8]>> {
        None
    }
}
async fn get(router: Router, path: &str) -> Response {
    router
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}
async fn bytes(response: Response) -> Vec<u8> {
    to_bytes(response.into_body(), 4096).await.unwrap().to_vec()
}
fn security_headers(response: &Response) {
    assert_eq!(response.headers()["content-security-policy"], CSP);
    assert_eq!(response.headers()["x-frame-options"], "DENY");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["referrer-policy"], "same-origin");
}

// U11: both entry redirects use the canonical bundle root.
#[tokio::test]
async fn root_and_ui_redirect_to_ui_slash() {
    for path in ["/", "/ui"] {
        let response = get(router_with_assets::<Fixed>(), path).await;
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(response.headers()["location"], "/ui/");
    }
}

// U11 / KTD18: reloads of client routes get the same uncached index.
#[tokio::test]
async fn extensionless_paths_serve_index_without_caching() {
    for path in ["/ui/", "/ui/index.html", "/ui/models/abc"] {
        let response = get(router_with_assets::<Fixed>(), path).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-cache");
        assert_eq!(response.headers()["content-type"], "text/html");
        assert_eq!(bytes(response).await, INDEX);
    }
}

// U11 / KTD18: Vite names these assets with content hashes.
#[tokio::test]
async fn hashed_assets_are_immutable_for_a_year() {
    let response = get(router_with_assets::<Fixed>(), "/ui/assets/app-a1b2c3d4.js").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .contains("javascript"));
    assert_eq!(bytes(response).await, SCRIPT);
}

// U11 / KTD17: notices ship with the bundle, not through the API.
#[tokio::test]
async fn third_party_notices_are_served() {
    let response = get(router_with_assets::<Fixed>(), "/ui/licenses.txt").await;
    assert_eq!(response.status(), 200);
    assert_eq!(bytes(response).await, b"Third-party notices");
}

// U11 / KTD18: errors, redirects and disallowed methods are UI responses too.
#[tokio::test]
async fn every_ui_response_has_security_headers() {
    let router = router_with_assets::<Fixed>();
    for path in [
        "/",
        "/ui",
        "/ui/",
        "/ui/models/abc",
        "/ui/assets/app-a1b2c3d4.js",
        "/ui/licenses.txt",
        "/ui/missing.js",
        "/ui/%FF",
        "/ui/../management/v1/snapshot",
        "/ui/%2e%2e/management/v1/snapshot",
    ] {
        security_headers(&get(router.clone(), path).await);
    }
    let response = router
        .oneshot(Request::post("/ui/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 405);
    security_headers(&response);
    security_headers(&get(router_with_assets::<Empty>(), "/ui/").await);
}

// U11 / KTD17: an asset-free checkout is still a usable CLI/server build.
#[tokio::test]
async fn empty_bundle_explains_no_ui_with_status_ok() {
    let response = get(router_with_assets::<Empty>(), "/ui/").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-cache");
    let body = String::from_utf8(bytes(response).await).unwrap();
    assert!(body.contains("This build has no web UI"));
}

// U11 / ADR 0040 §13: decoded parent segments never reach another namespace.
#[tokio::test]
async fn traversal_cannot_escape_the_asset_root() {
    let router = router_with_assets::<Fixed>().route(
        "/management/v1/snapshot",
        axum::routing::get(|| async { "private snapshot" }),
    );
    for path in [
        "/ui/../management/v1/snapshot",
        "/ui/%2e%2e/management/v1/snapshot",
        "/ui/%2E%2E%2Fmanagement/v1/snapshot",
    ] {
        let response = get(router.clone(), path).await;
        assert_eq!(response.status(), 404, "{path}");
        assert!(!String::from_utf8(bytes(response).await)
            .unwrap()
            .contains("private snapshot"));
    }
}

// U11 / ADR 0040 §§2, 13: build the two real role compositions, not a stand-in
// auth layer. Management remains closed and static cache headers survive it.
#[tokio::test]
async fn both_roles_compose_public_ui_and_authorized_management_without_panicking() {
    use capyctl_controller::{
        agent_sessions::AgentSessions,
        coordinator::{CoordinatorOptions, OwnedCoordinator},
        enrollment::EnrollmentAuthority,
        installation_gate::EmbeddedInstallations,
        OwnedCoordinatorState,
    };
    use capyctl_management::{
        actions::OwnedActionSource,
        compose::{self, ServerRoutes, StandaloneRoutes},
        configuration::SharedConfigurationSource,
        engine_log::{RemoteEngineLogs, StandaloneEngineLogs},
        hosts::StandaloneHost,
        inference_listener::InferenceListenerView,
        metrics::{LatencySource, LoadRead, LoadSource},
        ui_sessions::SessionVerifier,
        ManagementCredentials,
    };
    use serde_json::{json, Value};
    use std::{os::unix::fs::PermissionsExt, time::Duration};

    let source = capyctl_testkit::fixture::owned_source().await;
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    struct NoObservations;
    impl capyctl_controller::coordinator::ServiceObservation for NoObservations {
        fn observe(&self, _: String) -> capyctl_controller::coordinator::ObservationFuture {
            Box::pin(async { Ok(Vec::new()) })
        }
    }
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let database = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &database).unwrap();
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let worker: OwnedCoordinator = capyctl_testkit::spawn_fake_coordinator(
        owner.clone(),
        Arc::new(NoObservations),
        Arc::new(|| Ok(1900)),
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
    let verifier = |name: &str| {
        SessionVerifier::open(
            ManagementCredentials::from_trusted_resolver(
                "management-credential-012345678901234567890",
                "inference-credential-0123456789012345678901",
            )
            .unwrap(),
            capyctl_store::Store::open(&dir.path().join(name)).unwrap(),
        )
        .unwrap()
    };
    let installations = EmbeddedInstallations::new();
    let standalone = compose::standalone(
        verifier("standalone.sqlite3"),
        None,
        StandaloneRoutes {
            ui: router_with_assets::<Fixed>(),
            playground: axum::Router::new(),
            actions: actions.clone(),
            check_sources: Arc::new(capyctl_management::check::UnavailableSources),
            host_admin: Arc::new(capyctl_management::host_admin::UnavailableHostAdmin),
            host_names: vec!["lab".into()],
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
        verifier("server.sqlite3"),
        None,
        ServerRoutes {
            ui: router_with_assets::<Fixed>(),
            playground: axum::Router::new(),
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
    for router in [standalone, server] {
        let response = get(router.clone(), "/ui/").await;
        assert_eq!(response.status(), 200);
        security_headers(&response);
        assert_eq!(response.headers()["cache-control"], "no-cache");
        assert_eq!(bytes(response).await, INDEX);
        let response = get(router.clone(), "/ui/assets/app-a1b2c3d4.js").await;
        assert_eq!(
            response.headers()["cache-control"],
            "public, max-age=31536000, immutable"
        );
        for path in ["/management/v1/snapshot", "/management/v1/no-such-route"] {
            assert_eq!(get(router.clone(), path).await.status(), 401);
        }
        for path in [
            "/ui/../management/v1/snapshot",
            "/ui/%2e%2e/management/v1/snapshot",
        ] {
            assert_eq!(get(router.clone(), path).await.status(), 404);
        }
    }
}

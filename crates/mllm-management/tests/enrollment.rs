use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use mllm_controller::{enrollment::EnrollmentAuthority, OwnedCoordinatorState};
use mllm_management::{enrollment::enrollment_router, ManagementCredentials};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;
// T05 T37: invitation issuance and revocation require the independent admin credential.
#[tokio::test]
async fn enrollment_mutations_require_admin_and_return_bounded_join_material() {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let owned = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let ca = mllm_agent::identity::CertificateAuthority::generate(100).unwrap();
    let authority = Arc::new(EnrollmentAuthority::new(owned, ca));
    let admin = "a".repeat(32);
    let inference = "b".repeat(32);
    let router = enrollment_router(
        ManagementCredentials::from_trusted_resolver(&admin, &inference).unwrap(),
        authority,
        "https://controller.example:7444".into(),
        "https://controller.example:7445".into(),
    )
    .unwrap();
    for token in [None, Some(inference.as_str())] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/management/v1/host-invitations")
            .header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(
                request
                    .body(Body::from(
                        r#"{"host_name":"host-a","lifetime_seconds":300}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/management/v1/host-invitations")
                .header("authorization", format!("Bearer {admin}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"host_name":"host-a","lifetime_seconds":300}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["cache-control"], "no-store");
}

// T05 T06 T37: failures follow SPEC §14's sanitized, typed envelope and never cache join material.
#[tokio::test]
async fn enrollment_errors_are_structured_and_queries_methods_rejected() {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let owned = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let authority = Arc::new(EnrollmentAuthority::new(
        owned,
        mllm_agent::identity::CertificateAuthority::generate(100).unwrap(),
    ));
    let admin = "a".repeat(32);
    let router = enrollment_router(
        ManagementCredentials::from_trusted_resolver(&admin, &"b".repeat(32)).unwrap(),
        authority,
        "https://controller.example:7444".into(),
        "https://controller.example:7445".into(),
    )
    .unwrap();
    for (method, uri, authorized, body, status, code) in [
        (
            "POST",
            "/management/v1/host-invitations",
            false,
            "{}",
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
        ),
        (
            "POST",
            "/management/v1/host-invitations?secret=do-not-echo",
            true,
            "{}",
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "GET",
            "/management/v1/host-invitations",
            true,
            "",
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
        ),
        (
            "POST",
            "/management/v1/missing",
            true,
            "",
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            "POST",
            "/management/v1/host-invitations",
            true,
            "{invalid-sensitive-body",
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "POST",
            "/management/v1/hosts/missing/revoke",
            true,
            "",
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        // SPEC §14: typed errors. A malformed host id, an out-of-range
        // lifetime or an invalid host name is the request's fault (400).
        (
            "POST",
            "/management/v1/hosts/bad%20id/revoke",
            true,
            "",
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "POST",
            "/management/v1/host-invitations",
            true,
            r#"{"host_name":"host-a","lifetime_seconds":0}"#,
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "POST",
            "/management/v1/host-invitations",
            true,
            r#"{"host_name":"bad name!","lifetime_seconds":300}"#,
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
    ] {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if authorized {
            request = request.header("authorization", format!("Bearer {admin}"));
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], code);
        assert!(value["error"]["retryable"].is_boolean());
        assert!(value["error"]["message"].is_string());
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("do-not-echo"));
        assert!(!text.contains("invalid-sensitive-body"));
    }
}

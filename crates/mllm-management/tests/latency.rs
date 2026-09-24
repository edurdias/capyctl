//! SPEC §17 (M80): `GET /management/v1/metrics/latency` is authenticated,
//! accepts one bounded `deployment` filter and serves the composed report.
use mllm_management::{metrics::latency_router, ManagementCredentials};
use std::sync::Arc;
use tower::ServiceExt;

const MANAGEMENT: &str = "management-token-0123456789abcdefghijklmnop";
const INFERENCE: &str = "inference-token-0123456789abcdefghijklmnopq";

fn get(uri: &str, token: Option<&str>) -> axum::http::Request<axum::body::Body> {
    let mut request = axum::http::Request::builder().method("GET").uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request.body(axum::body::Body::empty()).unwrap()
}

// SPEC §17 T38: the report is served only to the management credential; the
// filter reaches the source; anything else in the query is refused.
#[tokio::test]
async fn latency_view_is_authenticated_and_filtered() {
    let router = latency_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(
            |deployment: Option<&str>| serde_json::json!({"deployments": [], "filter": deployment}),
        ),
    );
    let denied = router
        .clone()
        .oneshot(get("/management/v1/metrics/latency", Some(INFERENCE)))
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let all = router
        .clone()
        .oneshot(get("/management/v1/metrics/latency", Some(MANAGEMENT)))
        .await
        .unwrap();
    assert_eq!(all.status(), 200);
    assert_eq!(all.headers()["cache-control"], "no-store");
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(all.into_body(), 1 << 16)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(body["filter"].is_null());
    let one = router
        .clone()
        .oneshot(get(
            "/management/v1/metrics/latency?deployment=dep_1",
            Some(MANAGEMENT),
        ))
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(one.into_body(), 1 << 16)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["filter"], "dep_1");
    let bad = router
        .oneshot(get(
            "/management/v1/metrics/latency?deployment=a%20b",
            Some(MANAGEMENT),
        ))
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

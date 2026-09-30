//! Design §9: `GET /management/v1/inference-listener` reports the inference
//! listener's effective bind and whether it requires the API key, so status
//! can repeat the start warning. It is served only to the management
//! credential.
use capyctl_management::{
    inference_listener::{inference_listener_router, InferenceListenerView},
    ManagementCredentials,
};
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

async fn body(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap(),
    )
    .unwrap()
}

// T37 (design §9): the view is authenticated, null until the role binds, and
// then names the bind and authentication.
#[tokio::test]
async fn the_inference_listener_view_is_authenticated() {
    let view = Arc::new(InferenceListenerView::default());
    let router = inference_listener_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        view.clone(),
    );
    const PATH: &str = "/management/v1/inference-listener";
    for token in [None, Some(INFERENCE)] {
        let denied = router.clone().oneshot(get(PATH, token)).await.unwrap();
        assert_eq!(denied.status(), 401, "{token:?}");
    }
    let before = router
        .clone()
        .oneshot(get(PATH, Some(MANAGEMENT)))
        .await
        .unwrap();
    assert_eq!(before.status(), 200);
    assert_eq!(before.headers()["cache-control"], "no-store");
    assert!(body(before).await["inference_listener"].is_null());
    view.set(serde_json::json!({"bind": "0.0.0.0:8443", "authenticated": false}));
    let after = router
        .clone()
        .oneshot(get(PATH, Some(MANAGEMENT)))
        .await
        .unwrap();
    assert_eq!(
        body(after).await["inference_listener"],
        serde_json::json!({"bind": "0.0.0.0:8443", "authenticated": false})
    );
    let query = router
        .clone()
        .oneshot(get(&format!("{PATH}?x=1"), Some(MANAGEMENT)))
        .await
        .unwrap();
    assert_eq!(query.status(), 400);
}

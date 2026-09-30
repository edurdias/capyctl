use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use capyctl_management::{snapshot_router, ManagementCredentials, StoreSnapshotSource};
use capyctl_store::Store;
use std::sync::Arc;
use tower::ServiceExt;

const MANAGEMENT: &str = "management-credential-012345678901234567890";
const INFERENCE: &str = "inference-credential-0123456789012345678901";

#[tokio::test]
async fn loopback_http_preserves_seeded_sqlite_cursor_without_new_session() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let store = Store::open_in_memory().unwrap();
    store.begin_coordinator_session().unwrap();
    let expected = store.snapshot().unwrap().cursor.to_string();
    let router = snapshot_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(StoreSnapshotSource::new(store)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    struct Stop(tokio::task::JoinHandle<()>);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server = Stop(server);
    for _ in 0..2 {
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client.write_all(format!("GET /management/v1/snapshot HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {MANAGEMENT}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.take(100_000).read_to_end(&mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        let (_, body) = text.split_once("\r\n\r\n").unwrap();
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["cursor"], expected);
        assert_eq!(value["session_epoch"], "1");
        assert!(!text.contains(MANAGEMENT));
    }
}

fn app() -> axum::Router {
    snapshot_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(StoreSnapshotSource::new(Store::open_in_memory().unwrap())),
    )
}

#[tokio::test]
async fn independent_auth_and_partial_snapshot_contract() {
    for key in [None, Some(INFERENCE), Some("bad")] {
        let mut request = Request::builder().uri("/management/v1/snapshot");
        if let Some(key) = key {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        let response = app()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["api_version"], "1");
        assert_eq!(value["error"]["code"], "unauthenticated");
    }
    let response = app()
        .oneshot(
            Request::builder()
                .uri("/management/v1/snapshot")
                .header("authorization", format!("Bearer {MANAGEMENT}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(value["scope"], "durable_store_foundation");
    assert_eq!(value["session_epoch"], "0");
    assert!(value["cursor"].is_string());
}

#[test]
fn credentials_must_be_distinct_and_strict() {
    assert!(ManagementCredentials::from_trusted_resolver(MANAGEMENT, MANAGEMENT).is_err());
    for bad in [
        "",
        "short",
        " credential-0123456789012345678901234567",
        "credential-0123456789012345678901234567\n",
    ] {
        assert!(ManagementCredentials::from_trusted_resolver(bad, INFERENCE).is_err());
        assert!(ManagementCredentials::from_trusted_resolver(MANAGEMENT, bad).is_err());
    }
}

#[tokio::test]
async fn auth_precedes_source_and_errors_never_include_provider_details() {
    use capyctl_management::{SnapshotSource, SnapshotUnavailable};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Failure(AtomicUsize);
    impl SnapshotSource for Failure {
        fn snapshot(&self) -> Result<capyctl_store::snapshot::Snapshot, SnapshotUnavailable> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(SnapshotUnavailable)
        }
    }
    let source = Arc::new(Failure(AtomicUsize::new(0)));
    let router = snapshot_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        source.clone(),
    );
    for auth in [
        format!("Bearer {INFERENCE}"),
        format!("Basic {MANAGEMENT}"),
        format!("Bearer  {MANAGEMENT}"),
        format!("Bearer {MANAGEMENT}, Bearer {MANAGEMENT}"),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/management/v1/snapshot")
                    .header("authorization", auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }
    assert_eq!(source.0.load(Ordering::SeqCst), 0);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/management/v1/snapshot")
                .header("authorization", format!("Bearer {MANAGEMENT}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    let body =
        String::from_utf8(to_bytes(response.into_body(), 4096).await.unwrap().to_vec()).unwrap();
    assert!(!body.contains(MANAGEMENT));
    assert!(!body.contains(INFERENCE));
    assert_eq!(source.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_reads_retain_bounded_worker_capacity() {
    use capyctl_management::{SnapshotSource, SnapshotUnavailable};
    use std::sync::{Condvar, Mutex};
    struct Blocked {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        release: (Mutex<bool>, Condvar),
    }
    impl SnapshotSource for Blocked {
        fn snapshot(&self) -> Result<capyctl_store::snapshot::Snapshot, SnapshotUnavailable> {
            self.entered.send(()).unwrap();
            let (lock, wake) = &self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            Err(SnapshotUnavailable)
        }
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let source = Arc::new(Blocked {
        entered: tx,
        release: (Mutex::new(false), Condvar::new()),
    });
    // Always release actual workers, including on assertion failure.
    struct Release(Arc<Blocked>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.release.0.lock().unwrap() = true;
            self.0.release.1.notify_all();
        }
    }
    let _release = Release(source.clone());
    let router = snapshot_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        source,
    );
    let request = || {
        Request::builder()
            .uri("/management/v1/snapshot")
            .header("authorization", format!("Bearer {MANAGEMENT}"))
            .body(Body::empty())
            .unwrap()
    };
    let first = tokio::spawn(router.clone().oneshot(request()));
    let second = tokio::spawn(router.clone().oneshot(request()));
    for _ in 0..2 {
        tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
    first.abort();
    second.abort();
    let response = router.oneshot(request()).await.unwrap();
    assert_eq!(response.status(), 429);
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(value["error"]["code"], "queue_full");
    assert_eq!(value["error"]["retryable"], true);
}

#[tokio::test]
async fn query_duplicate_auth_and_control_routes_fail_closed() {
    for (method, uri, duplicate, status) in [
        ("GET", "/management/v1/snapshot?ignored=1", false, 400),
        ("GET", "/management/v1/snapshot", true, 401),
        ("POST", "/management/v1/snapshot", false, 405),
        ("POST", "/management/v1/deployments", false, 404),
        ("GET", "/v1/models", false, 404),
        ("GET", "/management/v1/events", false, 404),
    ] {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {MANAGEMENT}"));
        if duplicate {
            request = request.header("authorization", format!("Bearer {MANAGEMENT}"));
        }
        let response = app()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{method} {uri}");
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(value["api_version"], "1");
    }
}

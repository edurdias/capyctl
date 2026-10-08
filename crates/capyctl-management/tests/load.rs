//! SPEC §§10, 17 (owner decision 2026-10-08): `GET /management/v1/metrics/load`
//! is authenticated, accepts one bounded `deployment` filter, answers an ID
//! that names no deployment `404 not_found`, and serves the report the router
//! composes from its counts and the host-reported samples, fresh or stale.
//! CPU tests with in-memory doubles; they never qualify a native engine.
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use capyctl_config::context_fit::{MaxRunning, MaxRunningSource};
use capyctl_controller::load_table::{LoadTable, LOAD_STALE_AFTER_MS};
use capyctl_management::metrics::{load_router, LoadRead};
use capyctl_management::ManagementCredentials;
use capyctl_protocol::reports::{EngineLoad, LoadReport, LoadSample};
use capyctl_router::admission::InFlight;
use capyctl_store::snapshot::{DeploymentCapacity, InstanceCapacity};
use tower::ServiceExt;

const MANAGEMENT: &str = "management-token-0123456789abcdefghijklmnop";
const INFERENCE: &str = "inference-token-0123456789abcdefghijklmnopq";
const NOW: i64 = 1_800_000_000_000;

fn get(uri: &str, token: Option<&str>) -> axum::http::Request<axum::body::Body> {
    let mut request = axum::http::Request::builder().method("GET").uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request.body(axum::body::Body::empty()).unwrap()
}

async fn read(router: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(get(uri, Some(MANAGEMENT)))
        .await
        .unwrap();
    let status = response.status().as_u16();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

/// The service's composition: the deployments the store names (here fixed),
/// the router's counts and the host samples, read at the test's clock.
fn router(clock: Arc<AtomicI64>) -> axum::Router {
    let deployments = [DeploymentCapacity {
        id: "dep-1".into(),
        name: "chat".into(),
        max_running: Some(MaxRunning {
            count: Some(32),
            source: MaxRunningSource::Default,
            reason: None,
        }),
        instances: vec![InstanceCapacity {
            index: 0,
            host_id: Some("h1".into()),
            generation: Some(3),
            observed_state: "ready".into(),
        }],
    }];
    let inflight = Arc::new(InFlight::default());
    inflight.increment("dep-1");
    let loads = Arc::new(LoadTable::new());
    loads
        .accept(
            "h1",
            LoadReport {
                host_id: "h1".into(),
                samples: vec![LoadSample {
                    deployment_id: "dep-1".into(),
                    generation: 3,
                    owned_handle: "launch-3".into(),
                    sampled_at_ms: NOW,
                    ingress_in_flight: 1,
                    engine: Some(EngineLoad {
                        running: 1,
                        waiting: 4,
                        kv_usage_ppm: 90_000,
                    }),
                    latency: None,
                    max_running: Some(16),
                }],
            },
            NOW,
        )
        .unwrap();
    load_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(move |deployment: Option<&str>| {
            let selected: Vec<_> = deployments
                .iter()
                .filter(|d| deployment.is_none_or(|id| d.id == id))
                .cloned()
                .collect();
            if deployment.is_some() && selected.is_empty() {
                return LoadRead::UnknownDeployment;
            }
            LoadRead::Report(capyctl_router::capacity::capacity_report(
                &selected,
                &inflight,
                32,
                Some(loads.as_ref()),
                clock.load(Ordering::SeqCst),
            ))
        }),
    )
}

// SPEC §§10, 17, T38: only the management credential reads it; a malformed
// filter is a 400; an unknown deployment is a 404 `not_found`.
#[tokio::test]
async fn the_load_read_is_authenticated_filtered_and_names_unknown_deployments() {
    let router = router(Arc::new(AtomicI64::new(NOW)));
    for token in [None, Some(INFERENCE)] {
        let denied = router
            .clone()
            .oneshot(get("/management/v1/metrics/load", token))
            .await
            .unwrap();
        assert_eq!(denied.status(), 401);
    }
    let (status, unknown) = read(&router, "/management/v1/metrics/load?deployment=nope").await;
    assert_eq!(status, 404, "{unknown}");
    assert_eq!(unknown["error"]["code"], "not_found");
    assert_eq!(unknown["error"]["message"], "Deployment not found");
    for bad in [
        "deployment=",
        "deployment=a%20b",
        "other=1",
        "deployment=a&x=1",
    ] {
        let (status, _) = read(&router, &format!("/management/v1/metrics/load?{bad}")).await;
        assert_eq!(status, 400, "{bad}");
    }
    let (status, all) = read(&router, "/management/v1/metrics/load").await;
    assert_eq!(status, 200);
    assert_eq!(all["deployments"][0]["deployment_id"], "dep-1");
}

// SPEC §§10, 17: a fresh sample reads with its age, the engine's running and
// waiting requests and its own running limit; past 3 s the same sample reads
// stale, never re-dated; the router's figures are live either way.
#[tokio::test]
async fn the_load_read_serves_fresh_then_stale_samples() {
    let clock = Arc::new(AtomicI64::new(NOW + 250));
    let router = router(clock.clone());
    let (status, fresh) = read(&router, "/management/v1/metrics/load?deployment=dep-1").await;
    assert_eq!(status, 200);
    let deployment = &fresh["deployments"][0];
    assert_eq!(deployment["router"]["in_flight"], 1);
    assert_eq!(deployment["router"]["in_flight_limit"], 32);
    assert_eq!(deployment["router"]["waiting"], 0);
    let instance = &deployment["instances"][0];
    assert_eq!(
        instance["max_running"],
        serde_json::json!({"count": 16, "source": "engine"})
    );
    let sample = &instance["sample"];
    assert_eq!(sample["fresh"], true);
    assert_eq!(sample["age_ms"], 250);
    assert_eq!(sample["sampled_at_ms"], NOW);
    assert_eq!(
        sample["engine"],
        serde_json::json!({"running": 1, "waiting": 4, "kv_usage_ppm": 90_000})
    );

    clock.store(NOW + LOAD_STALE_AFTER_MS + 1_000, Ordering::SeqCst);
    let (_, stale) = read(&router, "/management/v1/metrics/load?deployment=dep-1").await;
    let sample = &stale["deployments"][0]["instances"][0]["sample"];
    assert_eq!(sample["fresh"], false, "{sample}");
    assert_eq!(sample["age_ms"], LOAD_STALE_AFTER_MS + 1_000);
    assert_eq!(sample["sampled_at_ms"], NOW);
    assert_eq!(stale["stale_after_ms"], LOAD_STALE_AFTER_MS);
}

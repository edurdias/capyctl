//! SPEC §§10, 17 (owner decision 2026-10-08): the management load read joins
//! the router's counts, the host-reported samples and the running limit
//! CapyCTL derives. CPU tests with in-memory doubles; they never qualify a
//! native engine.
use std::sync::Arc;

use capyctl_config::context_fit::{MaxRunning, MaxRunningSource};
use capyctl_controller::load_table::{LoadTable, LOAD_RETAIN_MS, LOAD_STALE_AFTER_MS};
use capyctl_protocol::reports::{EngineLoad, LoadReport, LoadSample};
use capyctl_router::admission::InFlight;
use capyctl_router::capacity::capacity_report;
use capyctl_router::queue::WaitLimits;
use capyctl_store::snapshot::{DeploymentCapacity, InstanceCapacity};
use serde_json::json;

const NOW: i64 = 1_800_000_000_000;

fn deployment() -> DeploymentCapacity {
    DeploymentCapacity {
        id: "dep-1".into(),
        name: "chat".into(),
        max_running: Some(MaxRunning {
            count: Some(32),
            source: MaxRunningSource::Default,
            reason: None,
        }),
        instances: vec![
            InstanceCapacity {
                index: 0,
                host_id: Some("h1".into()),
                generation: Some(7),
                observed_state: "ready".into(),
            },
            // Its sample arrives from another host than the one it is placed on.
            InstanceCapacity {
                index: 1,
                host_id: Some("h2".into()),
                generation: Some(8),
                observed_state: "ready".into(),
            },
            InstanceCapacity {
                index: 2,
                host_id: None,
                generation: None,
                observed_state: "parked".into(),
            },
        ],
    }
}

fn sample(generation: i64, max_running: Option<u32>) -> LoadSample {
    LoadSample {
        deployment_id: "dep-1".into(),
        generation,
        owned_handle: format!("launch-{generation}"),
        sampled_at_ms: NOW,
        ingress_in_flight: 2,
        engine: Some(EngineLoad {
            running: 3,
            waiting: 5,
            kv_usage_ppm: 410_000,
        }),
        latency: None,
        max_running,
    }
}

// T19 T34, SPEC §§10, 17: per deployment, the router's in-flight and waiting
// requests with their bounds; per instance, the latest sample of exactly that
// incarnation from its own host, with its age and freshness, and the engine's
// reported running limit in place of the derived one. A fresh sample turns
// stale after 3 s and is gone after 30 s; then the derived limit is shown and
// the sample is null, never zeros.
#[test]
fn the_load_read_reports_fresh_then_stale_then_no_sample() {
    let inflight = Arc::new(InFlight::with_wait_limits(WaitLimits {
        max_pending_per_deployment: 0,
        ..Default::default()
    }));
    inflight.increment("dep-1");
    inflight.increment("dep-1");
    let loads = LoadTable::new();
    loads
        .accept(
            "h1",
            LoadReport {
                host_id: "h1".into(),
                samples: vec![sample(7, Some(24)), sample(8, Some(99))],
            },
            NOW,
        )
        .unwrap();
    let deployments = [deployment()];

    let report = capacity_report(&deployments, &inflight, 32, Some(&loads), NOW + 500);
    assert_eq!(report["observed_at_ms"], NOW + 500);
    assert_eq!(report["stale_after_ms"], LOAD_STALE_AFTER_MS);
    let d = &report["deployments"][0];
    assert_eq!(d["deployment_id"], "dep-1");
    assert_eq!(
        d["router"],
        json!({"in_flight": 2, "in_flight_limit": 32, "waiting": 0, "waiting_limit": 0})
    );
    assert_eq!(d["max_running"], json!({"count": 32, "source": "default"}));
    let fresh = &d["instances"][0];
    assert_eq!(fresh["generation"], 7);
    assert_eq!(fresh["router_in_flight"], 0);
    assert_eq!(
        fresh["max_running"],
        json!({"count": 24, "source": "engine"})
    );
    assert_eq!(
        fresh["sample"],
        json!({"sampled_at_ms": NOW, "age_ms": 500, "fresh": true, "ingress_in_flight": 2,
               "engine": {"running": 3, "waiting": 5, "kv_usage_ppm": 410_000}})
    );
    // Another host's claim on an instance placed elsewhere is not its load.
    let other = &d["instances"][1];
    assert_eq!(other["sample"], json!(null));
    assert_eq!(
        other["max_running"],
        json!({"count": 32, "source": "default"})
    );
    let parked = &d["instances"][2];
    assert_eq!(parked["sample"], json!(null));
    assert_eq!(parked["generation"], json!(null));

    let stale = capacity_report(
        &deployments,
        &inflight,
        32,
        Some(&loads),
        NOW + LOAD_STALE_AFTER_MS + 1,
    );
    let sample = &stale["deployments"][0]["instances"][0]["sample"];
    assert_eq!(sample["fresh"], false, "{sample}");
    assert_eq!(sample["age_ms"], LOAD_STALE_AFTER_MS + 1);
    assert_eq!(
        sample["engine"]["running"], 3,
        "a stale figure is still shown"
    );

    let gone = capacity_report(
        &deployments,
        &inflight,
        32,
        Some(&loads),
        NOW + LOAD_RETAIN_MS + 1,
    );
    let instance = &gone["deployments"][0]["instances"][0];
    assert_eq!(instance["sample"], json!(null));
    assert_eq!(
        instance["max_running"],
        json!({"count": 32, "source": "default"})
    );
}

// SPEC §10, §17: an engine scrape that failed is unknown load (`engine: null`),
// and a role with no host load report (standalone) shows no sample at all.
#[test]
fn unknown_load_is_null_never_zero() {
    let inflight = InFlight::default();
    let loads = LoadTable::new();
    let mut unscraped = sample(7, None);
    unscraped.engine = None;
    loads
        .accept(
            "h1",
            LoadReport {
                host_id: "h1".into(),
                samples: vec![unscraped],
            },
            NOW,
        )
        .unwrap();
    let deployments = [deployment()];
    let report = capacity_report(&deployments, &inflight, 32, Some(&loads), NOW);
    let instance = &report["deployments"][0]["instances"][0];
    assert_eq!(instance["sample"]["engine"], json!(null));
    assert_eq!(instance["sample"]["fresh"], true);
    assert_eq!(instance["max_running"]["source"], "default");
    assert_eq!(report["deployments"][0]["router"]["waiting_limit"], 64);

    let standalone = capacity_report(&deployments, &inflight, 32, None, NOW);
    assert_eq!(
        standalone["deployments"][0]["instances"][0]["sample"],
        json!(null)
    );
    assert_eq!(
        capacity_report(&[], &inflight, 32, None, NOW)["deployments"],
        json!([])
    );
}

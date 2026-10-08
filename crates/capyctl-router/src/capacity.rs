//! SPEC §§10, 17 (owner decision 2026-10-08): the live conditions of each
//! deployment, for `GET /management/v1/metrics/load`.
//!
//! The report joins three reads, none of which touches an engine: the
//! deployments, their instances and the running limit CapyCTL derives (the
//! store, [`capyctl_store::snapshot::Store::capacity`]); the router's own
//! in-flight and waiting counts; and the latest load sample each host reported
//! for an instance (the controller's in-memory load table). Status
//! distinguishes configured capacity from observation (SPEC §17): the
//! engine's own running limit replaces the derived one only where the engine
//! reported it, and a missing sample or gauge is `null`, never zero. A sample
//! is a routing hint (SPEC §10): nothing here admits, releases or proves.
use capyctl_config::context_fit::MaxRunning;
use capyctl_controller::load_table::{InstanceKey, LoadTable, LoadView, LOAD_STALE_AFTER_MS};
use capyctl_store::snapshot::{DeploymentCapacity, InstanceCapacity};
use serde_json::{json, Value};

use crate::admission::InFlight;

/// The report for `deployments`, read at `now_ms` on the controller clock.
/// `loads` is `None` where no host reports load (the standalone role's
/// embedded engine); `in_flight_limit` is the router's per-deployment bound.
pub fn capacity_report(
    deployments: &[DeploymentCapacity],
    inflight: &InFlight,
    in_flight_limit: usize,
    loads: Option<&LoadTable>,
    now_ms: i64,
) -> Value {
    let waiting_limit = inflight.waiting.limits().max_pending_per_deployment;
    let deployments: Vec<Value> = deployments
        .iter()
        .map(|deployment| {
            let id = deployment.id.as_str();
            let instances: Vec<Value> = deployment
                .instances
                .iter()
                .map(|instance| {
                    instance_report(
                        id,
                        instance,
                        deployment.max_running.as_ref(),
                        inflight,
                        loads,
                        now_ms,
                    )
                })
                .collect();
            json!({
                "deployment_id": id,
                "name": deployment.name,
                "router": {
                    "in_flight": inflight.current(id),
                    "in_flight_limit": in_flight_limit,
                    "waiting": inflight.waiting.waiting(id),
                    "waiting_limit": waiting_limit,
                },
                "max_running": deployment.max_running,
                "instances": instances,
            })
        })
        .collect();
    json!({
        "observed_at_ms": now_ms,
        "stale_after_ms": LOAD_STALE_AFTER_MS,
        "deployments": deployments,
    })
}

fn instance_report(
    deployment: &str,
    instance: &InstanceCapacity,
    derived: Option<&MaxRunning>,
    inflight: &InFlight,
    loads: Option<&LoadTable>,
    now_ms: i64,
) -> Value {
    // T18, T34: a sample counts only for this incarnation, as reported by the
    // host the instance is placed on.
    let sample = match (loads, instance.generation, instance.host_id.as_deref()) {
        (Some(loads), Some(generation), Some(host)) => loads
            .sample_at(&InstanceKey::new(deployment, generation), now_ms)
            .filter(|view| view.host_id == host),
        _ => None,
    };
    let max_running = match sample.as_ref().and_then(|view| view.max_running) {
        Some(reported) => Some(MaxRunning::reported(reported)),
        None => derived.cloned(),
    };
    json!({
        "index": instance.index,
        "host_id": instance.host_id,
        "generation": instance.generation,
        "observed_state": instance.observed_state,
        "router_in_flight": instance
            .generation
            .map_or(0, |generation| inflight.instance_in_flight(deployment, generation)),
        "max_running": max_running,
        "sample": sample.as_ref().map(sample_report),
    })
}

fn sample_report(view: &LoadView) -> Value {
    json!({
        "sampled_at_ms": view.sampled_at_ms,
        "age_ms": view.age_ms,
        "fresh": view.fresh,
        "ingress_in_flight": view.ingress_in_flight,
        "engine": view.engine.map(|engine| json!({
            "running": engine.running,
            "waiting": engine.waiting,
            "kv_usage_ppm": engine.kv_usage_ppm,
        })),
    })
}

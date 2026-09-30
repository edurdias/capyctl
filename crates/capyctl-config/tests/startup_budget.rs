//! Owner decision 2026-09-23: the startup memory budget. A deployment declares
//! its startup peak as `engine_config.memory.startup`, or a placeholder
//! default applies until a first run measures it; admission reserves it as the
//! cold phase until Ready. CPU-only tests; none of this is qualification of a
//! native engine recipe, and the placeholder is not a measurement.

use capyctl_config::effective::{
    decode_effective_snapshot, default_startup_bytes, deployment_command_fingerprint,
    resolve_effective, resolve_effective_with_checkpoint, startup_budget, CheckpointFacts,
    StartupProvenance, VLLM_OVERHEAD_MARGIN_BYTES,
};
use capyctl_config::{parse_strict, ConfigKind};
use capyctl_domain::launch::SettingSource;
use serde_json::{json, Value};

const GIB: i64 = 1 << 30;

fn fixture() -> (Value, Value) {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    let (mut deployment, host) = (all["deployment"].clone(), all["host"].clone());
    deployment.as_object_mut().unwrap().remove("resources");
    (deployment, host)
}

fn weights(bytes: i64) -> CheckpointFacts {
    CheckpointFacts {
        weights_bytes: Some(bytes),
        ..Default::default()
    }
}

/// The engine's CUDA context and graphs, charged beside the request in every
/// active derived phase (re-review parity rule).
const OVERHEAD: i64 = capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES;

fn cold(effective: &capyctl_config::effective::EffectiveDeployment) -> i64 {
    effective.resources.cold.allocations[0].bytes
}

fn ready(effective: &capyctl_config::effective::EffectiveDeployment) -> i64 {
    effective.resources.ready.allocations[0].bytes
}

/// A declared startup peak is the cold phase; Ready stays the request. The
/// effective configuration says it was declared, the strict walk accepts the
/// field, the command identity changes with it, and a snapshot re-derives it.
// T14 T26
#[test]
fn a_declared_startup_peak_is_the_cold_phase_until_ready() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["memory"] =
        json!({"request": "40GiB", "kv_cache": "8GiB", "startup": "70GiB"});
    parse_strict(ConfigKind::Deployment, &deployment.to_string()).unwrap();
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(cold(&effective), 70 * GIB + OVERHEAD);
    assert_eq!(ready(&effective), 40 * GIB + OVERHEAD);
    assert_eq!(
        effective.resources.parking.allocations[0].bytes,
        40 * GIB + OVERHEAD
    );
    assert_eq!(
        effective.resources.wake.allocations[0].bytes,
        40 * GIB + OVERHEAD
    );
    assert_eq!(
        effective.engine_config.memory().startup_bytes,
        Some(70 * GIB)
    );
    assert!(!effective
        .engine_config
        .provenance()
        .contains_key("memory.startup"));
    let budget = startup_budget(&effective);
    assert_eq!(budget.bytes, 70 * GIB + OVERHEAD);
    assert_eq!(budget.provenance, StartupProvenance::Declared);
    let snapshot = serde_json::to_string(&effective).unwrap();
    assert_eq!(decode_effective_snapshot(&snapshot).unwrap(), effective);

    let mut undeclared = deployment.clone();
    undeclared["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "8GiB"});
    assert_ne!(
        deployment_command_fingerprint(&deployment, 300_000).unwrap(),
        deployment_command_fingerprint(&undeclared, 300_000).unwrap()
    );
}

/// A startup peak below the steady request, not positive, or beside a
/// declared `resources:` block (whose cold phase is the peak) is refused.
// T14
#[test]
fn an_impossible_startup_peak_is_refused() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["memory"] =
        json!({"request": "40GiB", "kv_cache": "8GiB", "startup": "30GiB"});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.memory.startup");
    assert!(error.to_string().contains("below"), "{error}");

    deployment["engine_config"]["memory"]["startup"] = json!("0B");
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.memory.startup");

    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    let mut declared = all["deployment"].clone();
    declared["engine_config"]["memory"]["startup"] = json!("20GiB");
    let error = resolve_effective(&declared, &all["host"]).unwrap_err();
    assert_eq!(error.path, "engine_config.memory.startup");
    assert!(error.to_string().contains("cold"), "{error}");

    let (mut bad, host) = fixture();
    bad["engine_config"]["memory"] =
        json!({"request": "40GiB", "kv_cache": "8GiB", "startup": "twelve"});
    assert!(resolve_effective(&bad, &host).is_err());
    assert!(parse_strict(ConfigKind::Deployment, &bad.to_string()).is_err());
}

/// Undeclared, the startup peak is the placeholder `max(request, weights ×
/// 1.6 + margin)` when the weights are known and the request otherwise; the
/// provenance names it derived so a snapshot cannot claim another value.
// T14 T26 T29
#[test]
fn an_undeclared_startup_peak_is_the_placeholder_default() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "8GiB"});
    let effective =
        resolve_effective_with_checkpoint(&deployment, &host, weights(30 * GIB)).unwrap();
    let expected = 30 * GIB * 8 / 5 + VLLM_OVERHEAD_MARGIN_BYTES;
    assert_eq!(expected, 56 * GIB);
    assert_eq!(cold(&effective), expected + OVERHEAD);
    assert_eq!(ready(&effective), 40 * GIB + OVERHEAD);
    assert_eq!(
        effective.engine_config.provenance().get("memory.startup"),
        Some(&SettingSource::Derived)
    );
    assert_eq!(
        startup_budget(&effective).provenance,
        StartupProvenance::Default
    );
    let snapshot = serde_json::to_value(&effective).unwrap();
    assert_eq!(
        decode_effective_snapshot(&snapshot.to_string()).unwrap(),
        effective
    );
    let mut forged = snapshot.clone();
    forged["engine_config"]["memory"]["startup_bytes"] = json!(41 * GIB);
    forged["resources"]["cold"]["allocations"][0]["bytes"] = json!(41 * GIB);
    assert!(decode_effective_snapshot(&forged.to_string()).is_err());

    // Small weights: the request is already above the placeholder.
    let small = resolve_effective_with_checkpoint(&deployment, &host, weights(GIB)).unwrap();
    assert_eq!(cold(&small), 40 * GIB + OVERHEAD);
    // Unknown weights: the request.
    let unknown = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(cold(&unknown), 40 * GIB + OVERHEAD);
    assert_eq!(
        default_startup_bytes(40 * GIB, None, 8 * GIB),
        Some(40 * GIB)
    );
    assert_eq!(
        default_startup_bytes(40 * GIB, Some(i64::MAX), 8 * GIB),
        None
    );
}

/// A revision frozen before the startup budget has no startup peak: it still
/// decodes, its cold phase stays the request, and status names that.
// T14 T34
#[test]
fn a_revision_frozen_before_the_budget_still_decodes_with_its_request() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "8GiB"});
    // What the old release wrote: no startup peak, no CUDA context charge.
    let old = resolve_effective_with_checkpoint(
        &deployment,
        &host,
        CheckpointFacts {
            weights_bytes: Some(30 * GIB),
            legacy_startup: true,
            legacy_overhead: true,
        },
    )
    .unwrap();
    let legacy = serde_json::to_value(&old).unwrap();
    assert!(legacy["engine_config"]["memory"]
        .get("startup_bytes")
        .is_none());
    assert!(legacy["engine_config"]["memory"]
        .get("overhead_bytes")
        .is_none());
    let decoded = decode_effective_snapshot(&legacy.to_string()).unwrap();
    assert_eq!(decoded, old);
    assert_eq!(cold(&decoded), 40 * GIB);
    assert_eq!(decoded.engine_config.memory().startup_bytes, None);
    assert_eq!(
        startup_budget(&decoded).provenance,
        StartupProvenance::Request
    );
}

/// A declared `resources:` block states its own cold phase; status says so.
// T14
#[test]
fn a_declared_cold_phase_is_the_startup_budget() {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    let effective = resolve_effective(&all["deployment"], &all["host"]).unwrap();
    assert_eq!(effective.engine_config.memory().startup_bytes, None);
    let budget = startup_budget(&effective);
    assert_eq!(budget.provenance, StartupProvenance::Resources);
    assert_eq!(budget.bytes, cold(&effective));
}

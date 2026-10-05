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

/// Undeclared, the startup peak is the placeholder `max(request + graphs,
/// weights × 2.25 + margin)` (ADR 0014 amendment A8) when the weights are known and the request otherwise; the
/// provenance names it derived so a snapshot cannot claim another value.
// T14 T26 T29
#[test]
fn an_undeclared_startup_peak_is_the_placeholder_default() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "8GiB"});
    let effective =
        resolve_effective_with_checkpoint(&deployment, &host, weights(30 * GIB)).unwrap();
    let expected = 30 * GIB * 9 / 4 + VLLM_OVERHEAD_MARGIN_BYTES;
    assert_eq!(expected, 75 * GIB + GIB / 2);
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

    // Small weights: the request and the graph allowance (amendment A8) are
    // already above the placeholder.
    let small = resolve_effective_with_checkpoint(&deployment, &host, weights(GIB)).unwrap();
    assert_eq!(cold(&small), 40 * GIB + GRAPHS + OVERHEAD);
    // Unknown weights: the request and the graph allowance.
    let unknown = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(cold(&unknown), 40 * GIB + GRAPHS + OVERHEAD);
    assert_eq!(
        default_startup_bytes(40 * GIB, None, 8 * GIB, Some(GRAPHS)),
        Some(40 * GIB + GRAPHS)
    );
    assert_eq!(
        default_startup_bytes(40 * GIB, Some(i64::MAX), 8 * GIB, Some(GRAPHS)),
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
            legacy_startup_graphs: true,
            legacy_device_margin: true,
            legacy_sglang_graphs_off: false,
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

/// ADR 0014 amendment A8: the first-start graph allowance per captured model.
const GRAPHS: i64 = 5 << 28;

/// The fixture deployment on `engine`, with a draft model when `draft` names
/// one, and a declared request so the request dominates the placeholder.
fn speculative(engine: &str, draft: bool) -> (Value, Value) {
    let (mut deployment, mut host) = fixture();
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = json!(engine);
    profile["args"] = json!([]);
    profile["security"]["approved_options"] =
        json!(["--speculative-config", "--speculative-draft-model-path"]);
    profile["security"]["approved_paths"] = json!(["/srv/drafters"]);
    if engine == "sglang" {
        profile["security"]["admin_credential_ref"] = json!("secret://engine-admin");
    }
    deployment["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "8GiB"});
    if draft {
        deployment["engine_config"]["accept_extra_args"] = json!(true);
        deployment["engine_config"]["extra_args"] = match engine {
            "vllm" => json!([
                "--speculative-config",
                r#"{"method":"dflash","model":"/srv/drafters/d","num_speculative_tokens":7}"#
            ]),
            _ => json!([
                "--speculative-draft-model-path",
                "/srv/drafters/d",
                "--speculative-algorithm",
                "DFLASH"
            ]),
        };
        // Pinned to restarting, as the A8 measurement ran (amendment A17 lets
        // a speculative SGLang deployment park with its weights resident).
        if engine == "sglang" {
            deployment["residency"] = json!("restart_only");
        }
    }
    (deployment, host)
}

/// ADR 0014 amendment A8 (found live 2026-10-02): vLLM 0.30 with Qwen3.8-27B
/// NVFP4 and DFlash2 peaked at 50.46 GiB on its first start against a
/// 49.25 GiB cold phase: the CUDA graphs it captured (1.64 GiB, the draft
/// model's included) sat above the request. The placeholder startup now
/// carries one graph allowance per captured model, on vLLM and SGLang, and
/// a snapshot records and re-derives it. Ready is unchanged.
// T14 T26
#[test]
fn the_placeholder_startup_covers_the_first_starts_graphs() {
    for engine in ["vllm", "sglang"] {
        for (draft, models) in [(false, 1), (true, 2)] {
            let (deployment, host) = speculative(engine, draft);
            let effective =
                resolve_effective_with_checkpoint(&deployment, &host, weights(GIB)).unwrap();
            assert_eq!(
                cold(&effective),
                40 * GIB + models * GRAPHS + OVERHEAD,
                "{engine} draft {draft}"
            );
            assert_eq!(ready(&effective), 40 * GIB + OVERHEAD);
            let memory = effective.engine_config.memory();
            assert_eq!(memory.startup_bytes, Some(40 * GIB + models * GRAPHS));
            assert_eq!(memory.startup_graphs_bytes, Some(models * GRAPHS));
            let snapshot = serde_json::to_value(&effective).unwrap();
            assert_eq!(
                decode_effective_snapshot(&snapshot.to_string()).unwrap(),
                effective
            );
            // A claimed allowance other than the derived one is refused.
            let mut forged = snapshot.clone();
            forged["engine_config"]["memory"]["startup_graphs_bytes"] = json!(0);
            assert!(decode_effective_snapshot(&forged.to_string()).is_err());
        }
    }
    // Large weights: the load term still wins when it is larger.
    let (deployment, host) = speculative("vllm", true);
    let heavy = resolve_effective_with_checkpoint(&deployment, &host, weights(30 * GIB)).unwrap();
    assert_eq!(cold(&heavy), 75 * GIB + GIB / 2 + OVERHEAD);
    // A declared startup peak is the operator's and carries no allowance.
    let (mut declared, host) = speculative("vllm", true);
    declared["engine_config"]["memory"]["startup"] = json!("45GiB");
    let declared = resolve_effective(&declared, &host).unwrap();
    assert_eq!(cold(&declared), 45 * GIB + OVERHEAD);
    assert_eq!(declared.engine_config.memory().startup_graphs_bytes, None);
}

/// ADR 0014 amendment A8: a revision frozen before the graph allowance
/// records none; it still decodes, with the placeholder it was frozen with.
// T14 T34
#[test]
fn a_revision_frozen_before_the_graph_allowance_keeps_its_placeholder() {
    let (deployment, host) = speculative("vllm", true);
    // What the previous release wrote: no allowance, and no record of one.
    let frozen = CheckpointFacts {
        weights_bytes: Some(GIB),
        legacy_startup_graphs: true,
        legacy_device_margin: true,
        legacy_sglang_graphs_off: false,
        ..Default::default()
    };
    let old = resolve_effective_with_checkpoint(&deployment, &host, frozen).unwrap();
    let snapshot = serde_json::to_value(&old).unwrap();
    assert!(snapshot["engine_config"]["memory"]
        .get("startup_graphs_bytes")
        .is_none());
    assert_eq!(
        snapshot["engine_config"]["memory"]["startup_bytes"],
        json!(40 * GIB)
    );
    let decoded = decode_effective_snapshot(&snapshot.to_string()).unwrap();
    assert_eq!(decoded, old);
    assert_eq!(cold(&decoded), 40 * GIB + OVERHEAD);
    // With heavy weights it keeps the 1.6 factor it was frozen with.
    let frozen = CheckpointFacts {
        weights_bytes: Some(30 * GIB),
        legacy_startup_graphs: true,
        legacy_device_margin: true,
        legacy_sglang_graphs_off: false,
        ..Default::default()
    };
    let old = resolve_effective_with_checkpoint(&deployment, &host, frozen).unwrap();
    assert_eq!(cold(&old), 56 * GIB + OVERHEAD);
    let snapshot = serde_json::to_value(&old).unwrap();
    assert_eq!(
        decode_effective_snapshot(&snapshot.to_string()).unwrap(),
        old
    );
    assert_eq!(
        default_startup_bytes(40 * GIB, Some(30 * GIB), 8 * GIB, None),
        Some(56 * GIB)
    );
}

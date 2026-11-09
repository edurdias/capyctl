//! Owner decision 2026-09-23: the startup memory budget. A deployment declares
//! its startup peak as `engine_config.memory.startup`, or a placeholder
//! default applies until a first run measures it; admission reserves it as the
//! cold phase until Ready. CPU-only tests; none of this is qualification of a
//! native engine recipe, and the placeholder is not a measurement.

use capyctl_config::effective::{
    decode_effective_snapshot, default_startup_bytes, deployment_command_fingerprint,
    resolve_effective, resolve_effective_with_checkpoint, startup_budget, CheckpointFacts,
    EagerStartup, StartupProvenance, VLLM_EAGER_STARTUP_EXTRA_CAP_BYTES,
    VLLM_EAGER_STARTUP_WEIGHTS_FACTOR, VLLM_OVERHEAD_MARGIN_BYTES,
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
    // A declared `lazy` loader keeps amendment A8's factor; the parking
    // fixture's undeclared default is `eager` (amendment A21, note of
    // 2026-10-15), covered by `the_startup_placeholder_follows_the_effective_loader`.
    deployment["engine_config"]["vllm"] = json!({"safetensors_load_strategy": "lazy"});
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
        default_startup_bytes(40 * GIB, None, 8 * GIB, Some(GRAPHS), None),
        Some(40 * GIB + GRAPHS)
    );
    assert_eq!(
        default_startup_bytes(40 * GIB, Some(i64::MAX), 8 * GIB, Some(GRAPHS), None),
        None
    );
}

/// ADR 0014 amendment A21 (owner decision 2026-10-09; note of 2026-10-15)
/// and amendment A22 (owner decision 2026-10-18): the startup placeholder
/// sizes vLLM's eager load, `min(weights × 3.6, weights + 32 GiB)`, exactly
/// when CapyCTL will actually pass `--safetensors-load-strategy eager` to
/// vLLM, declared or defaulted (a parking deployment's undeclared default is
/// `eager` on unified memory too), never with a declared `lazy` loader on the
/// same (unified) host. The cap is recorded beside the placeholder.
// T14 T26
#[test]
fn the_startup_placeholder_follows_the_effective_loader() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "8GiB"});
    // 30 GiB × 2.6 = 78 GiB is above the cap: 30 + 32 + 8 GiB.
    let expected = 30 * GIB + VLLM_EAGER_STARTUP_EXTRA_CAP_BYTES + VLLM_OVERHEAD_MARGIN_BYTES;
    assert_eq!(expected, 70 * GIB);
    let eager_cap = Some(VLLM_EAGER_STARTUP_EXTRA_CAP_BYTES);

    // Undeclared on a parking deployment (the fixture parks on unified
    // memory): the defaulted `eager` takes the eager load.
    let defaulted =
        resolve_effective_with_checkpoint(&deployment, &host, weights(30 * GIB)).unwrap();
    assert_eq!(cold(&defaulted), expected + OVERHEAD);
    assert_eq!(
        defaulted.engine_config.memory().startup_eager_cap_bytes,
        eager_cap
    );

    deployment["engine_config"]["vllm"] = json!({"safetensors_load_strategy": "eager"});
    let eager = resolve_effective_with_checkpoint(&deployment, &host, weights(30 * GIB)).unwrap();
    assert_eq!(cold(&eager), expected + OVERHEAD);
    assert_eq!(
        eager.engine_config.memory().startup_eager_cap_bytes,
        eager_cap
    );
    assert_eq!(
        default_startup_bytes(
            40 * GIB,
            Some(30 * GIB),
            VLLM_OVERHEAD_MARGIN_BYTES,
            Some(GRAPHS),
            Some(EagerStartup::Capped(VLLM_EAGER_STARTUP_EXTRA_CAP_BYTES)),
        ),
        Some(expected)
    );
    let snapshot = serde_json::to_value(&eager).unwrap();
    assert_eq!(
        decode_effective_snapshot(&snapshot.to_string()).unwrap(),
        eager
    );
    // A claimed cap other than the derived one is refused.
    let mut forged = snapshot.clone();
    forged["engine_config"]["memory"]["startup_eager_cap_bytes"] = json!(64 * GIB);
    assert!(decode_effective_snapshot(&forged.to_string()).is_err());

    // Declared `lazy` on the same unified host keeps the lighter factor
    // (amendment A8's `STARTUP_WEIGHTS_FACTOR`, 2.25) and records no cap.
    let mut lazy = deployment.clone();
    lazy["engine_config"]["vllm"] = json!({"safetensors_load_strategy": "lazy"});
    let lazy_effective =
        resolve_effective_with_checkpoint(&lazy, &host, weights(30 * GIB)).unwrap();
    assert_eq!(
        cold(&lazy_effective),
        30 * GIB * 9 / 4 + VLLM_OVERHEAD_MARGIN_BYTES + OVERHEAD
    );
    assert_eq!(
        lazy_effective
            .engine_config
            .memory()
            .startup_eager_cap_bytes,
        None
    );
}

/// ADR 0014 amendment A22 (owner decision 2026-10-18): the eager load is
/// `weights + min(weights × 13/5, 32 GiB)`. Up to the last checkpoint whose
/// extra fits under the cap (13215283988 bytes, about 12.3 GiB) it is
/// amendment A21's `weights × 3.6` to the byte; one byte above, the cap
/// holds. A 10 GiB checkpoint resolves exactly as it did under A21.
// T14 T26
#[test]
fn the_eager_placeholder_keeps_3_6_below_the_cap_and_caps_above_it() {
    let capped = Some(EagerStartup::Capped(VLLM_EAGER_STARTUP_EXTRA_CAP_BYTES));
    let uncapped = Some(EagerStartup::Uncapped);
    let margin = VLLM_OVERHEAD_MARGIN_BYTES;
    let placeholder = |weights: i64, eager| {
        default_startup_bytes(weights, Some(weights), margin, Some(GRAPHS), eager).unwrap()
    };
    let below = 13_215_283_988;
    assert!(below * 13 / 5 <= VLLM_EAGER_STARTUP_EXTRA_CAP_BYTES);
    assert_eq!(placeholder(below, capped), placeholder(below, uncapped));
    assert_eq!(placeholder(below, capped), below * 18 / 5 + margin);
    let above = below + 1;
    assert!(above * 13 / 5 > VLLM_EAGER_STARTUP_EXTRA_CAP_BYTES);
    assert_eq!(
        placeholder(above, capped),
        above + VLLM_EAGER_STARTUP_EXTRA_CAP_BYTES + margin
    );
    assert!(placeholder(above, capped) < placeholder(above, uncapped));
    assert_eq!(
        VLLM_EAGER_STARTUP_WEIGHTS_FACTOR,
        (18, 5),
        "the factor below the cap is A21's"
    );

    // Through resolution: the parking fixture loads `eager` by default.
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "8GiB"});
    let small = resolve_effective_with_checkpoint(&deployment, &host, weights(10 * GIB)).unwrap();
    assert_eq!(cold(&small), 10 * GIB * 18 / 5 + margin + OVERHEAD);
    assert_eq!(cold(&small), 44 * GIB + OVERHEAD);
}

/// ADR 0014 amendment A22: every eager startup peak CapyCTL measured on GB10
/// (vLLM 0.30.0, `startup.measured`) sits under the capped placeholder, even
/// with the smallest request a checkpoint can have (its weights). Weights and
/// peaks in bytes; the Super VL placeholder is about 111 GiB.
// T14 T26
#[test]
fn the_capped_eager_placeholder_covers_every_measured_peak() {
    let capped = Some(EagerStartup::Capped(VLLM_EAGER_STARTUP_EXTRA_CAP_BYTES));
    for (model, weights, peak, placeholder) in [
        // Qwen3.8-27B NVFP4, KV 4 GiB, 2026-10-18.
        (
            "qwen3.8-27b-nvfp4",
            23_749_332_688,
            56_220_348_416,
            66_699_005_648,
        ),
        // Qwen3-14B BF16, KV 4 GiB, 2026-10-18.
        (
            "qwen3-14b-bf16",
            29_536_665_640,
            44_531_412_992,
            72_486_338_600,
        ),
        // Qwen3-30B-A3B BF16, KV 4 GiB, 2026-10-18.
        (
            "qwen3-30b-a3b-bf16",
            61_066_575_648,
            76_076_666_880,
            104_016_248_608,
        ),
        // Qwen3.8-27B BF16, KV 4 GiB, 2026-10-18.
        (
            "qwen3.8-27b-bf16",
            55_563_006_776,
            68_840_255_488,
            98_512_679_736,
        ),
        // Nemotron 3.5 Super VL 120B-A12B NVFP4, 2026-10-16.
        (
            "super-vl-nvfp4",
            76_328_140_836,
            101_695_578_112,
            119_277_813_796,
        ),
    ] {
        let derived = default_startup_bytes(
            weights,
            Some(weights),
            VLLM_OVERHEAD_MARGIN_BYTES,
            Some(GRAPHS),
            capped,
        )
        .unwrap();
        assert_eq!(derived, placeholder, "{model}");
        assert!(derived >= peak, "{model}: {derived} < {peak}");
    }
    // Amendment A21's own evidence, Qwen3.6-35B-A3B NVFP4 (23.5 GB): a
    // 54.2 GiB eager loading peak.
    let weights = 23_500_000_000;
    let derived = default_startup_bytes(
        weights,
        Some(weights),
        VLLM_OVERHEAD_MARGIN_BYTES,
        Some(GRAPHS),
        capped,
    )
    .unwrap();
    assert!(derived >= 54 * GIB + GIB / 5, "{derived}");
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
            state_slot_bytes: None,
            legacy_startup: true,
            legacy_overhead: true,
            legacy_startup_graphs: true,
            legacy_family_margin: true,
            legacy_sglang_graphs_off: false,
            ..Default::default()
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
    // Large weights: the load term still wins when it is larger. This parking
    // vLLM deployment loads `eager` by default (ADR 0014 amendment A21, note
    // of 2026-10-15), capped by amendment A22: 30 GiB + 32 GiB + the 8 GiB
    // margin.
    let (deployment, host) = speculative("vllm", true);
    let heavy = resolve_effective_with_checkpoint(&deployment, &host, weights(30 * GIB)).unwrap();
    assert_eq!(cold(&heavy), 70 * GIB + OVERHEAD);
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
        legacy_family_margin: true,
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
        legacy_family_margin: true,
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
        default_startup_bytes(40 * GIB, Some(30 * GIB), 8 * GIB, None, None),
        Some(56 * GIB)
    );
}

/// ADR 0014 amendment A22 (owner decision 2026-10-18): a revision frozen
/// under amendment A21 records no eager cap; it still decodes, with the
/// uncapped `weights × 3.6` placeholder it was frozen with, as amendment A8
/// let earlier revisions keep 1.6. Its first start's measurement replaces
/// it, and a new revision takes the cap. A snapshot that drops the cap but
/// keeps the capped figure is refused.
// T14 T34
#[test]
fn a_revision_frozen_before_the_eager_cap_keeps_its_placeholder() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "8GiB"});
    // What the previous release wrote: the parking fixture's default `eager`
    // at 30 GiB × 3.6 + 8 GiB, and no cap.
    let frozen = CheckpointFacts {
        weights_bytes: Some(30 * GIB),
        legacy_eager_startup: true,
        ..Default::default()
    };
    let old = resolve_effective_with_checkpoint(&deployment, &host, frozen).unwrap();
    assert_eq!(cold(&old), 116 * GIB + OVERHEAD);
    let snapshot = serde_json::to_value(&old).unwrap();
    assert!(snapshot["engine_config"]["memory"]
        .get("startup_eager_cap_bytes")
        .is_none());
    assert_eq!(
        snapshot["engine_config"]["memory"]["startup_bytes"],
        json!(30 * GIB * 18 / 5 + VLLM_OVERHEAD_MARGIN_BYTES)
    );
    let decoded = decode_effective_snapshot(&snapshot.to_string()).unwrap();
    assert_eq!(decoded, old);
    assert_eq!(cold(&decoded), 116 * GIB + OVERHEAD);
    assert_eq!(
        default_startup_bytes(
            40 * GIB,
            Some(30 * GIB),
            VLLM_OVERHEAD_MARGIN_BYTES,
            Some(GRAPHS),
            Some(EagerStartup::Uncapped),
        ),
        Some(116 * GIB)
    );

    // The same deployment resolved now takes the cap; dropping the recorded
    // cap from its snapshot does not pass for a frozen revision.
    let new = resolve_effective_with_checkpoint(&deployment, &host, weights(30 * GIB)).unwrap();
    assert_eq!(cold(&new), 70 * GIB + OVERHEAD);
    let mut forged = serde_json::to_value(&new).unwrap();
    forged["engine_config"]["memory"]
        .as_object_mut()
        .unwrap()
        .remove("startup_eager_cap_bytes");
    assert!(decode_effective_snapshot(&forged.to_string()).is_err());

    // Below the cap the frozen and the new placeholder are the same bytes.
    let small = CheckpointFacts {
        weights_bytes: Some(10 * GIB),
        legacy_eager_startup: true,
        ..Default::default()
    };
    let old_small = resolve_effective_with_checkpoint(&deployment, &host, small).unwrap();
    let new_small =
        resolve_effective_with_checkpoint(&deployment, &host, weights(10 * GIB)).unwrap();
    assert_eq!(cold(&old_small), cold(&new_small));
    let snapshot = serde_json::to_value(&old_small).unwrap();
    assert_eq!(
        decode_effective_snapshot(&snapshot.to_string()).unwrap(),
        old_small
    );
}

/// #219: the issue's 16 GB laptop card (16376 MiB, a 14.7 GiB managed limit)
/// as a discrete host: host RAM in `system`, the card in `gpu0`, an SGLang
/// profile.
fn laptop_card() -> (Value, Value) {
    let (mut deployment, mut host) = fixture();
    host["resource_policy"]["domains"] = json!({
        "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB",
                   "parked_limit": "16GiB", "host_kv_limit": "4GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "15052MiB",
                 "free_reserve": "1324MiB", "parked_limit": "2GiB"}
    });
    host["resource_policy"]["devices"] = json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "sglang".into();
    profile["args"] = json!([]);
    profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
    deployment["residency"] = json!("restart_only");
    // The issue's request: 12.86 GiB (FP8 weights of about 11.3 GiB and a
    // 16k context), declared, so acceptance resolves it before the weights
    // are known.
    deployment["engine_config"]["memory"] = json!({"request": "13169MiB", "kv_cache": "1GiB"});
    (deployment, host)
}

/// The card's part of a phase.
fn on_card(phase: &capyctl_config::effective::PhaseFootprint) -> i64 {
    phase
        .allocations
        .iter()
        .find(|a| a.domain == "gpu0")
        .map(|a| a.bytes)
        .unwrap()
}

/// #219 (ADR 0019 amendment A1): a request declared for a discrete card has
/// an undeclared startup peak equal to the request, whether or not the
/// checkpoint's weights are known when the revision is resolved. Acceptance
/// resolves a declared request without them, and the revision then took the
/// unified placeholder (request plus the first-start graph allowance), so the
/// issue's first start was charged 15.4 GiB of a card that manages 14.7 GiB
/// and could never start, although its Ready charge (14.1 GiB) fits.
// T14 T26
#[test]
fn a_request_declared_for_a_card_starts_at_its_ready_charge_before_the_weights_are_known() {
    let (deployment, host) = laptop_card();
    let request = 13169_i64 << 20;
    let limit = 15052_i64 << 20;
    for facts in [CheckpointFacts::default(), weights(11571 << 20)] {
        let effective = resolve_effective_with_checkpoint(&deployment, &host, facts).unwrap();
        assert_eq!(on_card(&effective.resources.ready), request + OVERHEAD);
        assert_eq!(
            on_card(&effective.resources.cold),
            request + OVERHEAD,
            "{facts:?}: the first start is charged the Ready figure, not the graph allowance on top"
        );
        assert!(on_card(&effective.resources.cold) <= limit);
        assert_eq!(
            effective.engine_config.memory().startup_bytes,
            Some(request)
        );
        assert_eq!(effective.engine_config.memory().startup_graphs_bytes, None);
        // An estimate, measured on the first run (ADR 0019 amendment A2).
        assert_eq!(
            startup_budget(&effective).provenance,
            StartupProvenance::Default
        );
        assert!(capyctl_config::effective::startup_measure(&effective).is_some());
        let snapshot = serde_json::to_string(&effective).unwrap();
        assert_eq!(decode_effective_snapshot(&snapshot).unwrap(), effective);
    }
}

/// #219: a revision frozen before the fix, which took the placeholder with
/// its graph allowance (it records `startup_graphs_bytes`), re-resolves
/// exactly as it was; a new revision of the same deployment gets the rule.
// T14
#[test]
fn a_card_revision_frozen_with_the_placeholder_keeps_it() {
    let (deployment, host) = laptop_card();
    let request = 13169_i64 << 20;
    // What the previous release froze: the placeholder, with its graph
    // allowance recorded.
    let frozen = CheckpointFacts {
        legacy_device_placeholder: true,
        ..Default::default()
    };
    let old = resolve_effective_with_checkpoint(&deployment, &host, frozen).unwrap();
    let snapshot = serde_json::to_value(&old).unwrap();
    assert_eq!(
        snapshot["engine_config"]["memory"]["startup_graphs_bytes"],
        json!(GRAPHS)
    );
    assert_eq!(
        snapshot["engine_config"]["memory"]["startup_bytes"],
        json!(request + GRAPHS)
    );
    let decoded = decode_effective_snapshot(&snapshot.to_string()).unwrap();
    assert_eq!(decoded, old);
    assert_eq!(
        on_card(&decoded.resources.cold),
        request + GRAPHS + OVERHEAD
    );
    // A new revision of the same deployment starts at its Ready charge, and
    // its snapshot cannot claim the placeholder.
    let new = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(on_card(&new.resources.cold), request + OVERHEAD);
    let mut forged = serde_json::to_value(&new).unwrap();
    forged["engine_config"]["memory"]["startup_bytes"] = json!(request + GRAPHS);
    assert!(decode_effective_snapshot(&forged.to_string()).is_err());
}

/// #219: a declared `memory.startup` replaces the placeholder: the card is
/// charged it and the engine's CUDA context, and nothing is added for the
/// graphs, so a startup equal to the request starts at the Ready charge.
/// A declared peak whose charge the card cannot hold is named by
/// [`capyctl_config::effective::startup_over_limit`]: such a revision can
/// never start, since a declared peak is never replaced by a measurement.
// T14 T26
#[test]
fn a_declared_startup_on_a_card_is_charged_with_the_cuda_context_only() {
    let (mut deployment, host) = laptop_card();
    let request = 13169_i64 << 20;
    let limit = 15052_i64 << 20;
    deployment["engine_config"]["memory"]["startup"] = json!("13169MiB");
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(on_card(&effective.resources.cold), request + OVERHEAD);
    assert_eq!(
        startup_budget(&effective).provenance,
        StartupProvenance::Declared
    );
    assert_eq!(
        capyctl_config::effective::startup_over_limit(&effective),
        None
    );

    deployment["engine_config"]["memory"]["startup"] = json!("14GiB");
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(on_card(&effective.resources.cold), 14 * GIB + OVERHEAD);
    assert_eq!(
        capyctl_config::effective::startup_over_limit(&effective),
        Some(capyctl_config::effective::StartupOverLimit {
            host: "lab".into(),
            domain: "gpu0".into(),
            bytes: 14 * GIB + OVERHEAD,
            limit_bytes: limit,
        })
    );
}

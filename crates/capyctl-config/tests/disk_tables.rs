//! ADR 0014 amendment A20 (owner decision 2026-10-09): when a deployment's
//! engine keeps the checkpoint's n-gram tables on disk (SGLang 0.5.21
//! `--ple-offload-backend file`, TensorFold 0.6.5 `--ple-on-ssd`), those
//! tensors count against the models disk, not memory. CPU tests only; none of
//! this qualifies either engine option.

use capyctl_config::effective::{
    decode_effective_snapshot, resolve_effective_with_checkpoint, CheckpointFacts,
    EffectiveDeployment, ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES as OVERHEAD,
};
use capyctl_config::engine_policy::{disk_table_cache_bytes, Engine};
use capyctl_config::ConfigErrorCode;
use capyctl_domain::disk_tables::{
    disk_table_weights_bytes, CheckpointTables, SGLANG_TABLE_CACHE_BYTES,
    TENSORFOLD_TABLE_CACHE_BYTES,
};
use capyctl_domain::member_weights::CheckpointLayout;
use serde_json::{json, Value};

const GIB: i64 = 1 << 30;
/// Qwen3.8-Flash-Next: 126 GiB of weights, 47.7 GiB of them one n-gram table.
const WEIGHTS: i64 = 126 * GIB;
const TABLE: i64 = 47 * GIB + 7 * GIB / 10;

fn tables() -> CheckpointTables {
    CheckpointTables {
        bytes: TABLE,
        count: 1,
        sharded_bytes: TABLE,
        resident_largest_layer_bytes: 2 * GIB,
    }
}

/// 48 layers, the table inside one of them, 2 GiB kept whole.
fn layout() -> CheckpointLayout {
    CheckpointLayout {
        sharded_bytes: 124 * GIB,
        layer_count: 48,
        largest_layer_bytes: TABLE + 2 * GIB,
    }
}

fn measured(tables: Option<CheckpointTables>) -> CheckpointFacts {
    CheckpointFacts {
        weights_bytes: Some(WEIGHTS),
        layout: Some(layout()),
        disk_tables: tables,
        ..Default::default()
    }
}

/// A 121.7 GiB GB10.
const GB10: i64 = 121 * GIB + 7 * GIB / 10;

/// The managed limit of a GB10 given to one large deployment: 112 GiB, the
/// rest left to the system. The automatic limit (about 97.4 GiB) holds the
/// share of a two-host group, not this.
fn gb10() -> i64 {
    112 * GIB
}

fn fixture() -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    host["resource_policy"]["domains"]["unified"]["managed_limit"] = format!("{}B", gb10()).into();
    (deployment, host)
}

/// An SGLang deployment whose phases derive from a 4 GiB KV cache, with
/// `extra_args`.
fn sglang(extra_args: Value) -> (Value, Value) {
    let (mut deployment, mut host) = fixture();
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "sglang".into();
    profile["args"] = json!([]);
    profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
    let object = deployment.as_object_mut().unwrap();
    object.remove("resources");
    object.remove("residency");
    deployment["engine_config"] = json!({
        "memory": {"kv_cache": "4GiB"},
        "accept_extra_args": true,
        "extra_args": extra_args,
    });
    (deployment, host)
}

/// A TensorFold deployment declaring 110 GiB in every active phase and a
/// 4 GiB KV cache inside it, so the weights it holds are checked against it.
fn tensorfold(args: Value) -> (Value, Value) {
    let (mut deployment, mut host) = fixture();
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "tensorfold".into();
    profile["executable"] = "/opt/tf/bin/tensorfold".into();
    profile["build_fingerprint"] = "0.6.5".into();
    profile["args"] = args;
    profile["security"]["deep_park"] = "disabled".into();
    deployment["residency"] = "restart_only".into();
    deployment["engine_config"] = json!({"context_length": 8192, "memory": {"kv_cache": "4GiB"}});
    for phase in ["cold", "ready", "parking", "wake"] {
        deployment["resources"][phase]["allocations"][0]["bytes"] = "110GiB".into();
    }
    (deployment, host)
}

fn ready(e: &EffectiveDeployment) -> i64 {
    e.resources.ready.allocations[0].bytes
}

fn round_trips(e: &EffectiveDeployment) {
    let text = serde_json::to_string(e).unwrap();
    assert_eq!(&decode_effective_snapshot(&text).unwrap(), e);
}

// T03: Flash Next on one GB10 with SGLang's file-backed table: the memory
// weights are the rest of the checkpoint and SGLang's 8 GiB cache of the
// table, recorded beside the whole checkpoint and the table.
#[test]
fn flash_next_fits_one_gb10_with_sglang_keeping_the_table_on_disk() {
    let (mut deployment, host) = sglang(json!(["--ple-offload-backend", "file"]));
    // The startup placeholder sizes a first start from the weights held; a
    // declared peak keeps the cold phase inside the limit as well.
    deployment["engine_config"]["memory"]["startup"] = "110GiB".into();
    let e =
        resolve_effective_with_checkpoint(&deployment, &host, measured(Some(tables()))).unwrap();
    let memory = e.engine_config.memory();
    let held = WEIGHTS - TABLE + SGLANG_TABLE_CACHE_BYTES;
    assert_eq!(memory.weights_bytes, Some(held));
    assert_eq!(memory.request_bytes, held + 4 * GIB + memory.margin_bytes);
    assert_eq!(ready(&e), memory.request_bytes + OVERHEAD);
    assert!(ready(&e) <= gb10(), "{} > {}", ready(&e), gb10());
    let cold = e.resources.cold.allocations[0].bytes;
    assert!(cold <= gb10(), "startup {cold} > {}", gb10());
    let recorded = memory.disk_tables.unwrap();
    assert_eq!(recorded.checkpoint_weights_bytes, WEIGHTS);
    assert_eq!(recorded.tables, tables());
    assert_eq!(recorded.cache_bytes, SGLANG_TABLE_CACHE_BYTES);
    // The launch plan names the whole checkpoint, which the host verifies.
    assert_eq!(memory.checkpoint_weights_bytes(), Some(WEIGHTS));
    round_trips(&e);
}

// T03: the same deployment without the option is charged every weight byte:
// its Ready phase is above the managed limit, and its weights alone above the
// whole box.
#[test]
fn flash_next_does_not_fit_one_gb10_without_the_option() {
    const { assert!(WEIGHTS > GB10) };
    for args in [
        json!([]),
        json!(["--ple-offload-backend", "pinned"]),
        json!([
            "--ple-offload-backend",
            "file",
            "--no-ple-offload-embedding"
        ]),
    ] {
        let (deployment, host) = sglang(args.clone());
        let e = resolve_effective_with_checkpoint(&deployment, &host, measured(Some(tables())))
            .unwrap();
        assert_eq!(
            e.engine_config.memory().weights_bytes,
            Some(WEIGHTS),
            "{args}"
        );
        assert_eq!(e.engine_config.memory().disk_tables, None, "{args}");
        assert!(ready(&e) > gb10(), "{args}");
    }
    // Nor does a checkpoint the host found no tables in.
    let (deployment, host) = sglang(json!(["--ple-offload-backend", "file"]));
    let e = resolve_effective_with_checkpoint(&deployment, &host, measured(None)).unwrap();
    assert_eq!(e.engine_config.memory().weights_bytes, Some(WEIGHTS));
    assert!(ready(&e) > gb10());
}

// T03: TensorFold's `--ple-on-ssd` reads the table from the checkpoint at each
// lookup; its declared 110 GiB holds the rest of the weights only then.
#[test]
fn flash_next_fits_one_gb10_with_tensorfold_reading_the_table_from_ssd() {
    let (deployment, host) = tensorfold(json!(["--ple-on-ssd"]));
    let e =
        resolve_effective_with_checkpoint(&deployment, &host, measured(Some(tables()))).unwrap();
    let memory = e.engine_config.memory();
    assert_eq!(
        memory.weights_bytes,
        Some(WEIGHTS - TABLE + TENSORFOLD_TABLE_CACHE_BYTES)
    );
    assert_eq!(memory.checkpoint_weights_bytes(), Some(WEIGHTS));
    assert_eq!(ready(&e), 110 * GIB);
    round_trips(&e);
    let (deployment, host) = tensorfold(json!([]));
    assert!(
        resolve_effective_with_checkpoint(&deployment, &host, measured(Some(tables()))).is_err()
    );
}

// T03, T39: a TP2 group member applies the same rule to its share: half the
// resident split weights, the tensors kept whole, and the cache.
#[test]
fn a_group_member_keeps_its_share_of_the_table_on_disk() {
    let (mut deployment, host) = sglang(json!(["--ple-offload-backend", "file"]));
    deployment["topology"] = json!({"tensor_parallel": 2, "pipeline_parallel": 1});
    deployment["placement"] = json!({"hosts": ["host-0", "host-1"]});
    let with =
        resolve_effective_with_checkpoint(&deployment, &host, measured(Some(tables()))).unwrap();
    let (held, cache) = disk_table_weights_bytes(
        WEIGHTS,
        Some(&layout()),
        &tables(),
        2,
        1,
        SGLANG_TABLE_CACHE_BYTES,
    )
    .unwrap();
    assert_eq!(cache, SGLANG_TABLE_CACHE_BYTES);
    let resident_split = 124 * GIB - TABLE;
    assert_eq!(held, resident_split / 2 + 2 * GIB + cache);
    let memory = with.engine_config.memory();
    assert_eq!(memory.weights_bytes, Some(held));
    assert_eq!(memory.checkpoint_weights_bytes(), Some(WEIGHTS));
    assert_eq!(
        memory.member.unwrap().checkpoint_weights_bytes,
        Some(WEIGHTS)
    );
    assert!(memory.disk_tables.is_some());
    round_trips(&with);
    // Without the option the member holds half the table as well.
    let (mut deployment, host) = sglang(json!([]));
    deployment["topology"] = json!({"tensor_parallel": 2, "pipeline_parallel": 1});
    deployment["placement"] = json!({"hosts": ["host-0", "host-1"]});
    let without =
        resolve_effective_with_checkpoint(&deployment, &host, measured(Some(tables()))).unwrap();
    assert_eq!(
        without.engine_config.memory().weights_bytes,
        Some(62 * GIB + 2 * GIB)
    );
    assert_eq!(without.engine_config.memory().disk_tables, None);
}

// T03, T39: without the option the tables change nothing: the same
// resolution, the same snapshot, the same fingerprint as before they were
// measured.
#[test]
fn without_the_option_the_tables_change_nothing() {
    for (deployment, host) in [sglang(json!([])), tensorfold(json!([]))] {
        let facts = CheckpointFacts {
            weights_bytes: Some(20 * GIB),
            disk_tables: Some(CheckpointTables {
                bytes: 6 * GIB,
                sharded_bytes: 6 * GIB,
                ..tables()
            }),
            ..Default::default()
        };
        let with = resolve_effective_with_checkpoint(&deployment, &host, facts).unwrap();
        let without = resolve_effective_with_checkpoint(
            &deployment,
            &host,
            CheckpointFacts {
                disk_tables: None,
                ..facts
            },
        )
        .unwrap();
        assert_eq!(with, without);
        assert!(!serde_json::to_string(&with)
            .unwrap()
            .contains("disk_tables"));
    }
    // Pinned on main (e970f03f) before the tables were measured: without the
    // option, and with it on a checkpoint the host found no tables in, each
    // with the tables measured beside it here.
    let facts = CheckpointFacts {
        weights_bytes: Some(20 * GIB),
        disk_tables: Some(CheckpointTables {
            bytes: 6 * GIB,
            sharded_bytes: 6 * GIB,
            ..tables()
        }),
        ..Default::default()
    };
    let fingerprint = |(deployment, host): (Value, Value), facts| {
        resolve_effective_with_checkpoint(&deployment, &host, facts)
            .unwrap()
            .recipe_fingerprint
    };
    assert_eq!(
        fingerprint(sglang(json!([])), facts),
        "caf70f56a309d8fe0a8c29711231c9169cd7105e3b3836d0dfcc867e5a9dabbc"
    );
    assert_eq!(
        fingerprint(tensorfold(json!([])), facts),
        "be1e89893ac42ba0a98935106e803a55afd3e24582408a76a5825a07ce4a5733"
    );
    let no_tables = CheckpointFacts {
        disk_tables: None,
        ..facts
    };
    assert_eq!(
        fingerprint(sglang(json!(["--ple-offload-backend", "file"])), no_tables),
        "68bbc309c5138b108d50cb7c7e60307d2bc2400fc5600e0e80354b4884aac9fc"
    );
}

// T03: the allowance assumes SGLang's default budget for the cached part of
// the table, so a deployment changing it is refused rather than mis-sized.
#[test]
fn a_changed_sglang_table_budget_is_refused() {
    let (mut deployment, mut host) = sglang(json!(["--ple-offload-backend", "file"]));
    host["runtime_profiles"]["local"]["env"]["SGLANG_QWEN4_PLE_FILE_RSS_BUDGET_GB"] = "0".into();
    let error = resolve_effective_with_checkpoint(&deployment, &host, measured(Some(tables())))
        .unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnsupportedCombination);
    assert_eq!(error.path, "engine_config.env");
    // Without the option the variable is SGLang's business only.
    deployment["engine_config"]["extra_args"] = json!([]);
    deployment["engine_config"]["memory"] = json!({"kv_cache": "4GiB"});
    let facts = CheckpointFacts {
        weights_bytes: Some(20 * GIB),
        ..Default::default()
    };
    resolve_effective_with_checkpoint(&deployment, &host, facts).unwrap();
}

// T03: which arguments keep the tables on disk, in every spelling; vLLM 0.30
// has no such option.
#[test]
fn the_engine_options_are_recognized() {
    let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let sglang = |list: &[&str]| disk_table_cache_bytes(Engine::Sglang, &args(list));
    assert_eq!(
        sglang(&["--ple-offload-backend", "file"]),
        Some(SGLANG_TABLE_CACHE_BYTES)
    );
    assert_eq!(
        sglang(&["--ple-offload-backend=file"]),
        Some(SGLANG_TABLE_CACHE_BYTES)
    );
    assert_eq!(
        sglang(&["--ple_offload_backend", "file"]),
        Some(SGLANG_TABLE_CACHE_BYTES)
    );
    assert_eq!(
        sglang(&["--ple-offload-embedding", "--ple-offload-backend", "file"]),
        Some(SGLANG_TABLE_CACHE_BYTES)
    );
    assert_eq!(sglang(&["--ple-offload-backend", "pinned"]), None);
    assert_eq!(sglang(&["--ple-offload-embedding"]), None);
    assert_eq!(
        sglang(&[
            "--ple-offload-backend",
            "file",
            "--no-ple-offload-embedding"
        ]),
        None
    );
    assert_eq!(
        sglang(&[
            "--ple-offload-backend",
            "file",
            "--ple-offload-backend",
            "pinned"
        ]),
        None
    );
    assert_eq!(sglang(&[]), None);
    let tensorfold = |list: &[&str]| disk_table_cache_bytes(Engine::Tensorfold, &args(list));
    assert_eq!(
        tensorfold(&["--ple-on-ssd"]),
        Some(TENSORFOLD_TABLE_CACHE_BYTES)
    );
    assert_eq!(tensorfold(&[]), None);
    assert_eq!(
        disk_table_cache_bytes(Engine::Vllm, &args(&["--ple-on-ssd"])),
        None
    );
    assert_eq!(
        disk_table_cache_bytes(Engine::Vllm, &args(&["--ple-offload-backend", "file"])),
        None
    );
}

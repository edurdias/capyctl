//! Owner decision 2026-09-22: pre-E1 effective revisions are carried forward
//! into the ADR 0014 shape. The fixtures are the effective halves of the pre-E1
//! goldens (`git show HEAD:crates/mllm-config/tests/fixtures/effective-*-golden.json`
//! before WE1), so they are exactly what a pre-E1 store holds.

use mllm_config::effective::{
    decode_effective_snapshot, legacy_retained_deployment, migrate_legacy_effective,
    resolve_effective, strip_legacy_launch_settings,
};
use serde_json::{json, Value};

fn fixture(engine: &str) -> Value {
    let text = match engine {
        "vllm" => include_str!("fixtures/pre-e1-effective-vllm.json"),
        _ => include_str!("fixtures/pre-e1-effective-sglang.json"),
    };
    serde_json::from_str(text).unwrap()
}

/// T14 T33: WE1 cannot read a pre-E1 revision; after migration it decodes, the
/// retired settings are carried as the deployment's engine configuration, and
/// the legacy fingerprint is returned for ownership to keep using.
#[test]
fn a_pre_e1_vllm_revision_migrates_to_the_engine_config_that_renders_it() {
    let legacy = fixture("vllm");
    let text = legacy.to_string();
    assert!(decode_effective_snapshot(&text).is_err());
    let migrated = migrate_legacy_effective(&text).unwrap().unwrap();
    assert_eq!(
        migrated.legacy_fingerprint,
        legacy["recipe_fingerprint"].as_str().unwrap()
    );
    assert_ne!(
        migrated.effective.recipe_fingerprint,
        migrated.legacy_fingerprint
    );
    assert_eq!(
        migrated.engine_config,
        json!({"kv_cache_dtype": "auto", "vllm": {"block_size_tokens": 16},
               "memory": {"kv_cache": "4294967296B"}})
    );
    let decoded = decode_effective_snapshot(&migrated.effective_json).unwrap();
    assert_eq!(decoded, migrated.effective);
    let value: Value = serde_json::from_str(&migrated.effective_json).unwrap();
    assert!(value["profile"].get("launch_settings").is_none());
    let engine = &value["engine_config"];
    assert_eq!(engine["engine"], "vllm");
    assert_eq!(engine["enable_sleep_mode"], true);
    assert_eq!(engine["block_size_tokens"], 16);
    assert_eq!(engine["common"]["kv_cache_dtype"], "auto");
    assert_eq!(engine["memory"]["kv_cache_bytes"], 4_i64 << 30);
    // Declared resources are carried, so the request is the Ready total
    // admission already reserved, never re-derived.
    assert_eq!(engine["memory"]["request_bytes"], 8_i64 << 30);
    assert_eq!(value["resources"], legacy["resources"]);
    // A second pass finds nothing to do.
    assert!(migrate_legacy_effective(&migrated.effective_json)
        .unwrap()
        .is_none());
}

/// T14 T33: the pinned SGLang recipe maps to explicit typed values, so the
/// migrated launch does not depend on a later mllm default.
#[test]
fn a_pre_e1_sglang_revision_migrates_with_the_pinned_values_explicit() {
    let text = fixture("sglang").to_string();
    let migrated = migrate_legacy_effective(&text).unwrap().unwrap();
    assert_eq!(
        migrated.engine_config,
        json!({"dtype": "bfloat16", "context_length": 4096, "max_concurrent_requests": 8,
               "cuda_graphs": false,
               "sglang": {"max_total_tokens": 4096, "tokenizer_workers": 1},
               "memory": {"kv_cache": "4294967296B"}})
    );
    let value: Value = serde_json::from_str(&migrated.effective_json).unwrap();
    let engine = &value["engine_config"];
    assert_eq!(engine["memory_saver"], true);
    assert_eq!(engine["weight_restore"], "disk_reload");
    assert_eq!(engine["tokenizer_workers"], 1);
    assert_eq!(engine["max_total_tokens"], 4096);
    decode_effective_snapshot(&migrated.effective_json).unwrap();
}

fn refused(mutate: impl FnOnce(&mut Value), engine: &str) -> String {
    let mut legacy = fixture(engine);
    mutate(&mut legacy);
    migrate_legacy_effective(&legacy.to_string())
        .unwrap_err()
        .to_string()
}

/// T33: a retired setting the E1 model cannot express is refused by name,
/// never approximated.
#[test]
fn unmappable_pre_e1_settings_are_refused_with_a_diagnostic() {
    let message = refused(
        |v| v["profile"]["launch_settings"]["tensor_parallel_size"] = json!(2),
        "vllm",
    );
    assert!(message.contains("tensor_parallel_size is 2"), "{message}");
    let message = refused(
        |v| v["profile"]["launch_settings"]["cpu_offload_bytes"] = json!(1 << 30),
        "vllm",
    );
    assert!(message.contains("cpu_offload_bytes"), "{message}");
    let message = refused(
        |v| v["profile"]["launch_settings"]["enable_sleep_mode"] = json!(false),
        "vllm",
    );
    assert!(message.contains("restart_only"), "{message}");
    let message = refused(
        |v| v["profile"]["launch_settings"]["lora"] = json!(true),
        "sglang",
    );
    assert!(message.contains("lora"), "{message}");
    let message = refused(
        |v| v["profile"]["launch_settings"]["recipe"] = json!("other"),
        "sglang",
    );
    assert!(message.contains("pinned SGLang recipe"), "{message}");
    // A requested KV above the Ready reservation does not resolve.
    let message = refused(
        |v| {
            v["profile"]["launch_settings"]["requested_budget"]["kv_cache_bytes"] =
                json!(64_i64 << 30)
        },
        "vllm",
    );
    assert!(message.contains("does not resolve"), "{message}");
}

/// T32: a stored host document loses only its retired settings; resolution
/// then accepts it. The host's own YAML stays refused at parse (config).
#[test]
fn stored_host_documents_lose_only_their_launch_settings() {
    let pre: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let mut host = pre["host"].clone();
    host["runtime_profiles"]["local"]["launch_settings"] = json!({"engine": "vllm"});
    assert!(resolve_effective(&pre["deployment"], &host).is_err());
    let (stripped, removed) = strip_legacy_launch_settings(&host).unwrap();
    assert_eq!(removed, json!({"local": {"engine": "vllm"}}));
    assert_eq!(stripped, pre["host"]);
    resolve_effective(&pre["deployment"], &stripped).unwrap();
    assert!(strip_legacy_launch_settings(&stripped).is_none());
    // JSON is YAML: the host's own file with the retired block is refused.
    let error =
        mllm_config::parse_strict(mllm_config::ConfigKind::Host, &host.to_string()).unwrap_err();
    assert!(error.to_string().contains("engine_config"), "{error}");
}

/// T33: a launch a host agent journaled before E1 resolves against the edited
/// host document for probing and parking; a command in the E1 shape is left
/// to ordinary resolution.
#[test]
fn a_pre_e1_retained_deployment_resolves_against_the_edited_host() {
    let pre: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let mut deployment = pre["deployment"].clone();
    deployment.as_object_mut().unwrap().remove("engine_config");
    assert!(resolve_effective(&deployment, &pre["host"]).is_err());
    let retained = legacy_retained_deployment(&deployment).unwrap();
    let effective = resolve_effective(&retained, &pre["host"]).unwrap();
    assert_eq!(effective.engine_config.memory().kv_cache_bytes, 8 << 30);
    assert!(legacy_retained_deployment(&pre["deployment"]).is_none());
}

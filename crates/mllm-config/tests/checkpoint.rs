//! ADR 0014 §5, §7 (WE3): checkpoint location and re-resolution with the
//! weights a recorded digest supplies.
use mllm_config::effective::{
    checkpoint_location, declared_checkpoint_digest, resolve_effective,
    resolve_effective_with_checkpoint, resolve_snapshot_with_checkpoint, CheckpointFacts,
};
use mllm_config::ConfigErrorCode;
use serde_json::{json, Value};

fn fixture() -> (Value, Value) {
    let value: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    (value["deployment"].clone(), value["host"].clone())
}

// T14: only the canonical digest form is an expectation; a label expects nothing.
#[test]
fn only_a_canonical_digest_is_an_expectation() {
    let digest = format!("sha256:{}", "a".repeat(64));
    assert_eq!(declared_checkpoint_digest(&digest), Some(digest.as_str()));
    for label in [
        "sha256:model".to_string(),
        format!("sha256:{}", "A".repeat(64)),
        format!("sha512:{}", "a".repeat(64)),
        "a".repeat(71),
    ] {
        assert_eq!(declared_checkpoint_digest(&label), None, "{label}");
    }
}

// SPEC §13.3: a checkpoint is located from the model block and the host's own
// store, without resolving memory; only a local source names a directory.
#[test]
fn a_checkpoint_is_located_without_resolving_memory() {
    let (mut deployment, host) = fixture();
    deployment["model"]["path"] = json!("toy");
    deployment.as_object_mut().unwrap().remove("resources");
    deployment["engine_config"] = json!({"memory": {"kv_cache": "4GiB"}});
    // Full resolution cannot proceed without the weights ...
    assert_eq!(
        resolve_effective(&deployment, &host).unwrap_err().code,
        ConfigErrorCode::NotMaterializable
    );
    // ... but the checkpoint can still be found and measured.
    let location = checkpoint_location(&deployment, &host).unwrap();
    assert_eq!(location.model_store.to_str(), Some("/srv/models"));
    assert_eq!(location.checkpoint.to_str(), Some("/srv/models/toy"));
    assert_eq!(location.content_fingerprint, "sha256:model");
    deployment["model"] = json!({
        "source": {"type": "huggingface", "repo": "Qwen/Qwen3-4B"},
        "content_fingerprint": "sha256:model", "revision": "r1"
    });
    assert_eq!(
        checkpoint_location(&deployment, &host).unwrap_err().code,
        ConfigErrorCode::NotMaterializable
    );
}

// T14 (P2): a revision frozen with placeholder weights re-resolves exactly
// with the measured ones; declared values stay, derived values follow.
#[test]
fn a_frozen_snapshot_re_resolves_with_the_recorded_weights() {
    let (mut deployment, host) = fixture();
    deployment.as_object_mut().unwrap().remove("resources");
    deployment["engine_config"] = json!({"memory": {"kv_cache": "4GiB"}});
    let placeholder = resolve_effective_with_checkpoint(
        &deployment,
        &host,
        CheckpointFacts {
            weights_bytes: Some(0),
            ..Default::default()
        },
    )
    .unwrap();
    let frozen = serde_json::to_string(&placeholder).unwrap();
    let weights = 3_i64 << 30;
    let resolved = resolve_snapshot_with_checkpoint(
        &frozen,
        CheckpointFacts {
            weights_bytes: Some(weights),
            ..Default::default()
        },
    )
    .unwrap();
    let direct = resolve_effective_with_checkpoint(
        &deployment,
        &host,
        CheckpointFacts {
            weights_bytes: Some(weights),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(resolved.engine_config, direct.engine_config);
    assert_eq!(resolved.resources, direct.resources);
    assert_eq!(resolved.engine_config.memory().weights_bytes, Some(weights));
    assert_eq!(resolved.engine_config.memory().kv_cache_bytes, 4 << 30);
    // A snapshot that is not exact is never carried forward.
    let mut tampered: Value = serde_json::from_str(&frozen).unwrap();
    tampered["engine_config"]["memory"]["request_bytes"] = json!(1);
    assert!(resolve_snapshot_with_checkpoint(
        &tampered.to_string(),
        CheckpointFacts {
            weights_bytes: Some(weights),
            ..Default::default()
        }
    )
    .is_err());
}

//! ADR 0014 §5, §7 (WE3): checkpoint location and re-resolution with the
//! weights a recorded digest supplies.
use capyctl_config::effective::{
    checkpoint_location, declared_checkpoint_digest, resolve_effective,
    resolve_effective_with_checkpoint, resolve_snapshot_with_checkpoint, CheckpointFacts,
    DrafterLocation,
};
use capyctl_config::ConfigErrorCode;
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
// store, without resolving memory.
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
    // ADR 0008: a remote source is located at its fixed directory in the
    // store, where the host materializes it before the first placement.
    let sha = "0123456789abcdef0123456789abcdef01234567";
    deployment["model"] = json!({
        "source": {"type": "huggingface", "repo": "Qwen/Qwen3-4B", "revision": sha},
        "content_fingerprint": "sha256:model", "revision": "r1"
    });
    let location = checkpoint_location(&deployment, &host).unwrap();
    assert_eq!(
        location.checkpoint.to_str().unwrap(),
        format!("/srv/models/sources/huggingface/Qwen--Qwen3-4B@{sha}")
    );
    // An unpinned revision is refused, not located.
    deployment["model"]["source"]["revision"] = json!("main");
    assert!(checkpoint_location(&deployment, &host).is_err());
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

// T14 (ADR 0008, owner decision 2026-09-25): a downloaded checkpoint is
// located in, and contained by, the host's sources store; a local one by its
// model store.
#[test]
fn a_downloaded_checkpoint_is_located_in_the_sources_store() {
    let (mut deployment, mut host) = fixture();
    let store = host["model_store"]["path"].as_str().unwrap().to_owned();
    host["model_sources"] = json!({"path": "/state/models"});
    deployment["model"].as_object_mut().unwrap().remove("path");
    let sha = "a".repeat(64);
    deployment["model"]["source"] =
        json!({"http": {"url": "https://example.test/w.bin", "sha256": sha}});
    let location = checkpoint_location(&deployment, &host).unwrap();
    assert_eq!(location.model_store, std::path::Path::new("/state/models"));
    assert_eq!(
        location.checkpoint,
        std::path::Path::new(&format!("/state/models/sources/http/{sha}"))
    );
    deployment["model"]
        .as_object_mut()
        .unwrap()
        .remove("source");
    deployment["model"]["path"] = json!("toy");
    let local = checkpoint_location(&deployment, &host).unwrap();
    assert_eq!(local.model_store, std::path::Path::new(&store));
}

// ADR 0014 §5 amendment A6 (found live 2026-10-02): a launch loads its draft
// model beside the checkpoint, so the checkpoint's location names the draft
// model's directory and the approved root it lies in, for each engine's
// spelling. Nothing else is a draft model.
#[test]
fn a_checkpoint_location_names_the_draft_model_it_loads_beside() {
    let located = |engine: &str, args: Value| {
        let (mut deployment, mut host) = fixture();
        let profile = &mut host["runtime_profiles"]["local"];
        profile["engine"] = json!(engine);
        profile["args"] = json!([]);
        profile["security"]["approved_paths"] = json!(["/srv/other", "/srv/drafters"]);
        deployment["engine_config"]["accept_extra_args"] = json!(true);
        deployment["engine_config"]["extra_args"] = args;
        checkpoint_location(&deployment, &host).unwrap().drafter
    };
    let expected = Some(DrafterLocation {
        root: "/srv/drafters".into(),
        path: "/srv/drafters/d".into(),
    });
    assert_eq!(
        located(
            "vllm",
            json!([
                "--speculative-config",
                r#"{"method":"dflash","model":"/srv/drafters/d"}"#
            ])
        ),
        expected
    );
    assert_eq!(
        located(
            "vllm",
            json!([r#"--speculative-config={"model":"/srv/drafters/d"}"#])
        ),
        expected
    );
    assert_eq!(
        located(
            "sglang",
            json!(["--speculative-draft-model-path", "/srv/drafters/d"])
        ),
        expected
    );
    assert_eq!(
        located("tensorfold", json!(["--drafter", "/srv/drafters/d"])),
        expected
    );
    // MTP heads live in the checkpoint; no draft directory.
    assert_eq!(
        located(
            "vllm",
            json!([
                "--speculative-config",
                r#"{"method":"mtp","num_speculative_tokens":3}"#
            ])
        ),
        None
    );
    assert_eq!(located("tensorfold", json!(["--no-drafts"])), None);
    // Another engine's spelling names nothing for this one.
    assert_eq!(
        located("vllm", json!(["--drafter", "/srv/drafters/d"])),
        None
    );
    // A path outside every approved root is not located (resolution refuses it).
    assert_eq!(
        located(
            "sglang",
            json!(["--speculative-draft-model-path", "/etc/d"])
        ),
        None
    );
    assert_eq!(located("vllm", json!([])), None);
}

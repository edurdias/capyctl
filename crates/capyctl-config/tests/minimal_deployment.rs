//! Owner decision 2026-09-25 (ADR 0014 amendment "minimal deployment
//! file"): `name`, `engine` and `model` are a deployment; everything else is
//! defaulted, the same way on every role (`capyctl_config::deployment_defaults`).
//! A full document resolves exactly as before. CPU-only resolution tests; none
//! of this qualifies an engine recipe (SPEC §18).

use capyctl_config::effective::{
    declared_checkpoint_digest, decode_effective_snapshot, resolve_effective,
    resolve_effective_with_checkpoint, resolve_snapshot_with_checkpoint, CheckpointFacts,
    ModelSource, Residency,
};
use capyctl_config::{parse_strict, ConfigErrorCode, ConfigKind};
use capyctl_domain::launch::SettingSource;
use serde_json::{json, Value};

const GIB: i64 = 1 << 30;
const MIB: i64 = 1 << 20;
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn fixture() -> (Value, Value) {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    (all["deployment"].clone(), all["host"].clone())
}

/// The lab host: one unified domain (32 GiB managed), one shared `gpu0`, and
/// the profile `local` (vLLM, revision 7).
fn unified_host() -> Value {
    fixture().1
}

/// A discrete host: host RAM in `system` (12 GiB parked), the card in `gpu0`
/// (14848 MiB managed).
fn discrete_host() -> Value {
    let mut host = unified_host();
    host["resource_policy"]["domains"] = json!({
        "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB",
                   "parked_limit": "12GiB", "host_kv_limit": "4GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "14848MiB",
                 "free_reserve": "1536MiB", "parked_limit": "2GiB"}
    });
    host["resource_policy"]["devices"] = json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
    host
}

fn minimal(model: &str) -> Value {
    parse_strict(
        ConfigKind::Deployment,
        &format!("name: my-model\nengine: vllm\nmodel: {model}\n"),
    )
    .expect("a minimal document parses")
}

fn weights(bytes: i64) -> CheckpointFacts {
    CheckpointFacts {
        weights_bytes: Some(bytes),
        ..Default::default()
    }
}

fn provenance(effective: &capyctl_config::effective::EffectiveDeployment) -> Value {
    serde_json::to_value(&effective.engine_config).unwrap()["provenance"].clone()
}

// T14 (owner decision 2026-09-25): the three fields parse into the document
// the rest of capyctl reads.
#[test]
fn a_minimal_file_parses_into_a_complete_document() {
    assert_eq!(
        minimal("Qwen3-4B"),
        json!({
            "schema_version": 1, "kind": "deployment", "name": "my-model",
            "runtime_profile": "vllm", "routes": ["my-model"],
            "recipe": "standard", "recovery": "reconcile",
            "model": {"path": "Qwen3-4B", "content_fingerprint": "measured", "revision": "1"},
        })
    );
    // `name` and `model` stay required; nothing else is.
    for missing in ["engine: vllm\nmodel: m\n", "name: n\nengine: vllm\n"] {
        let error = parse_strict(ConfigKind::Deployment, missing).unwrap_err();
        assert_eq!(error.code, ConfigErrorCode::MissingRequired, "{error}");
    }
    // An unknown field is still refused.
    let error = parse_strict(
        ConfigKind::Deployment,
        "name: n\nengine: vllm\nmodel: m\nsurprise: 1\n",
    )
    .unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnknownField);
    // A home-relative path is the CLI's to expand; a document that still has
    // one came from elsewhere and is refused rather than guessed.
    let error = parse_strict(
        ConfigKind::Deployment,
        "name: n\nengine: vllm\nmodel: ~/models/m\n",
    )
    .unwrap_err();
    assert_eq!(error.path, "model");
    let mut home = json!({"model": "~/models/Qwen3-4B"});
    capyctl_config::deployment_defaults::expand_home(
        &mut home,
        Some(std::path::Path::new("/home/user")),
    );
    assert_eq!(home["model"], "/home/user/models/Qwen3-4B");
}

// T14 T21 (owner decision 2026-09-25, ADR 0012): on a unified host the
// minimal file runs the host's vLLM profile at its published revision on its
// GPU, parks deep, and sizes its memory from the checkpoint with the default
// KV cache.
#[test]
fn a_minimal_file_resolves_on_a_unified_host() {
    let host = unified_host();
    let deployment = minimal("toy");
    // Before the checkpoint is measured the request cannot be derived:
    // acceptance takes the provisional path (ADR 0014 §7).
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::NotMaterializable, "{error}");
    let effective = resolve_effective_with_checkpoint(&deployment, &host, weights(8 * GIB))
        .expect("resolves once the weights are known");
    assert_eq!(effective.routes, ["my-model"]);
    assert_eq!(effective.recipe, "standard");
    assert_eq!(effective.profile.revision, 7);
    assert_eq!(effective.residency, Residency::Deep);
    assert_eq!(
        serde_json::to_value(&effective.selected_devices).unwrap(),
        json!([{"id": "gpu0", "sharing": "shared"}])
    );
    assert_eq!(
        effective.model.resolved_path.as_deref(),
        Some("/srv/models/toy")
    );
    assert_eq!(effective.model.content_fingerprint, "measured");
    assert_eq!(effective.model.revision, "1");
    // min(4 GiB, 32 GiB / 4) of KV cache; the request adds the weights and
    // the engine's margin.
    let memory = effective.engine_config.memory();
    assert_eq!(memory.kv_cache_bytes, 4 * GIB);
    assert_eq!(
        memory.request_bytes,
        8 * GIB + 4 * GIB + capyctl_config::effective::overhead_margin(effective.profile.engine)
    );
    let provenance = provenance(&effective);
    assert_eq!(provenance["residency"], json!(SettingSource::CapyctlDefault));
    assert_eq!(
        provenance["memory.kv_cache"],
        json!(SettingSource::CapyctlDefault)
    );
    assert_eq!(provenance["memory.request"], json!(SettingSource::Derived));
    // A profile that opted out of deep parking restarts instead.
    let mut opted_out = host.clone();
    opted_out["runtime_profiles"]["local"]["security"]["deep_park"] = "disabled".into();
    let effective =
        resolve_effective_with_checkpoint(&deployment, &opted_out, weights(8 * GIB)).unwrap();
    assert_eq!(effective.residency, Residency::RestartOnly);
}

// T14 T26 (owner decision 2026-09-25, discrete GPU design §3, §5): on a
// discrete host the same file is sized for the card and parks in host RAM
// when the copy fits, deep when it does not.
#[test]
fn a_minimal_file_resolves_on_a_discrete_host() {
    let host = discrete_host();
    let deployment = minimal("toy");
    let small = resolve_effective_with_checkpoint(&deployment, &host, weights(4 * GIB)).unwrap();
    assert_eq!(small.residency, Residency::HostBacked);
    let memory = small.engine_config.memory();
    // min(4 GiB, 14848 MiB / 4) of KV cache; vLLM's request is at least 0.75
    // of the card (16384 MiB).
    assert_eq!(memory.kv_cache_bytes, 3712 * MIB);
    assert_eq!(memory.request_bytes, 16384 * MIB / 100 * 75);
    let domains: Vec<&str> = small
        .resources
        .ready
        .allocations
        .iter()
        .map(|a| a.domain.as_str())
        .collect();
    assert_eq!(domains, ["gpu0", "system"]);
    // 8 GiB of weights plus the engine's host overhead exceed what the system
    // domain holds parked: deep.
    let large = resolve_effective_with_checkpoint(&deployment, &host, weights(8 * GIB)).unwrap();
    assert_eq!(large.residency, Residency::Deep);
    // A stated residency is kept.
    let mut stated = deployment.clone();
    stated["residency"] = "deep".into();
    let kept = resolve_effective_with_checkpoint(&stated, &host, weights(4 * GIB)).unwrap();
    assert_eq!(kept.residency, Residency::Deep);
    assert!(provenance(&kept).get("residency").is_none());
}

// T14 (ADR 0014 §7): the chosen residency is re-chosen when a provisional
// revision is re-resolved with the measured weights, and a snapshot of it
// decodes exactly.
#[test]
fn a_defaulted_residency_is_chosen_again_from_the_measured_weights() {
    let host = discrete_host();
    let deployment = minimal("toy");
    // Acceptance before measurement: frozen with zero weights.
    let provisional = resolve_effective_with_checkpoint(&deployment, &host, weights(0)).unwrap();
    assert_eq!(provisional.residency, Residency::HostBacked);
    let snapshot = serde_json::to_string(&provisional).unwrap();
    assert_eq!(decode_effective_snapshot(&snapshot).unwrap(), provisional);
    let measured = resolve_snapshot_with_checkpoint(&snapshot, weights(8 * GIB)).unwrap();
    assert_eq!(measured.residency, Residency::Deep);
    assert_eq!(
        measured,
        resolve_effective_with_checkpoint(&deployment, &host, weights(8 * GIB)).unwrap()
    );
    let snapshot = serde_json::to_string(&measured).unwrap();
    assert_eq!(decode_effective_snapshot(&snapshot).unwrap(), measured);
}

// T14 T34 (ADR 0008): `hf: owner/repo@<commit>` is a pinned Hugging Face
// source that resolves into the sources store under the models directory.
#[test]
fn the_hugging_face_shorthand_resolves_into_the_sources_store() {
    let deployment = minimal(&format!("{{hf: Qwen/Qwen3-4B@{SHA}}}"));
    assert_eq!(
        deployment["model"]["source"],
        json!({"type": "huggingface", "repo": "Qwen/Qwen3-4B", "revision": SHA})
    );
    let effective =
        resolve_effective_with_checkpoint(&deployment, &unified_host(), weights(8 * GIB)).unwrap();
    assert!(matches!(
        effective.model.source,
        ModelSource::HuggingFace { .. }
    ));
    assert_eq!(
        effective.model.resolved_path,
        Some(format!(
            "/srv/models/sources/huggingface/Qwen--Qwen3-4B@{SHA}"
        ))
    );
    // Unpinned, it is refused with the way to pin it.
    let error = parse_strict(
        ConfigKind::Deployment,
        "name: n\nengine: vllm\nmodel: {hf: Qwen/Qwen3-4B}\n",
    )
    .unwrap_err();
    assert_eq!(error.path, "model.hf");
}

// T14 (SPEC §7): a relative path is under the models directory; an absolute
// one is taken as written.
#[test]
fn relative_and_absolute_model_paths() {
    let host = unified_host();
    for (model, resolved) in [
        ("toy", "/srv/models/toy"),
        ("family/toy", "/srv/models/family/toy"),
        ("/srv/models/family/toy", "/srv/models/family/toy"),
    ] {
        let effective =
            resolve_effective_with_checkpoint(&minimal(model), &host, weights(GIB)).unwrap();
        assert_eq!(effective.model.resolved_path.as_deref(), Some(resolved));
    }
}

// T14 (ADR 0014 §7): the content fingerprint is optional and then measured;
// a stated digest is an expectation the measurement must match (the store
// records a different measurement as `mismatch`).
#[test]
fn the_content_fingerprint_is_optional_and_a_stated_digest_is_expected() {
    let measured = minimal("toy");
    assert_eq!(
        declared_checkpoint_digest(measured["model"]["content_fingerprint"].as_str().unwrap()),
        None
    );
    let digest = format!("sha256:{}", "a".repeat(64));
    let stated = parse_strict(
        ConfigKind::Deployment,
        &format!(
            "name: n\nengine: vllm\nmodel:\n  path: toy\n  content_fingerprint: \"{digest}\"\n"
        ),
    )
    .unwrap();
    let effective =
        resolve_effective_with_checkpoint(&stated, &unified_host(), weights(GIB)).unwrap();
    assert_eq!(
        declared_checkpoint_digest(&effective.model.content_fingerprint),
        Some(digest.as_str())
    );
}

// T14 T03: an engine family names the host's one profile of that family, and
// a profile name is taken as the profile; an unknown one is refused.
#[test]
fn engine_names_a_profile_or_the_hosts_one_profile_of_that_family() {
    let host = unified_host();
    for engine in ["vllm", "local"] {
        let deployment = parse_strict(
            ConfigKind::Deployment,
            &format!("name: n\nengine: {engine}\nmodel: toy\n"),
        )
        .unwrap();
        let effective =
            resolve_effective_with_checkpoint(&deployment, &host, weights(GIB)).unwrap();
        assert_eq!(effective.profile.revision, 7, "{engine}");
    }
    let unknown = parse_strict(
        ConfigKind::Deployment,
        "name: n\nengine: sglang\nmodel: toy\n",
    )
    .unwrap();
    let error = resolve_effective_with_checkpoint(&unknown, &host, weights(GIB)).unwrap_err();
    assert_eq!(error.path, "runtime_profile");
}

// T14 (owner decision 2026-09-25): existing full documents are unchanged by
// the defaults, key for key, and resolve exactly as before.
#[test]
fn full_documents_are_unchanged() {
    let (deployment, host) = fixture();
    let mut json_expanded = deployment.clone();
    capyctl_config::deployment_defaults::expand(&mut json_expanded).unwrap();
    assert_eq!(json_expanded, deployment);
    assert_eq!(
        capyctl_config::deployment_defaults::for_host(&deployment, &host).unwrap(),
        deployment
    );
    let effective = resolve_effective(&deployment, &host).unwrap();
    let provenance = provenance(&effective);
    assert!(provenance.get("residency").is_none());
    assert!(provenance.get("memory.kv_cache").is_none());
}

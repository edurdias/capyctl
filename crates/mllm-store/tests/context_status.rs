//! ADR 0014 §5 (owner decision 2026-09-25): status shows each deployment's
//! effective context and where it came from. An embedded checkpoint is read
//! where the launch reads it; a remote host's path is never read here.
//! CPU-only.

use mllm_config::effective::resolve_effective;
use mllm_store::Store;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

fn effective(models: &std::path::Path, context_length: Option<u32>) -> Value {
    let source: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let (mut deployment, mut host) = (
        source["input"]["deployment"].clone(),
        source["input"]["host"].clone(),
    );
    host["model_store"]["path"] = json!(models);
    host["runtime_profiles"]["local"]["args"] = json!([]);
    deployment["model"]["path"] = json!(models.join("toy"));
    deployment["engine_config"]["memory"]["kv_cache"] = json!("4GiB");
    if let Some(tokens) = context_length {
        deployment["engine_config"]["context_length"] = json!(tokens);
    }
    serde_json::to_value(resolve_effective(&deployment, &host).unwrap()).unwrap()
}

fn store_with(effective: &Value, remote: bool) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("context.sqlite3");
    let store = Store::open(&path).unwrap();
    let writer = Connection::open(path).unwrap();
    writer
        .execute_batch(
            "INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('d','toy','managed','ready',1,0,1,1);",
        )
        .unwrap();
    writer
        .execute(
            "INSERT INTO effective_revisions VALUES('d',1,?1,'digest')",
            params![effective.to_string()],
        )
        .unwrap();
    if remote {
        writer
            .execute_batch(
                "INSERT INTO enrolled_hosts(host_id,host_name,key_digest,revoked) VALUES('h2','lab','key',0);",
            )
            .unwrap();
        writer
            .execute(
                "INSERT INTO host_effective_revisions(deployment_id,revision,host_id,outcome,effective_json,fingerprint) VALUES('d',1,'h2','resolved',?1,'digest')",
                params![effective.to_string()],
            )
            .unwrap();
    }
    (dir, store)
}

fn shown(store: &Store) -> Value {
    serde_json::to_value(store.snapshot().unwrap()).unwrap()["deployments"][0]["context"].clone()
}

/// A dense model at 512 KiB of KV per token: the 4 GiB grant holds 8192, of
/// which vLLM keeps one 16-token block for itself (8176).
fn checkpoint() -> tempfile::TempDir {
    let models = tempfile::tempdir().unwrap();
    std::fs::create_dir(models.path().join("toy")).unwrap();
    std::fs::write(
        models.path().join("toy/config.json"),
        json!({
            "num_hidden_layers": 32, "num_attention_heads": 32, "hidden_size": 4096,
            "max_position_embeddings": 32768, "torch_dtype": "bfloat16",
        })
        .to_string(),
    )
    .unwrap();
    models
}

// T14: status shows the fitted context with its provenance.
#[test]
fn status_shows_the_context_fitted_to_the_grant() {
    let models = checkpoint();
    let (_dir, store) = store_with(&effective(models.path(), None), false);
    assert_eq!(shown(&store), json!({"tokens": 8176, "source": "fitted"}));
    // A declared context wins, and one the grant cannot hold is flagged.
    let (_dir, store) = store_with(&effective(models.path(), Some(16384)), false);
    let context = shown(&store);
    assert_eq!(context["tokens"], 16384);
    assert_eq!(context["source"], "declared");
    assert!(context["warning"].as_str().unwrap().contains("8176"));
    // No configuration: the fallback and its reason.
    std::fs::remove_file(models.path().join("toy/config.json")).unwrap();
    let (_dir, store) = store_with(&effective(models.path(), None), false);
    let context = shown(&store);
    assert_eq!(context["tokens"], 4096);
    assert_eq!(context["source"], "fallback");
    assert!(context["reason"].as_str().unwrap().contains("config.json"));
}

// T14: a revision on an enrolled remote host is fitted by that host; the
// server never reads the host's path, even where it exists locally.
#[test]
fn a_remote_revision_is_fitted_by_its_host() {
    let models = checkpoint();
    let (_dir, store) = store_with(&effective(models.path(), None), true);
    let context = shown(&store);
    assert_eq!(context["source"], "on_host");
    assert!(context.get("tokens").is_none());
    let (_dir, store) = store_with(&effective(models.path(), Some(2048)), true);
    assert_eq!(shown(&store), json!({"tokens": 2048, "source": "declared"}));
}

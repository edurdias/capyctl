//! Owner decision 2026-09-23: the startup memory budget in `validate config`.
//! CPU-only; no engine runs here, and nothing here measures a startup peak.

mod support;

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

const GOLDEN: &str = include_str!("../../capyctl-config/tests/fixtures/effective-vllm-golden.json");

fn golden() -> Value {
    serde_json::from_str(GOLDEN).unwrap()
}

fn host_document(root: &Path) -> Value {
    let mut host = golden()["input"]["host"].clone();
    let state = root.join("host-state");
    host["state_dir"] = json!(state);
    host["identity_dir"] = json!(state.join("identity"));
    host
}

fn write(root: &Path, name: &str, value: &Value) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, value.to_string()).unwrap();
    path
}

fn validate(args: &[&str]) -> (i32, Value, String) {
    let out = support::capyctl()
        .args(["validate", "config"])
        .args(args)
        .args(["--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let value = serde_json::from_str(stdout.trim())
        .or_else(|_| serde_json::from_str(stderr.trim()))
        .unwrap_or(Value::Null);
    (
        out.status.code().unwrap(),
        value,
        format!("{stdout}{stderr}"),
    )
}

fn derived(memory: Value) -> Value {
    let mut doc = golden()["input"]["deployment"].clone();
    doc.as_object_mut().unwrap().remove("resources");
    doc["engine_config"]["memory"] = memory;
    doc
}

/// T14: resolution against a host shows the startup reservation and where it
/// came from: declared, the placeholder default, or a declared cold phase.
// T14 T26
#[test]
fn validate_shows_the_startup_reservation_and_its_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let host = write(dir.path(), "host.yaml", &host_document(dir.path()));
    for (doc, bytes, provenance) in [
        (
            derived(json!({"request": "8GiB", "kv_cache": "4GiB", "startup": "12GiB"})),
            // The reservation carries the engine's CUDA context and graphs.
            (12_i64 << 30) + capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES,
            "declared",
        ),
        // Weights pending: the placeholder is the request and the first
        // start's graph allowance (ADR 0014 amendment A8).
        (
            derived(json!({"request": "8GiB", "kv_cache": "4GiB"})),
            (8_i64 << 30)
                + capyctl_config::effective::STARTUP_GRAPH_ALLOWANCE_BYTES
                + capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES,
            "default",
        ),
        (
            golden()["input"]["deployment"].clone(),
            10_i64 << 30,
            "resources",
        ),
    ] {
        let deployment = write(dir.path(), "deployment.yaml", &doc);
        let (code, value, raw) = validate(&[
            "--file",
            deployment.to_str().unwrap(),
            "--host",
            host.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{raw}");
        let startup = &value["effective"]["startup"];
        assert_eq!(startup["bytes"], bytes, "{raw}");
        assert_eq!(startup["provenance"], provenance, "{raw}");
    }
}

/// T03: a startup peak below the request, beside a declared cold phase, or not
/// a byte quantity is a named error without a host document.
// T03
#[test]
fn validate_refuses_an_impossible_startup_peak_without_a_host() {
    let dir = tempfile::tempdir().unwrap();
    let mut beside = golden()["input"]["deployment"].clone();
    beside["engine_config"]["memory"]["startup"] = json!("12GiB");
    for doc in [
        derived(json!({"request": "8GiB", "kv_cache": "4GiB", "startup": "6GiB"})),
        derived(json!({"request": "8GiB", "kv_cache": "4GiB", "startup": "twelve"})),
        beside,
    ] {
        let deployment = write(dir.path(), "deployment.yaml", &doc);
        let (code, value, raw) = validate(&["--file", deployment.to_str().unwrap()]);
        assert_eq!(code, 2, "{doc}: {raw}");
        assert_eq!(value["code"], "invalid_config", "{raw}");
        assert!(
            value["message"]
                .as_str()
                .unwrap()
                .contains("memory.startup")
                || value["message"].as_str().unwrap().contains("bytes"),
            "{raw}"
        );
    }
}

/// ADR 0014 §5 (owner decision 2026-09-25): resolution against a host shows
/// the effective context: fitted to the KV grant from the checkpoint's
/// configuration, or the declared value (flagged when the grant cannot hold it).
// T14
#[test]
fn validate_shows_the_effective_context() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir_all(models.join("toy")).unwrap();
    // 512 KiB of bfloat16 KV per token: the 4 GiB grant holds 8192, of
    // which vLLM keeps one 16-token block (8176).
    std::fs::write(
        models.join("toy/config.json"),
        json!({
            "num_hidden_layers": 32, "num_attention_heads": 32, "hidden_size": 4096,
            "max_position_embeddings": 32768, "torch_dtype": "bfloat16",
        })
        .to_string(),
    )
    .unwrap();
    let mut host = host_document(dir.path());
    host["model_store"]["path"] = json!(models);
    host["runtime_profiles"]["local"]["args"] = json!([]);
    let host = write(dir.path(), "host.yaml", &host);
    let mut doc = golden()["input"]["deployment"].clone();
    doc["model"]["path"] = json!(models.join("toy"));
    for (declared, expected) in [
        (None, json!({"tokens": 8176, "source": "fitted"})),
        (Some(2048), json!({"tokens": 2048, "source": "declared"})),
    ] {
        if let Some(tokens) = declared {
            doc["engine_config"]["context_length"] = json!(tokens);
        }
        let deployment = write(dir.path(), "deployment.yaml", &doc);
        let (code, value, raw) = validate(&[
            "--file",
            deployment.to_str().unwrap(),
            "--host",
            host.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{raw}");
        assert_eq!(value["effective"]["context"], expected, "{raw}");
    }
}

/// #219: the issue's 16 GB laptop card (a 15052 MiB managed limit, 14.7 GiB)
/// as the host document's only GPU, with an SGLang profile.
fn laptop_card(root: &Path) -> Value {
    let mut host = host_document(root);
    host["resource_policy"]["domains"] = json!({
        "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB",
                   "parked_limit": "16GiB", "host_kv_limit": "4GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "15052MiB",
                 "free_reserve": "1324MiB", "parked_limit": "2GiB"}
    });
    host["resource_policy"]["devices"] = json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = json!("sglang");
    profile["args"] = json!([]);
    profile["security"]["admin_credential_ref"] = json!("secret://admin-key");
    host
}

/// `validate config` in text form, as an operator reads it.
fn validate_text(args: &[&str]) -> String {
    let out = support::capyctl()
        .args(["validate", "config"])
        .args(args)
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// #219 (ADR 0019 amendment A1, §3): on a discrete card `validate config
/// --host` shows the first start's charge on the card. The issue's declared
/// 12.86 GiB request starts at its Ready charge (14.1 GiB, within the
/// card's 14.7 GiB); it was shown 15.4 GiB, the graph allowance on top, and
/// could never start. A declared startup peak whose charge the card cannot
/// hold is named, with the card and its limit, before any start is refused.
// T14 T26
#[test]
fn validate_shows_the_first_start_on_a_card_and_one_that_can_never_start() {
    let dir = tempfile::tempdir().unwrap();
    let host = write(dir.path(), "host.yaml", &laptop_card(dir.path()));
    let overhead = capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES;
    let request = 13169_i64 << 20;
    let mut doc = derived(json!({"request": "13169MiB", "kv_cache": "1GiB"}));
    doc["residency"] = json!("restart_only");
    let deployment = write(dir.path(), "deployment.yaml", &doc);
    let args = [
        "--file",
        deployment.to_str().unwrap(),
        "--host",
        host.to_str().unwrap(),
    ];
    let (code, value, raw) = validate(&args);
    assert_eq!(code, 0, "{raw}");
    let startup = &value["effective"]["startup"];
    assert_eq!(startup["device_bytes"], json!(request + overhead), "{raw}");
    assert_eq!(startup["provenance"], "default", "{raw}");
    assert!(startup.get("over_limit").is_none(), "{raw}");
    assert!(!validate_text(&args).contains("cannot start there"));

    // The same file, now with a startup peak the card cannot hold.
    doc["engine_config"]["memory"]["startup"] = json!("14GiB");
    write(dir.path(), "deployment.yaml", &doc);
    let (code, value, raw) = validate(&args);
    assert_eq!(code, 0, "{raw}");
    let startup = &value["effective"]["startup"];
    assert_eq!(startup["provenance"], "declared", "{raw}");
    assert_eq!(
        startup["over_limit"],
        json!({"host": "lab", "domain": "gpu0", "bytes": (14_i64 << 30) + overhead,
               "limit_bytes": 15052_i64 << 20}),
        "{raw}"
    );
    let text = validate_text(&args);
    assert!(
        text.contains(
            "a start on gpu0 of host lab needs 15.2 GiB, above the 14.7 GiB it manages, so it \
             cannot start there"
        ),
        "{text}"
    );
}

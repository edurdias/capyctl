//! Owner decision 2026-09-25 (ADR 0014 amendment "minimal deployment file"):
//! `capyctl deploy model --file` accepts a file of three fields on standalone,
//! and `capyctl validate config` shows the document with its defaults.
//!
//! The real `capyctl` binary against a standalone running the testkit's Fake
//! installation. Fake engine only; nothing here qualifies an engine recipe
//! (SPEC §18). No test reaches the network: a Hugging Face reference is pinned
//! against a loopback endpoint.
mod support;

use serde_json::{json, Value};
use std::path::Path;
use std::process::Output;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn capyctl(state: &Path, home: &Path, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut command = support::capyctl();
    command
        .env("CAPYCTL_STATE_DIR", state)
        .env("HOME", home)
        .args(args)
        .arg("--json");
    for (key, value) in extra {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn json_of(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// A loopback stand-in for the Hugging Face API that says `main` of
/// `org/model` is at [`SHA`].
async fn hugging_face() -> String {
    let app = axum::Router::new().route(
        "/api/models/org/model/revision/main",
        axum::routing::get(|| async { axum::Json(json!({"id": "org/model", "sha": SHA})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    origin
}

// T14 T10 (owner decision 2026-09-25): three fields deploy on standalone. The
// defaults are the host's: its one vLLM profile at its published revision,
// its GPU, the residency its profile allows (the Fake opts out of deep
// parking, so restart_only), and a KV cache sized from its memory; the model
// path's `~/` is the CLI user's home. The request derives from the weights,
// so the revision waits, provisional, for the checkpoint digest (ADR 0014
// §7), which the Fake bindings never measure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_minimal_file_deploys_on_standalone() {
    let dir = support::safe_state_dir();
    let home = tempfile::TempDir::new_in(dir.path()).unwrap();
    let app = support::boot(dir.path()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let router = app.management_router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let management = [(capyctl_cli::roles::MANAGEMENT_ADDR_ENV, address.as_str())];

    let file = dir.path().join("mini.yaml");
    std::fs::write(&file, "name: mini\nengine: vllm\nmodel: ~/models/mini\n").unwrap();
    // `validate config` shows the completed document.
    let validated = json_of(&capyctl(
        dir.path(),
        home.path(),
        &["validate", "config", "--file", file.to_str().unwrap()],
        &[],
    ));
    let model_path = home.path().join("models/mini");
    assert_eq!(
        validated["document"],
        json!({
            "schema_version": 1, "kind": "deployment", "name": "mini",
            "runtime_profile": "vllm", "routes": ["mini"],
            "recipe": "standard", "recovery": "reconcile",
            "model": {"path": model_path, "content_fingerprint": "measured", "revision": "1"},
        })
    );

    let receipt = json_of(&capyctl(
        dir.path(),
        home.path(),
        &["deploy", "model", "--file", file.to_str().unwrap()],
        &management,
    ));
    assert!(receipt["deployment_id"].is_string(), "{receipt}");
    let inspected = json_of(&capyctl(
        dir.path(),
        home.path(),
        &["inspect", "deployment", "mini", "--effective-config"],
        &management,
    ));
    let effective = &inspected["effective"];
    assert_eq!(effective["routes"], json!(["mini"]), "{inspected}");
    assert_eq!(effective["recipe"], "standard");
    assert_eq!(effective["recovery"], "reconcile");
    assert_eq!(effective["residency"], "restart_only");
    assert_eq!(effective["model"]["source"]["path"], json!(model_path));
    assert_eq!(effective["model"]["content_fingerprint"], "measured");
    assert_eq!(effective["selected_devices"][0]["sharing"], "shared");
    let host = app.host_document();
    assert_eq!(
        effective["profile"]["revision"],
        host["runtime_profiles"]["local"]["revision"]
    );
    let provenance = &effective["engine_config"]["provenance"];
    assert_eq!(provenance["residency"], "capyctl default", "{provenance}");
    assert_eq!(provenance["memory.kv_cache"], "capyctl default");
    assert_eq!(provenance["memory.request"], "derived");
    let id = receipt["deployment_id"].as_str().unwrap();
    let revision = app.store.current_revision(id).unwrap().unwrap();
    let digest = app.store.checkpoint_digest(id, revision).unwrap().unwrap();
    assert!(digest.provisional, "sized once the checkpoint is measured");

    // A Hugging Face reference is pinned to the commit it names now before
    // the document leaves the CLI; the server only ever stores the commit.
    let origin = hugging_face().await;
    let hf = dir.path().join("hf.yaml");
    std::fs::write(&hf, "name: hf-mini\nengine: vllm\nmodel: {hf: org/model}\n").unwrap();
    let offline = capyctl(
        dir.path(),
        home.path(),
        &["validate", "config", "--file", hf.to_str().unwrap()],
        &[],
    );
    assert!(
        !offline.status.success(),
        "validate never contacts the network"
    );
    assert!(
        String::from_utf8_lossy(&offline.stderr).contains("deploy model --file"),
        "{}",
        String::from_utf8_lossy(&offline.stderr)
    );
    json_of(&capyctl(
        dir.path(),
        home.path(),
        &["deploy", "model", "--file", hf.to_str().unwrap()],
        &[management[0], ("HF_ENDPOINT", origin.as_str())],
    ));
    let inspected = json_of(&capyctl(
        dir.path(),
        home.path(),
        &["inspect", "deployment", "hf-mini", "--effective-config"],
        &management,
    ));
    assert_eq!(
        inspected["effective"]["model"]["source"],
        json!({"type": "huggingface", "repo": "org/model", "revision": SHA})
    );

    // T03 (owner rule 2026-09-25: every setting three ways): `--hf-endpoint`
    // wins over CAPYCTL_HF_ENDPOINT, which wins over HF_ENDPOINT. The losing
    // values would be refused (not https), so a pin proves which was used.
    let not_https = "http://mirror.example";
    for (name, args, extra) in [
        (
            "flag-mini",
            vec!["--hf-endpoint", origin.as_str()],
            vec![("CAPYCTL_HF_ENDPOINT", not_https), ("HF_ENDPOINT", not_https)],
        ),
        (
            "env-mini",
            vec![],
            vec![
                ("CAPYCTL_HF_ENDPOINT", origin.as_str()),
                ("HF_ENDPOINT", not_https),
            ],
        ),
    ] {
        let file = dir.path().join(format!("{name}.yaml"));
        std::fs::write(
            &file,
            format!("name: {name}\nengine: vllm\nmodel: {{hf: org/model}}\n"),
        )
        .unwrap();
        let mut argv = vec!["deploy", "model", "--file", file.to_str().unwrap()];
        argv.extend(args);
        let mut env = vec![management[0]];
        env.extend(extra);
        json_of(&capyctl(dir.path(), home.path(), &argv, &env));
    }
    let refused = capyctl(
        dir.path(),
        home.path(),
        &["deploy", "model", "--file", hf.to_str().unwrap()],
        &[management[0], ("CAPYCTL_HF_ENDPOINT", not_https)],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("CAPYCTL_HF_ENDPOINT"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    // T26 (final review I9): the short pin form in a file deploys, with the
    // host's sharing for the pinned GPU.
    let pinned = dir.path().join("pinned.yaml");
    std::fs::write(
        &pinned,
        "name: pinned\nengine: vllm\nmodel: ~/models/mini\ndevices: [{id: gpu0}]\n",
    )
    .unwrap();
    json_of(&capyctl(
        dir.path(),
        home.path(),
        &["deploy", "model", "--file", pinned.to_str().unwrap()],
        &management,
    ));
    let inspected = json_of(&capyctl(
        dir.path(),
        home.path(),
        &["inspect", "deployment", "pinned", "--effective-config"],
        &management,
    ));
    assert_eq!(
        inspected["effective"]["selected_devices"],
        json!([{"id": "gpu0", "sharing": "shared"}]),
        "{inspected}"
    );
    server.abort();
    let _ = app.shutdown().await;
}

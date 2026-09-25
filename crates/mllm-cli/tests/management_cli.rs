//! T10/T13: the shipped CLI uses the authenticated application-owned API.
mod support;

use mllm_config::effective::{Engine, ModelSource};
use serde_json::Value;
use std::process::Command;

fn cli(state: &std::path::Path, args: &[&str]) -> Value {
    let result = Command::new(env!("CARGO_BIN_EXE_mllm"))
        .env("MLLM_STATE_DIR", state)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

fn stdout(state: &std::path::Path, args: &[&str]) -> String {
    let result = Command::new(env!("CARGO_BIN_EXE_mllm"))
        .env("MLLM_STATE_DIR", state)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

/// T10, owner decision 2026-09-25: record views print an aligned table by
/// default, terminal or not; `--format json` (and `--json`) print the JSON
/// result byte for byte as `--output json` does, and as the plain command
/// printed before tables existed. Fake engine; not qualification.
fn record_views_print_tables_and_json_on_request(state: &std::path::Path, id: &str) {
    let table = stdout(state, &["list", "deployments"]);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines.len(), 2, "{table}");
    assert!(
        lines[0].starts_with("NAME        KIND")
            && lines[0].contains("   READY   REVISION   HOSTS"),
        "{table}"
    );
    assert!(lines[1].starts_with("cli-model   "), "{table}");
    assert!(lines[1].contains("   ready   1/1   "), "{table}");
    assert!(serde_json::from_str::<Value>(&table).is_err());
    for args in [
        &["list", "deployments"][..],
        &["status", "deployment", id][..],
    ] {
        let legacy = stdout(state, &[args, &["--output", "json"]].concat());
        let json = stdout(state, &[args, &["--format", "json"]].concat());
        assert_eq!(json, legacy, "--format json prints the unchanged JSON");
        assert_eq!(stdout(state, &[args, &["--json"]].concat()), legacy);
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(json, format!("{value}\n"), "one compact JSON line");
        assert_eq!(
            stdout(state, &[args, &["--format", "table"]].concat()),
            stdout(state, args),
            "--format table is the default"
        );
    }
    let status = stdout(state, &["status", "deployment", id]);
    let lines: Vec<&str> = status.lines().collect();
    assert!(lines[0].starts_with("NAME        KIND"), "{status}");
    assert!(lines[1].starts_with("cli-model   "), "{status}");
    assert_eq!(lines[2], "", "{status}");
    assert!(lines[3].starts_with("INSTANCE   HOST"), "{status}");
    assert!(lines[4].starts_with("0          "), "{status}");
}

// T10/T13: acceptance is durable, status is read-only, and inference keys cannot manage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_deploys_starts_observes_and_stops_through_management() {
    let dir = support::safe_state_dir();
    let app = support::boot(dir.path()).await;
    // An ephemeral loopback port the CLI children are pointed at, so a live
    // standalone or server holding the 7443 default does not collide.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    std::env::set_var(mllm_cli::roles::MANAGEMENT_ADDR_ENV, address.to_string());
    let router = app.management_router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let denied = reqwest::Client::new()
        .get(format!("http://{address}/management/v1/snapshot"))
        .bearer_auth(app.api_key())
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
    let config = mllm_cli::standalone_config::deployment_document(
        "cli-model",
        "cli-model",
        &ModelSource::Local {
            path: "/models/cli-model".into(),
        },
        Engine::Vllm,
        // The capacity the app was booted with (`support::test_memory`), not
        // this machine's: the deployment must be sized against the same host.
        support::TEST_CAPACITY_BYTES,
        mllm_cli::standalone_config::DEFAULT_REQUEST_DEADLINE,
        // ADR 0012: the Fake host opts out of deep parking, so its
        // generated deployment is restart_only.
        false,
        "local",
    );
    let path = dir.path().join("deployment.json");
    std::fs::write(&path, config.to_string()).unwrap();
    let result = cli(
        dir.path(),
        &[
            "deploy",
            "model",
            "--file",
            path.to_str().unwrap(),
            "--activate",
            "--wait",
        ],
    );
    assert_eq!(result["deployment"]["observed_state"], "ready");
    let id = result["deployment"]["id"].as_str().unwrap();
    let status = cli(
        dir.path(),
        &["status", "deployment", id, "--format", "json"],
    );
    assert_eq!(status["observed_state"], "ready");
    record_views_print_tables_and_json_on_request(dir.path(), id);
    // T32, SPEC §6.3: a plain delete is refused while the engine runs, and
    // changes nothing.
    let refused = Command::new(env!("CARGO_BIN_EXE_mllm"))
        .env("MLLM_STATE_DIR", dir.path())
        .args(["delete", "deployment", id])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("stop the deployment"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        cli(
            dir.path(),
            &["status", "deployment", id, "--format", "json"]
        )["observed_state"],
        "ready"
    );
    let request = ulid::Ulid::new().to_string();
    let receipt = cli(
        dir.path(),
        &["stop", "deployment", id, "--request-id", &request],
    );
    assert!(receipt["operation_id"].is_string());
    let replay = cli(
        dir.path(),
        &["stop", "deployment", id, "--request-id", &request],
    );
    assert_eq!(replay["operation_id"], receipt["operation_id"]);
    assert_eq!(replay["generation"], receipt["generation"]);
    for _ in 0..100 {
        let status = cli(
            dir.path(),
            &["status", "deployment", id, "--format", "json"],
        );
        if status["observed_state"] == "stopped" {
            let fresh = delete_by_name_replays_and_frees_the_name(dir.path(), id, &path);
            delete_with_stop_stops_waits_and_deletes(dir.path(), &fresh);
            server.abort();
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("stop did not settle");
}

/// T09 T10, SPEC §6.3 (W6): `delete deployment <name>` after verified cleanup
/// removes the deployment, an exact retry by request id returns the same
/// receipt, and deploying the same name again creates a new deployment id.
/// The new deployment is left running; its id is returned.
fn delete_by_name_replays_and_frees_the_name(
    state: &std::path::Path,
    id: &str,
    file: &std::path::Path,
) -> String {
    let request = ulid::Ulid::new().to_string();
    let receipt = cli(
        state,
        &[
            "delete",
            "deployment",
            "cli-model",
            "--request-id",
            &request,
        ],
    );
    assert_eq!(receipt["deployment_id"], id);
    assert!(receipt["operation_id"].is_string());
    let replay = cli(
        state,
        &[
            "delete",
            "deployment",
            "cli-model",
            "--request-id",
            &request,
        ],
    );
    assert_eq!(replay, receipt);
    let gone = Command::new(env!("CARGO_BIN_EXE_mllm"))
        .env("MLLM_STATE_DIR", state)
        .args(["status", "deployment", "cli-model"])
        .output()
        .unwrap();
    assert!(!gone.status.success());
    let created = cli(
        state,
        &[
            "deploy",
            "model",
            "--file",
            file.to_str().unwrap(),
            "--activate",
            "--wait",
        ],
    );
    assert_eq!(created["deployment"]["observed_state"], "ready");
    let fresh = created["deployment"]["id"].as_str().unwrap().to_owned();
    assert_ne!(fresh, id);
    assert_eq!(
        cli(
            state,
            &["status", "deployment", "cli-model", "--format", "json"]
        )["id"],
        fresh.as_str()
    );
    fresh
}

/// T09 T10, owner decision 2026-09-23: `delete deployment <name> --stop` on a
/// running deployment stops every instance, waits for verified cleanup and
/// deletes, in one command. An exact rerun by request id replays the delete's
/// receipt. Fake engine; not qualification of any native engine recipe.
fn delete_with_stop_stops_waits_and_deletes(state: &std::path::Path, id: &str) {
    let request = ulid::Ulid::new().to_string();
    let args = [
        "delete",
        "deployment",
        "cli-model",
        "--stop",
        "--request-id",
        &request,
    ];
    let report = cli(state, &args);
    assert_eq!(report["deleted"], true, "{report}");
    assert_eq!(report["deployment_id"], id, "{report}");
    assert!(report["operation_id"].is_string(), "{report}");
    assert_eq!(cli(state, &args), report, "a rerun replays the receipt");
    let gone = Command::new(env!("CARGO_BIN_EXE_mllm"))
        .env("MLLM_STATE_DIR", state)
        .args(["status", "deployment", "cli-model"])
        .output()
        .unwrap();
    assert!(!gone.status.success());
}

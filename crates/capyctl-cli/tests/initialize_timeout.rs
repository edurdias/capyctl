//! Owner decision 2026-09-22 (1), ADR 0014 amendment A1: deployment timeouts
//! in `validate config`, and the per-command `--initialize-timeout` override.
//! CPU-only; no engine runs here.

mod support;

use std::path::{Path, PathBuf};

use capyctl_cli::grammar::{parse_invocation, Command as Cli, LifecycleAction};
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

/// T14: resolution against a host shows each timeout with its provenance.
// T14
#[test]
fn validate_shows_declared_and_derived_timeouts() {
    let dir = tempfile::tempdir().unwrap();
    let host = write(dir.path(), "host.yaml", &host_document(dir.path()));
    let mut doc = golden()["input"]["deployment"].clone();
    doc["timeouts"] = json!({"initialize": "4m"});
    let deployment = write(dir.path(), "deployment.yaml", &doc);
    let (code, value, raw) = validate(&[
        "--file",
        deployment.to_str().unwrap(),
        "--host",
        host.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{raw}");
    let timeouts = &value["effective"]["timeouts"];
    assert_eq!(timeouts["initialize_ms"], 240_000, "{raw}");
    assert_eq!(timeouts["provenance"]["initialize"], "declared", "{raw}");
    assert_eq!(timeouts["provenance"]["wake"], "derived", "{raw}");
    // Pending weights: the conservative value, lowered to the 300 s request
    // deadline the document declares.
    assert_eq!(timeouts["wake_ms"], 300_000, "{raw}");
}

/// T03: a timeout beyond the request deadline, below its floor, or with an
/// unknown field is a named error, with or without a host document.
// T03
#[test]
fn validate_refuses_invalid_timeouts_without_a_host() {
    let dir = tempfile::tempdir().unwrap();
    for (block, needle) in [
        (json!({"initialize": "301s"}), "timeouts.initialize"),
        (json!({"wake": "1s"}), "timeouts.wake"),
        (json!({"deadline": "60s"}), "timeouts.deadline"),
    ] {
        let mut doc = golden()["input"]["deployment"].clone();
        doc["timeouts"] = block.clone();
        let deployment = write(dir.path(), "deployment.yaml", &doc);
        let (code, value, raw) = validate(&["--file", deployment.to_str().unwrap()]);
        assert_eq!(code, 2, "{block}: {raw}");
        assert_eq!(value["code"], "invalid_config", "{raw}");
        assert!(value["message"].as_str().unwrap().contains(needle), "{raw}");
    }
}

/// The override is accepted on the commands that start, and nowhere else.
// T08 T20
#[test]
fn initialize_timeout_is_a_start_option() {
    let invocation = parse_invocation([
        "capyctl",
        "start",
        "deployment",
        "d",
        "--initialize-timeout",
        "20m",
    ])
    .unwrap();
    assert_eq!(invocation.initialize_timeout_ms, Some(1_200_000));
    assert!(matches!(
        invocation.command,
        Cli::Lifecycle { action: LifecycleAction::Start, ref deployment } if deployment == "d"
    ));
    let invocation = parse_invocation([
        "capyctl",
        "start",
        "instance",
        "d/1",
        "--initialize-timeout",
        "90s",
    ])
    .unwrap();
    assert_eq!(invocation.initialize_timeout_ms, Some(90_000));
    let invocation = parse_invocation([
        "capyctl",
        "deploy",
        "model",
        "--file",
        "d.yaml",
        "--activate",
        "--initialize-timeout",
        "1h",
    ])
    .unwrap();
    assert_eq!(invocation.initialize_timeout_ms, Some(3_600_000));
    assert_eq!(
        parse_invocation(["capyctl", "start", "deployment", "d"])
            .unwrap()
            .initialize_timeout_ms,
        None
    );
    // Without --activate a deploy starts nothing, so there is nothing to bound.
    assert!(parse_invocation([
        "capyctl",
        "deploy",
        "model",
        "--file",
        "d.yaml",
        "--initialize-timeout",
        "1h"
    ])
    .is_err());
    assert!(parse_invocation([
        "capyctl",
        "stop",
        "deployment",
        "d",
        "--initialize-timeout",
        "1m"
    ])
    .is_err());
    for bad in ["0s", "ten", "10GiB", "-5s"] {
        assert!(
            parse_invocation([
                "capyctl",
                "start",
                "deployment",
                "d",
                "--initialize-timeout",
                bad
            ])
            .is_err(),
            "{bad}"
        );
    }
}

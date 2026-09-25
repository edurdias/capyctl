//! SPEC §14 / §15.3: `mllm validate config --file` checks server, host,
//! standalone and deployment documents offline with the same strict parsers
//! and resolution the product uses, reporting named errors and performing no
//! side effects.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

const GOLDEN: &str = include_str!("../../mllm-config/tests/fixtures/effective-vllm-golden.json");

fn golden() -> Value {
    serde_json::from_str(GOLDEN).unwrap()
}

/// The golden host input made into a complete host role document.
fn host_document(root: &Path) -> Value {
    let mut host = golden()["input"]["host"].clone();
    let state = root.join("host-state");
    host["state_dir"] = json!(state);
    host["identity_dir"] = json!(state.join("identity"));
    host
}

fn deployment_document() -> Value {
    golden()["input"]["deployment"].clone()
}

fn write(root: &Path, name: &str, text: &str) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

fn validate(args: &[&str]) -> (i32, Value, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mllm"))
        .arg("validate")
        .arg("config")
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

fn listing(root: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    entries
}

// T03
#[test]
fn a_valid_host_document_is_reported_valid_without_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let text = serde_json::to_string_pretty(&host_document(dir.path())).unwrap();
    let file = write(dir.path(), "host.yaml", &text);
    let before = listing(dir.path());
    let (code, value, raw) = validate(&["--file", file.to_str().unwrap()]);
    assert_eq!(code, 0, "{raw}");
    assert_eq!(value["valid"], true, "{raw}");
    assert_eq!(value["kind"], "host", "{raw}");
    // No state directory, identity, or rewritten configuration appears.
    assert_eq!(listing(dir.path()), before);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
}

// T03
#[test]
fn duplicate_yaml_keys_are_a_named_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        dir.path(),
        "host.yaml",
        "schema_version: 1\nkind: host\nname: a\nname: b\n",
    );
    let before = listing(dir.path());
    let (code, value, raw) = validate(&["--file", file.to_str().unwrap()]);
    assert_eq!(code, 2, "{raw}");
    assert_eq!(value["code"], "invalid_config", "{raw}");
    assert!(
        value["message"].as_str().unwrap().contains("duplicate_key"),
        "{raw}"
    );
    assert_eq!(listing(dir.path()), before);
}

// T03
#[test]
fn a_missing_explicit_file_fails_without_generating_one() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("absent.yaml");
    let (code, value, raw) = validate(&["--file", file.to_str().unwrap()]);
    assert_eq!(code, 2, "{raw}");
    assert_eq!(value["code"], "invalid_config", "{raw}");
    assert!(!file.exists());
    assert!(listing(dir.path()).is_empty());
}

// T03
#[test]
fn an_unknown_field_is_named_with_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_document(dir.path());
    host["resource_policy"]["surprise"] = json!(1);
    let file = write(dir.path(), "host.yaml", &host.to_string());
    let (code, value, raw) = validate(&["--file", file.to_str().unwrap()]);
    assert_eq!(code, 2, "{raw}");
    let message = value["message"].as_str().unwrap();
    assert!(message.contains("unknown_field"), "{raw}");
    assert!(message.contains("resource_policy.surprise"), "{raw}");
}

// T03
#[test]
fn an_unknown_document_kind_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        dir.path(),
        "x.yaml",
        "schema_version: 1\nkind: printer\nname: x\n",
    );
    let (code, value, raw) = validate(&["--file", file.to_str().unwrap()]);
    assert_eq!(code, 2, "{raw}");
    assert!(value["message"].as_str().unwrap().contains("kind"), "{raw}");
}

// T03
#[test]
fn a_host_whose_policy_does_not_normalize_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_document(dir.path());
    // A device naming a domain the host never declared.
    host["resource_policy"]["devices"]["gpu0"]["domain"] = json!("missing");
    let file = write(dir.path(), "host.yaml", &host.to_string());
    let (code, value, raw) = validate(&["--file", file.to_str().unwrap()]);
    assert_eq!(code, 2, "{raw}");
    assert_eq!(value["code"], "invalid_config", "{raw}");
}

// T03 T08
#[test]
fn a_deployment_resolves_against_the_given_host_document() {
    let dir = tempfile::tempdir().unwrap();
    let host = write(
        dir.path(),
        "host.yaml",
        &host_document(dir.path()).to_string(),
    );
    let deployment = write(
        dir.path(),
        "deployment.yaml",
        &deployment_document().to_string(),
    );
    let before = listing(dir.path());
    let (code, value, raw) = validate(&[
        "--file",
        deployment.to_str().unwrap(),
        "--host",
        host.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{raw}");
    assert_eq!(value["valid"], true, "{raw}");
    assert_eq!(value["kind"], "deployment", "{raw}");
    assert_eq!(value["resolved_against"], "lab", "{raw}");
    assert_eq!(value["effective"]["residency"], "deep", "{raw}");
    assert_eq!(listing(dir.path()), before);
}

// T03 T08
#[test]
fn a_deployment_that_does_not_resolve_on_the_host_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let host = write(
        dir.path(),
        "host.yaml",
        &host_document(dir.path()).to_string(),
    );
    let mut doc = deployment_document();
    doc["runtime_profile"] = json!("not-on-this-host");
    let deployment = write(dir.path(), "deployment.yaml", &doc.to_string());
    let (code, value, raw) = validate(&[
        "--file",
        deployment.to_str().unwrap(),
        "--host",
        host.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{raw}");
    assert_eq!(value["code"], "invalid_config", "{raw}");
    assert!(
        value["message"].as_str().unwrap().contains("deployment"),
        "{raw}"
    );
}

// T03
#[test]
fn a_deployment_without_a_host_is_checked_structurally_only() {
    let dir = tempfile::tempdir().unwrap();
    let deployment = write(
        dir.path(),
        "deployment.yaml",
        &deployment_document().to_string(),
    );
    let (code, value, raw) = validate(&["--file", deployment.to_str().unwrap()]);
    assert_eq!(code, 0, "{raw}");
    assert_eq!(value["valid"], true, "{raw}");
    assert_eq!(value["resolved_against"], Value::Null, "{raw}");

    let mut doc = deployment_document();
    doc["instances"] = json!(0);
    let bad = write(dir.path(), "bad.yaml", &doc.to_string());
    let (code, _, raw) = validate(&["--file", bad.to_str().unwrap()]);
    assert_eq!(code, 2, "{raw}");
}

// T03
#[test]
fn host_is_only_accepted_for_a_deployment() {
    let dir = tempfile::tempdir().unwrap();
    let host = write(
        dir.path(),
        "host.yaml",
        &host_document(dir.path()).to_string(),
    );
    let (code, value, raw) = validate(&[
        "--file",
        host.to_str().unwrap(),
        "--host",
        host.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{raw}");
    assert_eq!(value["code"], "invalid_config", "{raw}");
}

// T03
#[test]
fn a_server_document_is_checked_by_the_server_parser() {
    let dir = tempfile::tempdir().unwrap();
    let text = mllm_config::remote_roles::ServerConfig::template(&dir.path().join("srv"));
    let file = write(dir.path(), "server.yaml", &text);
    let (code, value, raw) = validate(&["--file", file.to_str().unwrap()]);
    assert_eq!(code, 0, "{raw}");
    assert_eq!(value["kind"], "server", "{raw}");
    assert!(!dir.path().join("srv").exists());

    let mut doc: Value = serde_json::from_str(&text).unwrap();
    doc["identity_dir"] = json!("/elsewhere");
    let bad = write(dir.path(), "bad.yaml", &doc.to_string());
    let (code, _, raw) = validate(&["--file", bad.to_str().unwrap()]);
    assert_eq!(code, 2, "{raw}");
}

// T03 T17: `shutdown.drain_timeout` is validated in every role document.
#[test]
fn an_out_of_range_drain_timeout_is_a_named_error_in_every_role_document() {
    let dir = tempfile::tempdir().unwrap();
    let server: Value = serde_json::from_str(&mllm_config::remote_roles::ServerConfig::template(
        &dir.path().join("srv"),
    ))
    .unwrap();
    let standalone = json!({"schema_version": 1, "kind": "standalone", "name": "local"});
    for (name, mut document) in [
        ("server.yaml", server),
        ("host.yaml", host_document(dir.path())),
        ("standalone.yaml", standalone),
    ] {
        document["shutdown"] = json!({"drain_timeout": "45s"});
        let file = write(dir.path(), name, &document.to_string());
        let (code, _, raw) = validate(&["--file", file.to_str().unwrap()]);
        assert_eq!(code, 0, "{name}: {raw}");
        document["shutdown"] = json!({"drain_timeout": "601s"});
        let file = write(dir.path(), name, &document.to_string());
        let (code, value, raw) = validate(&["--file", file.to_str().unwrap()]);
        assert_eq!(code, 2, "{name}: {raw}");
        assert!(
            value["message"]
                .as_str()
                .unwrap()
                .contains("shutdown.drain_timeout"),
            "{name}: {raw}"
        );
    }
}

// T17 T33 (owner decision 2026-09-23): the server's control-session heartbeat
// bounds are validated offline, with the field named.
#[test]
fn server_heartbeat_bounds_are_validated_with_the_field_named() {
    let dir = tempfile::tempdir().unwrap();
    let mut server: Value = serde_json::from_str(
        &mllm_config::remote_roles::ServerConfig::template(&dir.path().join("srv")),
    )
    .unwrap();
    server["control"] = json!({"heartbeat_suspend_after": "5s", "heartbeat_lost_after": "30s"});
    let file = write(dir.path(), "server.yaml", &server.to_string());
    let (code, _, raw) = validate(&["--file", file.to_str().unwrap()]);
    assert_eq!(code, 0, "{raw}");
    for (control, field) in [
        (
            json!({"heartbeat_suspend_after": "1s"}),
            "control.heartbeat_suspend_after",
        ),
        (
            json!({"heartbeat_lost_after": "11m"}),
            "control.heartbeat_lost_after",
        ),
        (
            json!({"heartbeat_suspend_after": "20s", "heartbeat_lost_after": "15s"}),
            "control.heartbeat_lost_after",
        ),
    ] {
        server["control"] = control;
        let file = write(dir.path(), "server.yaml", &server.to_string());
        let (code, value, raw) = validate(&["--file", file.to_str().unwrap()]);
        assert_eq!(code, 2, "{raw}");
        assert!(
            value["message"].as_str().unwrap().contains(field),
            "{field}: {raw}"
        );
    }
}

/// The checkout's `docs/examples` directory.
fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples")
}

// T03: every documented example is a document the product accepts, through
// the same parsers `mllm validate config` runs, and every server-mode
// deployment example also resolves against the host example.
#[test]
fn every_documented_example_passes_validate_config() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(examples())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "yaml" || e == "yml"))
        .collect();
    files.sort();
    let mut kinds = Vec::new();
    for file in &files {
        let (code, value, raw) = validate(&["--file", file.to_str().unwrap()]);
        assert_eq!(code, 0, "{}: {raw}", file.display());
        assert_eq!(value["valid"], true, "{}: {raw}", file.display());
        let kind = value["kind"].as_str().unwrap().to_owned();
        // The quickstart's standalone deployment names standalone's `local`
        // engine, which host.yaml does not publish; `site_quickstart.rs`
        // places it on a fresh standalone instead.
        let standalone = file
            .file_name()
            .is_some_and(|n| n == "deployment-standalone.yaml");
        if kind == "deployment" && !standalone {
            let host = examples().join("host.yaml");
            let (code, value, raw) = validate(&[
                "--file",
                file.to_str().unwrap(),
                "--host",
                host.to_str().unwrap(),
            ]);
            assert_eq!(code, 0, "{} against host.yaml: {raw}", file.display());
            assert_eq!(value["resolved_against"], "gpu-box", "{raw}");
        }
        kinds.push(kind);
    }
    kinds.sort();
    kinds.dedup();
    assert_eq!(kinds, ["deployment", "host", "server", "standalone"]);
}

// T03 (ADR 0013 §2, §3): resolving against a host runs the server's per-host
// step: unnamed device claims take the host's devices, and a host outside the
// allowed set is refused.
#[test]
fn a_multi_host_deployment_resolves_on_an_allowed_host_only() {
    let dir = tempfile::tempdir().unwrap();
    let host = examples().join("host.yaml");
    let source = std::fs::read_to_string(examples().join("deployment-multinode.yaml")).unwrap();
    let elsewhere = write(
        dir.path(),
        "elsewhere.yaml",
        &source.replace(
            "hosts: [\"gpu-box\", \"workstation\"]",
            "hosts: [\"workstation\", \"server\"]",
        ),
    );
    let (code, value, raw) = validate(&[
        "--file",
        elsewhere.to_str().unwrap(),
        "--host",
        host.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{raw}");
    assert!(
        value["message"].as_str().unwrap().contains("gpu-box"),
        "{raw}"
    );
}

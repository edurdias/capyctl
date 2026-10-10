//! SPEC §14 / §15.3: `capyctl validate config --file` checks server, host,
//! standalone and deployment documents offline with the same strict parsers
//! and resolution the product uses, reporting named errors and performing no
//! side effects.

mod support;

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

const GOLDEN: &str = include_str!("../../capyctl-config/tests/fixtures/effective-vllm-golden.json");

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
    let out = support::capyctl()
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
    // ADR 0018 §7: deploy refuses a profile the host does not declare with
    // `profile_not_published`; validate names the same refusal.
    assert_eq!(code, 24, "{raw}");
    assert_eq!(value["code"], "profile_not_published", "{raw}");
    let message = value["message"].as_str().unwrap();
    assert!(
        message.contains("not-on-this-host") && message.contains("capyctl engine add"),
        "{raw}"
    );
}

// T03 T16 (ADR 0013 §2; found walking the guides 2026-09-25): a placement
// selector the host's labels do not satisfy is refused by validate, as deploy
// refuses it (`selector_mismatch`); it used to be accepted.
#[test]
fn a_selector_the_host_labels_do_not_match_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let host = write(
        dir.path(),
        "host.yaml",
        &host_document(dir.path()).to_string(),
    );
    let mut doc = deployment_document();
    doc["placement"] = json!({"selector": {"accelerator": "h100"}});
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
        value["message"].as_str().unwrap().contains("selector"),
        "{raw}"
    );
}

// T03 T07 (ADR 0018 §2): a runtime profile registered with `capyctl engine add`
// lives in the engines.yaml beside the host document, where the host role
// merges it and deploy finds it published; validate resolves against the same
// merged document instead of refusing the profile as unknown.
#[test]
fn a_profile_registered_beside_the_host_document_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let host = write(
        dir.path(),
        "host.yaml",
        &host_document(dir.path()).to_string(),
    );
    let profile = host_document(dir.path())["runtime_profiles"]["local"].clone();
    write(
        dir.path(),
        "engines.yaml",
        &json!({"schema_version": 1, "kind": "engines",
            "runtime_profiles": {"registered": profile}})
        .to_string(),
    );
    let mut doc = deployment_document();
    doc["runtime_profile"] = json!("registered");
    let deployment = write(dir.path(), "deployment.yaml", &doc.to_string());
    let (code, value, raw) = validate(&[
        "--file",
        deployment.to_str().unwrap(),
        "--host",
        host.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{raw}");
    assert_eq!(value["valid"], true, "{raw}");
    // What only a running server can check is named, not implied.
    let unchecked = value["requires_server"].to_string();
    assert!(
        unchecked.contains("host_unpublished") && unchecked.contains("route_conflict"),
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
    // SPEC §15.3: it says what it did not check, and how to check it.
    let unchecked = value["requires_server"].to_string();
    assert!(
        unchecked.contains("--host") && unchecked.contains("profile_not_published"),
        "{raw}"
    );

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
    let text = capyctl_config::remote_roles::ServerConfig::template(&dir.path().join("srv"));
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
    let server: Value = serde_json::from_str(
        &capyctl_config::remote_roles::ServerConfig::template(&dir.path().join("srv")),
    )
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
        &capyctl_config::remote_roles::ServerConfig::template(&dir.path().join("srv")),
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
// the same parsers `capyctl validate config` runs, and every server-mode
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
    let mut minimal = false;
    for file in &files {
        let (code, value, raw) = validate(&["--file", file.to_str().unwrap()]);
        assert_eq!(code, 0, "{}: {raw}", file.display());
        assert_eq!(value["valid"], true, "{}: {raw}", file.display());
        let kind = value["kind"].as_str().unwrap().to_owned();
        // The quickstart's standalone deployment names standalone's `local`
        // engine, which host.yaml does not publish; `site_quickstart.rs`
        // places it on a fresh standalone instead.
        // The TensorFold and llama.cpp examples name the `tensorfold` and
        // `llamacpp` profiles, which host.yaml does not declare either; they
        // are checked on their own.
        let standalone = file.file_name().is_some_and(|n| {
            n == "deployment-standalone.yaml"
                || n == "deployment-tensorfold.yaml"
                || n == "deployment-llamacpp.yaml"
        });
        // ADR 0028 §2 (OD8): the group example resolves on both hosts it
        // names; `a_group_validates_against_every_named_host` checks it.
        let group = file.ends_with("deployment-multinode.yaml");
        if kind == "deployment" && !standalone && !group {
            let host = examples().join("host.yaml");
            let (code, value, raw) = validate(&[
                "--file",
                file.to_str().unwrap(),
                "--host",
                host.to_str().unwrap(),
            ]);
            assert_eq!(code, 0, "{} against host.yaml: {raw}", file.display());
            assert_eq!(value["resolved_against"], "gpu-box", "{raw}");
            // T14 (owner decision 2026-09-25): the minimal example resolves
            // to a complete document on the example host.
            if file.ends_with("deployment-minimal.yaml") {
                assert_eq!(
                    value["document"]["routes"],
                    serde_json::json!(["coding-small"])
                );
                assert_eq!(value["document"]["runtime_profile"], "vllm");
                assert!(
                    value["document"]["runtime_profile_revision"].is_u64(),
                    "{raw}"
                );
                assert_eq!(value["document"]["devices"][0]["id"], "gpu0");
                assert_eq!(value["effective"]["residency"], "deep", "{raw}");
                minimal = true;
            }
        }
        kinds.push(kind);
    }
    kinds.sort();
    kinds.dedup();
    assert_eq!(kinds, ["deployment", "host", "server", "standalone"]);
    assert!(
        minimal,
        "docs/examples/deployment-minimal.yaml is validated"
    );
    // The per-engine examples of docs/guide/engines.md are among them.
    for engine in ["vllm", "sglang", "tensorfold", "llamacpp"] {
        let name = format!("deployment-{engine}.yaml");
        assert!(
            files.iter().any(|f| f.ends_with(&name)),
            "docs/examples/{name} is validated"
        );
    }
}

// T03 T26 (ADR 0019): the discrete-GPU host example validates, and the
// minimal deployment resolves on it to a budget charging both the GPU's
// device domain and the system domain.
#[test]
fn the_discrete_host_example_validates() {
    let host = examples().join("host-discrete.yaml");
    let (code, value, raw) = validate(&["--file", host.to_str().unwrap()]);
    assert_eq!(code, 0, "{raw}");
    assert_eq!(value["valid"], true, "{raw}");
    assert_eq!(value["kind"], "host", "{raw}");
    let minimal = examples().join("deployment-minimal.yaml");
    let (code, value, raw) = validate(&[
        "--file",
        minimal.to_str().unwrap(),
        "--host",
        host.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{raw}");
    // Its weights are unknown offline, so it is provisional (final review
    // I10); a deployment stating its memory shows the budget.
    assert_eq!(value["provisional"], true, "{raw}");
    let dir = tempfile::tempdir().unwrap();
    let sized = write(
        dir.path(),
        "sized.yaml",
        "name: sized\nengine: vllm\nmodel: coding-small-r1\nengine_config:\n  memory:\n    request: 12GiB\n    kv_cache: 2GiB\n",
    );
    let (code, value, raw) = validate(&[
        "--file",
        sized.to_str().unwrap(),
        "--host",
        host.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{raw}");
    let domains: Vec<&str> = value["effective"]["resources"]["ready"]["allocations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["domain"].as_str().unwrap())
        .collect();
    assert_eq!(domains, ["gpu0", "system"], "{raw}");
}

// T03 (ADR 0013 §2, §3): resolving against a host runs the server's per-host
// step: unnamed device claims take the host's devices, and a host outside the
// allowed set is refused.
#[test]
fn a_multi_host_deployment_resolves_on_an_allowed_host_only() {
    let dir = tempfile::tempdir().unwrap();
    let host = examples().join("host.yaml");
    let source = std::fs::read_to_string(examples().join("deployment-spread.yaml")).unwrap();
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

/// The checkout's root; the example paths below are relative to it.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `validate config` of a checkout-relative deployment against the given
/// host documents (checkout-relative or absolute).
fn validate_group(deployment: &str, hosts: &[&str]) -> (i32, Value, String) {
    let resolve = |path: &str| repo().join(path).to_str().unwrap().to_owned();
    let mut args = vec!["--file".to_owned(), resolve(deployment)];
    for host in hosts {
        args.push("--host".into());
        args.push(resolve(host));
    }
    validate(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

/// The second example host (`docs/examples/host-b.yaml`) as a document named
/// `name`, with state under `root` and the peer address given (none: the
/// groups block removed).
fn group_host_document(root: &Path, name: &str, peer: Option<&str>) -> Value {
    let text = std::fs::read_to_string(examples().join("host-b.yaml")).unwrap();
    let mut host = capyctl_config::parse_document(&text).unwrap();
    let state = root.join(format!("{name}-state"));
    host["name"] = json!(name);
    host["state_dir"] = json!(state);
    host["identity_dir"] = json!(state.join("identity"));
    match peer {
        Some(peer) => host["resource_policy"]["groups"] = json!({"peer_address": peer}),
        None => {
            host["resource_policy"]
                .as_object_mut()
                .unwrap()
                .remove("groups");
        }
    }
    host
}

/// As [`group_host_document`], with the `vllm` profile on another build.
fn group_host_document_other_build(root: &Path, name: &str, peer: &str) -> Value {
    let mut host = group_host_document(root, name, Some(peer));
    host["runtime_profiles"]["vllm"]["build_fingerprint"] = json!("vllm 0.29.1");
    host
}

/// As [`group_host_document`], with the `vllm` profile approving `approved`.
fn group_host_document_approving(root: &Path, name: &str, peer: &str, approved: &[&str]) -> Value {
    let mut host = group_host_document(root, name, Some(peer));
    host["runtime_profiles"]["vllm"]["security"]["approved_env"] = json!(approved);
    host
}

/// The multinode example placed on `host-a` and `host-b`, setting one
/// engine environment variable.
fn group_deployment_with_env(name: &str, value: &str) -> Value {
    let text = std::fs::read_to_string(examples().join("deployment-multinode.yaml")).unwrap();
    let mut deployment = capyctl_config::parse_document(&text).unwrap();
    deployment["placement"]["hosts"] = json!(["host-a", "host-b"]);
    deployment["engine_config"]["env"] = json!({ name: value });
    deployment
}

// T03 (ADR 0028 §2, §3; OD8, decided 2026-10-06): the multinode example
// validates against its two host documents, one resolution per named host,
// the head first.
#[test]
fn a_group_validates_against_every_named_host() {
    let (code, value, raw) = validate_group(
        "docs/examples/deployment-multinode.yaml",
        &["docs/examples/host.yaml", "docs/examples/host-b.yaml"],
    );
    assert_eq!(code, 0, "{raw}");
    assert_eq!(value["hosts"].as_array().unwrap().len(), 2, "{raw}");
    assert_eq!(
        value["resolved_against"],
        json!(["gpu-box", "host-b"]),
        "{raw}"
    );
    assert_eq!(value["hosts"][0]["resolved_against"], "gpu-box", "{raw}");
    assert_eq!(value["hosts"][1]["resolved_against"], "host-b", "{raw}");
    // The order of the documents on the command line does not matter.
    let (code, value, raw) = validate_group(
        "docs/examples/deployment-multinode.yaml",
        &["docs/examples/host-b.yaml", "docs/examples/host.yaml"],
    );
    assert_eq!(code, 0, "{raw}");
    assert_eq!(
        value["resolved_against"],
        json!(["gpu-box", "host-b"]),
        "{raw}"
    );
}

// T14: a named host without a peer address is refused.
#[test]
fn a_named_host_without_a_peer_address_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let host_b = write(
        dir.path(),
        "host-b.yaml",
        &group_host_document(dir.path(), "host-b", None).to_string(),
    );
    let (code, value, raw) = validate_group(
        "docs/examples/deployment-multinode.yaml",
        &["docs/examples/host.yaml", host_b.to_str().unwrap()],
    );
    assert_eq!(code, 2, "{raw}");
    assert_eq!(value["code"], "peer_address_missing", "{raw}");
    assert!(
        value["message"].as_str().unwrap().contains("host-b"),
        "{raw}"
    );
}

// T14: a profile whose build differs on one named host is refused.
#[test]
fn a_profile_mismatch_on_one_host_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let host_b = write(
        dir.path(),
        "host-b.yaml",
        &group_host_document_other_build(dir.path(), "host-b", "192.0.2.11").to_string(),
    );
    let (code, value, raw) = validate_group(
        "docs/examples/deployment-multinode.yaml",
        &["docs/examples/host.yaml", host_b.to_str().unwrap()],
    );
    assert_eq!(code, 2, "{raw}");
    assert_eq!(value["code"], "group_profile_mismatch", "{raw}");
    // A named host that does not publish the profile at all is the same refusal.
    let mut bare = group_host_document(dir.path(), "host-b", Some("192.0.2.11"));
    bare["runtime_profiles"]
        .as_object_mut()
        .unwrap()
        .remove("vllm");
    let host_b = write(dir.path(), "host-b.yaml", &bare.to_string());
    let (code, value, raw) = validate_group(
        "docs/examples/deployment-multinode.yaml",
        &["docs/examples/host.yaml", host_b.to_str().unwrap()],
    );
    assert_eq!(code, 2, "{raw}");
    assert_eq!(value["code"], "group_profile_mismatch", "{raw}");
    assert!(
        value["message"].as_str().unwrap().contains("host-b"),
        "{raw}"
    );
}

// T14, T37: an engine environment name approved on host A but not on host B is refused.
#[test]
fn an_env_name_not_approved_on_one_host_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let deployment = write(
        dir.path(),
        "deployment.yaml",
        &group_deployment_with_env("SGLANG_ENABLE_X", "1").to_string(),
    );
    let approving = |name: &str, peer: &str, approved: &[&str]| {
        write(
            dir.path(),
            &format!("{name}.yaml"),
            &group_host_document_approving(dir.path(), name, peer, approved).to_string(),
        )
    };
    let host_a = approving("host-a", "192.0.2.10", &["SGLANG_ENABLE_*"]);
    let host_b = approving("host-b", "192.0.2.11", &[]);
    let (code, value, raw) = validate(&[
        "--file",
        deployment.to_str().unwrap(),
        "--host",
        host_a.to_str().unwrap(),
        "--host",
        host_b.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{raw}");
    assert_eq!(
        value["code"], "engine_env_not_approved:SGLANG_ENABLE_X",
        "{raw}"
    );
    // Approved on both, the group validates.
    let host_b = approving("host-b", "192.0.2.11", &["SGLANG_ENABLE_*"]);
    let (code, _, raw) = validate(&[
        "--file",
        deployment.to_str().unwrap(),
        "--host",
        host_a.to_str().unwrap(),
        "--host",
        host_b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{raw}");
}

// T03: `--host` is repeatable; a named host without a document is refused, naming it.
#[test]
fn every_named_host_needs_its_document() {
    let (code, value, raw) = validate_group(
        "docs/examples/deployment-multinode.yaml",
        &["docs/examples/host.yaml"],
    );
    assert_eq!(code, 2, "{raw}");
    assert!(
        value["message"].as_str().unwrap().contains("host-b"),
        "{raw}"
    );
    let (code, value, raw) = validate_group("docs/examples/deployment-multinode.yaml", &[]);
    assert_eq!(code, 0, "{raw}");
    assert!(value["resolved_against"].is_null(), "{raw}");
}

// T03: a host document for a host the group does not name, or two documents
// for one host, is refused with the host named.
#[test]
fn a_document_for_a_host_not_named_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let other = write(
        dir.path(),
        "host-c.yaml",
        &group_host_document(dir.path(), "host-c", Some("192.0.2.12")).to_string(),
    );
    let (code, value, raw) = validate_group(
        "docs/examples/deployment-multinode.yaml",
        &[
            "docs/examples/host.yaml",
            "docs/examples/host-b.yaml",
            other.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 2, "{raw}");
    assert!(
        value["message"].as_str().unwrap().contains("host-c"),
        "{raw}"
    );
    let (code, value, raw) = validate_group(
        "docs/examples/deployment-multinode.yaml",
        &[
            "docs/examples/host.yaml",
            "docs/examples/host-b.yaml",
            "docs/examples/host-b.yaml",
        ],
    );
    assert_eq!(code, 2, "{raw}");
    assert!(
        value["message"].as_str().unwrap().contains("host-b"),
        "{raw}"
    );
}

// T39: a deployment without a group takes at most one `--host`.
#[test]
fn a_deployment_without_a_group_takes_one_host() {
    let (code, value, raw) = validate_group(
        "docs/examples/deployment-minimal.yaml",
        &["docs/examples/host.yaml", "docs/examples/host-b.yaml"],
    );
    assert_eq!(code, 2, "{raw}");
    assert_eq!(value["code"], "invalid_config", "{raw}");
    assert!(
        value["message"].as_str().unwrap().contains("--host"),
        "{raw}"
    );
}

// T03 (final review I10): `validate config` runs the checks `start
// standalone` runs before any side effect, so a standalone document that
// binds management beyond loopback is refused here (before, it validated and
// only the start refused it). The documented example still validates.
#[test]
fn a_standalone_document_start_refuses_is_refused_by_validate() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let text = format!(
        "schema_version: 1\nkind: standalone\nname: local\nserver:\n  name: local\n  state_dir: {}\n  listeners:\n    management:\n      bind: \"0.0.0.0:7443\"\n      authentication: admin_token\n",
        state.join("server").display()
    );
    let file = write(dir.path(), "standalone.yaml", &text);
    let (code, _, raw) = validate(&["--file", file.to_str().unwrap()]);
    assert_eq!(code, 2, "{raw}");
    assert!(raw.contains("server.listeners.management"), "{raw}");
    let loopback = write(
        dir.path(),
        "loopback.yaml",
        &text.replace("0.0.0.0:7443", "127.0.0.1:7443"),
    );
    let (code, value, raw) = validate(&["--file", loopback.to_str().unwrap()]);
    assert_eq!(code, 0, "{raw}");
    assert_eq!(value["valid"], true);
    // A state directory outside the root the start uses is refused too.
    let (code, _, raw) = validate(&[
        "--file",
        loopback.to_str().unwrap(),
        "--state-dir",
        dir.path().join("elsewhere").to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{raw}");
    assert!(raw.contains("server.state_dir"), "{raw}");
}

// T03 T14 (final review I10, ADR 0014 §7): offline validation does not know
// a checkpoint's weights. A deployment sized from them is reported
// provisional with every weight-derived figure unknown, never a request
// sized from zero bytes or a host_backed tier for a zero-byte copy.
#[test]
fn unknown_weights_are_reported_unknown_not_zero() {
    let host = examples().join("host-discrete.yaml");
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        dir.path(),
        "remote.yaml",
        "name: remote\nengine: vllm\nmodel: {hf: org/model@0123456789abcdef0123456789abcdef01234567}\n",
    );
    let (code, value, raw) = validate(&[
        "--file",
        file.to_str().unwrap(),
        "--host",
        host.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{raw}");
    assert_eq!(value["provisional"], true, "{raw}");
    let effective = &value["effective"];
    let unknown = "unknown until the checkpoint is measured (after deploy)";
    assert_eq!(effective["memory"]["request_bytes"], unknown, "{raw}");
    assert_eq!(effective["memory"]["weights_bytes"], unknown, "{raw}");
    assert_eq!(effective["resources"], unknown, "{raw}");
    assert_eq!(effective["residency"], unknown, "{raw}");
    assert_ne!(effective["residency"], "host_backed");
}

// T03, SPEC §15.3: a resources block the server would refuse fails offline.
#[test]
fn an_incomplete_resources_block_fails_without_a_host() {
    let root = tempfile::tempdir().unwrap();
    let file = write(
        root.path(),
        "d.yaml",
        "name: m\nengine: vllm\nmodel: toy\nresources:\n  cold:\n    allocations: [{domain: unified, bytes: 32GiB}]\n",
    );
    let (code, out, raw) = validate(&["--file", file.to_str().unwrap()]);
    assert_eq!(code, 2, "{raw}");
    assert!(raw.contains("resources.cold"), "{out}");
}

// T03 T41, ADR 0023 §4: a TensorFold deployment without resources fails offline.
#[test]
fn a_tensorfold_deployment_without_resources_fails_without_a_host() {
    let root = tempfile::tempdir().unwrap();
    let file = write(
        root.path(),
        "d.yaml",
        "name: m\nengine: tensorfold\nmodel: toy\nengine_config: {context_length: 32768}\n",
    );
    let (code, _, raw) = validate(&["--file", file.to_str().unwrap()]);
    assert_eq!(code, 2, "{raw}");
    assert!(raw.contains("resources"), "{raw}");
}

// T03, SPEC §15.3: the text output names what was not checked.
#[test]
fn the_text_output_names_what_still_needs_a_host() {
    let root = tempfile::tempdir().unwrap();
    let file = write(root.path(), "d.yaml", "name: m\nengine: vllm\nmodel: toy\n");
    let out = support::capyctl()
        .args(["validate", "config", "--file", file.to_str().unwrap()])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Not checked"), "{text}");
    assert!(text.contains("--host"), "{text}");
}

// T03 T15: the short form of `resources` validates offline and shows the five
// phases it stands for; against a discrete host it is the long form, with the
// card's domain and the system domain named.
#[test]
fn the_short_resources_form_shows_its_phases() {
    let root = tempfile::tempdir().unwrap();
    let text = "name: m\nengine: vllm\nmodel: coding-small-r1\nresidency: restart_only\n\
                devices: [{id: gpu0}]\nresources: {gpu: 11GiB, ram: 2GiB}\n\
                engine_config: {memory: {kv_cache: 2GiB}}\n";
    let short = write(root.path(), "short.yaml", text);
    let (code, value, raw) = validate(&["--file", short.to_str().unwrap()]);
    assert_eq!(code, 0, "{raw}");
    let active = json!({"gpu": "11GiB", "ram": "2GiB"});
    assert_eq!(
        value["resources"],
        json!({"cold": active, "ready": active, "parking": active,
               "parked": {"gpu": "0B", "ram": "0B"}, "wake": active}),
        "{raw}"
    );
    let out = support::capyctl()
        .args(["validate", "config", "--file", short.to_str().unwrap()])
        .output()
        .unwrap();
    let text_out = String::from_utf8(out.stdout).unwrap();
    assert!(text_out.contains("ready"), "{text_out}");
    assert!(text_out.contains("gpu 11GiB, ram 2GiB"), "{text_out}");

    let host = examples().join("host-discrete.yaml");
    let (code, value, raw) = validate(&[
        "--file",
        short.to_str().unwrap(),
        "--host",
        host.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{raw}");
    // Strict configs take no YAML anchors, so the long form spells each phase.
    let phase = |name: &str, gpu: &str, ram: &str, devices: &str| {
        format!(
            "  {name}:\n    allocations: [{{domain: gpu0, bytes: {gpu}, host_kv_bytes: 0B}}, \
             {{domain: system, bytes: {ram}, host_kv_bytes: 0B}}]\n    devices: {devices}\n"
        )
    };
    let mut phases = String::from("resources:\n");
    for name in ["cold", "ready", "parking", "wake"] {
        phases.push_str(&phase(name, "11GiB", "2GiB", "[{id: gpu0}]"));
    }
    phases.push_str(&phase("parked", "0B", "0B", "[]"));
    let long_text = text.replace("resources: {gpu: 11GiB, ram: 2GiB}\n", &phases);
    let long = write(root.path(), "long.yaml", &long_text);
    let (code, long_value, raw) = validate(&[
        "--file",
        long.to_str().unwrap(),
        "--host",
        host.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{raw}");
    assert_eq!(
        value["effective"], long_value["effective"],
        "the short form resolves as the long form"
    );
    assert_eq!(
        value["document"]["resources"]["ready"]["allocations"][0]["domain"],
        "gpu0"
    );
    let out = support::capyctl()
        .args(["validate", "config", "--file", short.to_str().unwrap()])
        .args(["--host", host.to_str().unwrap()])
        .output()
        .unwrap();
    let text_out = String::from_utf8(out.stdout).unwrap();
    assert!(
        text_out.contains("gpu0 11.0 GiB, system 2.0 GiB"),
        "{text_out}"
    );
}

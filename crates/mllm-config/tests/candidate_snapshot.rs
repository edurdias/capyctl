use mllm_config::effective::candidate::{
    validate_candidate_reviewed_snapshot as snapshot,
    validate_candidate_reviewed_snapshot_text as snapshot_text,
};
use mllm_config::effective::resolve_effective;
use mllm_config::{parse_strict, ConfigKind};
use serde_json::{json, Value};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/candidate-fake.json")).unwrap()
}

fn assert_snapshot_rejects(value: &Value, label: &str) {
    assert!(snapshot(value).is_err(), "value: {label}");
    assert!(snapshot_text(&value.to_string()).is_err(), "text: {label}");
}

fn fixture_yaml(value: &Value) -> String {
    value
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| format!("{key}: {}\n", serde_json::to_string(value).unwrap()))
        .collect()
}

#[test]
fn frozen_snapshot_has_no_local_configuration_dependency() {
    let value = fixture();
    let accepted = snapshot(&value).unwrap();
    let read = snapshot_text(std::str::from_utf8(accepted.reviewed_json()).unwrap()).unwrap();
    assert_eq!(read.reviewed_json(), accepted.reviewed_json());
    assert_eq!(read.manifest_digest(), accepted.manifest_digest());
    assert_eq!(read.total_case_request_budget(), 7);
    assert_eq!(read.host_id(), "lab");
}

#[test]
fn intrinsic_bounds_do_not_depend_on_profile_equality() {
    for (path, bad) in [
        ("/effective_recipe/runtime_profile_revision", json!(0)),
        ("/effective_recipe/resolved_profile/revision", json!(0)),
        ("/effective_recipe/resolved_profile/revision", json!(8)),
        ("/effective_recipe/resolved_profile/log_policy/max_file_bytes", json!(-1)),
        ("/effective_recipe/resources/cold/allocations/0/bytes", json!(-1)),
        ("/effective_recipe/resources/cold/allocations/0/host_kv_bytes", json!(-1)),
        ("/effective_recipe/request_deadline_ms", json!(0)),
        ("/effective_recipe/model/path", json!("relative")),
        ("/effective_recipe/resolved_profile/executable", json!("relative")),
        ("/cases/1/request_budget", json!(0)),
        ("/cases/2/count", json!(0)),
        ("/cases/2/corpus_digest", json!(null)),
        ("/cases/5/cycle", json!(2)),
        ("/limits/max_requests", json!(6)),
    ] {
        let mut value = fixture();
        *value.pointer_mut(path).unwrap() = bad;
        assert!(snapshot(&value).is_err(), "{path}");
        assert!(snapshot_text(&value.to_string()).is_err(), "{path}");
    }
}

#[test]
fn text_rejects_discardable_duplicates() {
    let text = serde_json::to_string(&fixture()).unwrap();
    for duplicate in [
        text.replacen("\"schema_version\":1", "\"schema_version\":1,\"schema_version\":1", 1),
        text.replacen("\"runtime_profile_revision\":7", "\"runtime_profile_revision\":7,\"runtime_profile_revision\":7", 1),
        text.replacen("\"count\":1", "\"count\":1,\"count\":1", 1),
    ] {
        assert_ne!(duplicate, text);
        assert!(snapshot_text(&duplicate).is_err());
    }
}

#[test]
fn existing_fixtures_have_stable_value_and_text_snapshots() {
    for text in [
        include_str!("fixtures/candidate-fake.json"),
        include_str!("fixtures/candidate-vllm.json"),
        include_str!("fixtures/candidate-sglang.json"),
        include_str!("fixtures/candidate-f2c.json"),
    ] {
        let value: Value = serde_json::from_str(text).unwrap();
        let from_value = snapshot(&value).unwrap();
        let from_text = snapshot_text(text).unwrap();
        assert_eq!(from_value.reviewed_json(), from_text.reviewed_json());
        assert_eq!(from_value.manifest_digest(), from_text.manifest_digest());
        assert_eq!(from_value.total_case_request_budget(), from_text.total_case_request_budget());
    }
    let vllm: Value = serde_json::from_str(include_str!("fixtures/candidate-vllm-canonical.json")).unwrap();
    assert_eq!(
        snapshot(&vllm).unwrap().reviewed_json(),
        include_bytes!("fixtures/candidate-vllm-canonical.json").strip_suffix(b"\n").unwrap(),
    );
}

#[test]
fn revisions_keep_full_u64_wire_domain() {
    for revision in [9_223_372_036_854_775_808_u64, u64::MAX] {
        let mut value = fixture();
        value["effective_recipe"]["runtime_profile_revision"] = revision.into();
        value["effective_recipe"]["resolved_profile"]["revision"] = revision.into();
        let accepted = snapshot(&value).unwrap();
        let read = snapshot_text(std::str::from_utf8(accepted.reviewed_json()).unwrap()).unwrap();
        assert_eq!(read.effective_recipe().runtime_profile_revision(), revision);
        assert_eq!(read.effective_recipe().profile().revision(), revision);
    }
    let overflow = serde_json::to_string(&fixture()).unwrap().replacen(
        "\"runtime_profile_revision\":7",
        "\"runtime_profile_revision\":18446744073709551616",
        1,
    );
    assert!(snapshot_text(&overflow).is_err());
}

#[test]
fn text_bound_is_checked_before_parsing() {
    let text = serde_json::to_string(&fixture()).unwrap();
    let exact = format!("{text}{}", " ".repeat((1 << 20) - text.len()));
    assert_eq!(exact.len(), 1 << 20);
    assert!(snapshot_text(&exact).is_ok());
    assert!(snapshot_text(&(exact + " ")).is_err());
}

#[test]
fn engine_specific_signed_bytes_and_engine_tags_are_intrinsic() {
    for (fixture_text, paths) in [
        (
            include_str!("fixtures/candidate-vllm.json"),
            &[
                "/effective_recipe/resolved_profile/launch_settings/cpu_offload_bytes",
                "/effective_recipe/resolved_profile/launch_settings/requested_budget/kv_cache_bytes",
                "/effective_recipe/resolved_profile/launch_settings/requested_budget/swap_space_bytes",
            ][..],
        ),
        (
            include_str!("fixtures/candidate-sglang.json"),
            &["/effective_recipe/resolved_profile/launch_settings/requested_budget/kv_cache_bytes"][..],
        ),
    ] {
        for path in paths {
            let mut value: Value = serde_json::from_str(fixture_text).unwrap();
            *value.pointer_mut(path).unwrap() = json!(-1);
            assert!(snapshot(&value).is_err(), "{path}");
            assert!(snapshot_text(&value.to_string()).is_err(), "{path}");
        }
    }

    let mut value = fixture();
    value["effective_recipe"]["resolved_profile"]["engine"] = "vllm".into();
    assert!(snapshot(&value).is_err());
    assert!(snapshot_text(&value.to_string()).is_err());
}

#[test]
fn intrinsic_recipe_and_primitive_failures_reject_without_profile_equality() {
    for (path, bad) in [
        (
            "/effective_recipe/resolved_profile/log_policy/max_file_bytes",
            json!(9_223_372_036_854_775_808_u64),
        ),
        (
            "/effective_recipe/resolved_profile/log_policy/max_file_bytes",
            json!(1.5),
        ),
        ("/effective_recipe/runtime_profile", json!("x".repeat(257))),
        ("/cases/2/corpus_digest", json!("A".repeat(64))),
    ] {
        let mut value = fixture();
        *value.pointer_mut(path).unwrap() = bad;
        assert_snapshot_rejects(&value, path);
    }

    let mut duplicate_device = fixture();
    let claim = duplicate_device["effective_recipe"]["devices"][0].clone();
    duplicate_device["effective_recipe"]["devices"]
        .as_array_mut()
        .unwrap()
        .push(claim);
    assert_snapshot_rejects(&duplicate_device, "duplicate selected device");

    let mut phase_claim = fixture();
    phase_claim["effective_recipe"]["resources"]["ready"]["devices"][0]["sharing"] =
        "exclusive".into();
    assert_snapshot_rejects(&phase_claim, "phase claim mismatch");

    let mut transition = fixture();
    transition["effective_recipe"]["resources"]["parked"]["allocations"][0]["bytes"] =
        10_737_418_241_i64.into();
    assert_snapshot_rejects(&transition, "malformed phase transition");

    let mut unknown = fixture();
    unknown["effective_recipe"]
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), json!(true));
    assert_snapshot_rejects(&unknown, "unknown recipe field");
}

#[test]
fn text_rejects_yaml_duplicates_documents_and_secret_diagnostics() {
    let value = fixture();
    let yaml = fixture_yaml(&value);
    assert!(snapshot_text(&yaml).is_ok());

    let duplicate_root = format!("schema_version: 1\n{yaml}");
    assert!(snapshot_text(&duplicate_root).is_err());

    let duplicate_sequence_key = yaml.replacen(
        "\"count\":1",
        "\"count\":1,\"count\":1",
        1,
    );
    assert_ne!(duplicate_sequence_key, yaml);
    assert!(snapshot_text(&duplicate_sequence_key).is_err());

    let multiple = format!("{yaml}---\n{yaml}");
    assert!(snapshot_text(&multiple).is_err());

    let sentinel = "candidate-secret-sentinel";
    let mut bad = value;
    bad["effective_recipe"]["runtime_profile_revision"] = sentinel.into();
    let value_error = snapshot(&bad).unwrap_err().to_string();
    let text_error = snapshot_text(&bad.to_string()).unwrap_err().to_string();
    assert!(!value_error.contains(sentinel));
    assert!(!text_error.contains(sentinel));
}

#[test]
fn signed_qualification_revision_rejects_preserved_large_unsigned_integer() {
    let golden: Value =
        serde_json::from_str(include_str!("fixtures/effective-vllm-golden.json")).unwrap();
    let deployment = golden["input"]["deployment"].clone();
    let mut host = golden["input"]["host"].clone();
    host["qualification_policy"] = json!({
        "revision": 1,
        "allow_qualification_runs": false,
        "allow_experimental_controls": true,
        "allowed_manifest_digests": [],
        "max_run_duration": "24h",
        "max_cleanup_duration": "60m",
        "max_cases": 128,
        "max_requests": 4096,
        "max_request_body_bytes": "1MiB",
        "max_input_tokens_per_request": 131072,
        "max_output_tokens_per_request": 16384
    });
    host["qualification_policy"]["revision"] = 9_223_372_036_854_775_808_u64.into();
    let text = serde_json::to_string(&host).unwrap();
    assert!(text.contains("9223372036854775808"));
    let parsed = parse_strict(ConfigKind::Host, &text).unwrap();
    assert_eq!(
        parsed["qualification_policy"]["revision"].as_u64(),
        Some(9_223_372_036_854_775_808)
    );
    assert!(resolve_effective(&deployment, &parsed).is_err());
}

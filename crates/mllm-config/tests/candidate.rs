use mllm_config::effective::{
    candidate::{normalize_candidate_manifest, normalize_candidate_manifest_text},
    resolve_effective,
};

fn fixture() -> (serde_json::Value, serde_json::Value) {
    let all: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let deployment = &all["deployment"];
    let host = all["host"].clone();
    let cases = serde_json::json!([
        {"id":"cold","kind":"cold_initialize","cycle":0,"count":1,"request_budget":0},
        {"id":"ready","kind":"ready_probe","cycle":0,"count":1,"request_budget":1},
        {"id":"marker-ns","kind":"marker_nonstreaming","cycle":0,"count":1,"request_budget":1,"corpus_digest":"0000000000000000000000000000000000000000000000000000000000000000"},
        {"id":"marker-s","kind":"marker_streaming","cycle":0,"count":1,"request_budget":1,"corpus_digest":"0000000000000000000000000000000000000000000000000000000000000000"},
        {"id":"security","kind":"security","cycle":0,"count":1,"request_budget":1},
        {"id":"park","kind":"park","cycle":1,"count":1,"request_budget":0},
        {"id":"restore","kind":"restore","cycle":1,"count":1,"request_budget":0},
        {"id":"ready-1","kind":"ready_probe","cycle":1,"count":1,"request_budget":1},
        {"id":"marker-ns-1","kind":"marker_nonstreaming","cycle":1,"count":1,"request_budget":1,"corpus_digest":"0000000000000000000000000000000000000000000000000000000000000000"},
        {"id":"marker-s-1","kind":"marker_streaming","cycle":1,"count":1,"request_budget":1,"corpus_digest":"0000000000000000000000000000000000000000000000000000000000000000"}
    ]);
    let devices = deployment["devices"].clone();
    let phase = |bytes: i64, host_kv_bytes: i64, active: bool| {
        serde_json::json!({
            "allocations":[{"domain":"unified","bytes":bytes,"host_kv_bytes":host_kv_bytes}],
            "devices": if active { devices.clone() } else { serde_json::json!([]) }
        })
    };
    let candidate = serde_json::json!({
        "schema_version":1,"kind":"candidate_recipe",
        "host":{"id":"lab","hardware_fingerprint":"hw-01","environment_fingerprint":"env-01"},
        "effective_recipe":{
            "model":deployment["model"],"recipe":"standard","residency":"warm","recovery":"reconcile",
            "runtime_profile":"local","runtime_profile_revision":7,
            "resolved_profile":{"engine":"vllm","revision":7,"executable":"/bin/true","build_fingerprint":"vllm-build-1",
                "args":["--max-model-len","4096"],"launch_settings":{"engine":"vllm","tensor_parallel_size":1,
                "pipeline_parallel_size":1,"enable_sleep_mode":true,"kv_cache_dtype":"auto","block_size_tokens":16,
                "cpu_offload_bytes":0,"requested_budget":{"kv_cache_bytes":4294967296_i64,"swap_space_bytes":0,"gpu_utilization_pct":75}},
                "env":{"RUST_LOG":"info"},"experimental_controls":true,"runtime_auth":true,"admin_auth":false,
                "log_policy":{"max_file_bytes":16777216,"retained_files":3}},
            "devices":devices,"resources":{"cold":phase(10737418240,1073741824,true),"ready":phase(8589934592,1073741824,true),
                "parking":phase(9663676416,1073741824,true),"parked":phase(2147483648,0,false),"wake":phase(10737418240,1073741824,true)},
            "host_devices":{"gpu0":{"domain":"unified","sharing":"shared"}},"host_device_sharing":"shared","request_deadline_ms":300000},
        "limits":{"max_run_duration_ms":86400000,"max_cleanup_duration_ms":3600000,"max_requests":16,
            "max_request_body_bytes":1048576,"max_input_tokens_per_request":131072,"max_output_tokens_per_request":16384},
        "evaluator_suite":"recipe_v1","cases":cases
    });
    (candidate, host)
}

fn ordinary_fixture() -> (serde_json::Value, serde_json::Value) {
    let all: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    (all["deployment"].clone(), all["host"].clone())
}

#[test]
fn valid_candidate_normalizes_without_authority_material() {
    let (candidate, host) = fixture();
    let normalized = normalize_candidate_manifest(&candidate, &host).unwrap();
    assert_eq!(normalized.host_id(), "lab");
    assert_eq!(normalized.total_case_request_budget(), 7);
    assert_eq!(normalized.manifest_digest().len(), 64);
    assert_eq!(
        normalized.manifest_digest(),
        "f7fafaa108d062fb62417ccef59a1088a158a51890c8af1915e3b331dca1b0e9"
    );
    let (deployment, ordinary_host) = ordinary_fixture();
    assert_eq!(
        normalized.recipe_fingerprint(),
        "8fca6812174ea5c21d212fce2b2923a52e6fcdd7ea38aef251a0a7ebf751a1ef"
    );
    assert_eq!(
        normalized.recipe_fingerprint(),
        resolve_effective(&deployment, &ordinary_host)
            .unwrap()
            .qualification_fingerprint
    );
    assert!(!normalized
        .reviewed_json()
        .windows(9)
        .any(|w| w == b"secret://"));
}

#[test]
fn text_entry_rejects_duplicates_and_multiple_documents() {
    let (candidate, host) = fixture();
    let json = serde_json::to_string(&candidate).unwrap();
    assert_eq!(
        normalize_candidate_manifest_text(&json, &host)
            .unwrap()
            .manifest_digest(),
        "f7fafaa108d062fb62417ccef59a1088a158a51890c8af1915e3b331dca1b0e9"
    );
    let duplicate = json.replacen("{\"cases\"", "{\"kind\":\"candidate_recipe\",\"cases\"", 1);
    assert!(normalize_candidate_manifest_text(&duplicate, &host).is_err());
    assert!(normalize_candidate_manifest_text(&(json.clone() + "\n---\n{}"), &host).is_err());
}

#[test]
fn authority_values_and_policy_do_not_affect_candidate_identities() {
    let (candidate, host) = fixture();
    let original = normalize_candidate_manifest(&candidate, &host).unwrap();
    let mut changed = host;
    changed["runtime_profiles"]["local"]["qualification_id"] = "candidate-looking".into();
    changed["runtime_profiles"]["local"]["security"]["credential_ref"] = "secret://rotated".into();
    changed["qualification_policy"] = serde_json::json!({"revision":1,"allow_qualification_runs":false,
        "allow_experimental_controls":false,"allowed_manifest_digests":[],"max_run_duration":"1h",
        "max_cleanup_duration":"1m","max_cases":1,"max_requests":1,"max_request_body_bytes":"1B",
        "max_input_tokens_per_request":1,"max_output_tokens_per_request":1});
    let changed = normalize_candidate_manifest(&candidate, &changed).unwrap();
    assert_eq!(original.manifest_digest(), changed.manifest_digest());
    assert_eq!(original.recipe_fingerprint(), changed.recipe_fingerprint());
    assert_eq!(
        changed.credential_refs().runtime(),
        Some("secret://rotated")
    );
}

#[test]
fn empty_credential_references_are_rejected_without_echoing_values() {
    for field in ["credential_ref", "admin_credential_ref"] {
        let (candidate, mut host) = fixture();
        host["runtime_profiles"]["local"]["security"][field] = "".into();
        assert!(
            normalize_candidate_manifest(&candidate, &host).is_err(),
            "{field}"
        );
    }
}

#[test]
fn reviewed_only_changes_do_not_change_recipe_fingerprint() {
    let (candidate, host) = fixture();
    let original = normalize_candidate_manifest(&candidate, &host).unwrap();
    let mut changed = candidate;
    changed["cases"][0]["request_budget"] = 1.into();
    let changed = normalize_candidate_manifest(&changed, &host).unwrap();
    assert_ne!(original.manifest_digest(), changed.manifest_digest());
    assert_eq!(original.recipe_fingerprint(), changed.recipe_fingerprint());
}

#[test]
fn candidate_allows_missing_qualification_reference_but_rejects_present_invalid_reference() {
    let (candidate, mut host) = fixture();
    host["runtime_profiles"]["local"]
        .as_object_mut()
        .unwrap()
        .remove("qualification_id");
    assert!(normalize_candidate_manifest(&candidate, &host).is_ok());
    for bad in [
        serde_json::Value::Null,
        serde_json::json!(""),
        serde_json::json!(17),
    ] {
        let (candidate, mut host) = fixture();
        host["runtime_profiles"]["local"]["qualification_id"] = bad;
        assert!(normalize_candidate_manifest(&candidate, &host).is_err());
    }
}

#[test]
fn limits_and_order_are_enforced() {
    let (mut candidate, host) = fixture();
    candidate["limits"]["max_requests"] = 6.into();
    assert!(normalize_candidate_manifest(&candidate, &host).is_err());
    candidate["limits"]["max_requests"] = 16.into();
    candidate["cases"].as_array_mut().unwrap().swap(0, 1);
    assert!(normalize_candidate_manifest(&candidate, &host).is_err());
}

#[test]
fn host_and_profile_drift_are_rejected() {
    let (mut candidate, host) = fixture();
    candidate["host"]["hardware_fingerprint"] = "wrong".into();
    assert!(normalize_candidate_manifest(&candidate, &host).is_err());
    let (mut candidate, host) = fixture();
    candidate["effective_recipe"]["resolved_profile"]["build_fingerprint"] = "wrong".into();
    assert!(normalize_candidate_manifest(&candidate, &host).is_err());
}

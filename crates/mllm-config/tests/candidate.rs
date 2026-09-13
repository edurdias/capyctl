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

#[test]
fn candidate_rejects_invalid_trusted_host_and_shared_profile_settings() {
    for (path, value) in [
        (
            "/resource_policy/queue/max_pending_total",
            serde_json::json!(0),
        ),
        (
            "/resource_policy/endpoint_port_range/start",
            serde_json::json!(0),
        ),
        (
            "/runtime_profiles/local/security/experimental_controls",
            serde_json::json!(false),
        ),
        (
            "/runtime_profiles/local/build_fingerprint",
            serde_json::json!(""),
        ),
    ] {
        let (mut candidate, mut host) = fixture();
        *host.pointer_mut(path).expect(path) = value.clone();
        if path.ends_with("experimental_controls") {
            candidate["effective_recipe"]["resolved_profile"]["experimental_controls"] = value;
        } else if path.ends_with("build_fingerprint") {
            candidate["effective_recipe"]["resolved_profile"]["build_fingerprint"] = value;
        }
        assert!(
            normalize_candidate_manifest(&candidate, &host).is_err(),
            "{path}"
        );
    }
}

fn engine_fixture(engine: &str) -> (serde_json::Value, serde_json::Value, serde_json::Value) {
    let (candidate, ordinary) = match engine {
        "vllm" => (
            include_str!("fixtures/candidate-vllm.json"),
            include_str!("fixtures/effective-vllm-golden.json"),
        ),
        "sglang" => (
            include_str!("fixtures/candidate-sglang.json"),
            include_str!("fixtures/effective-sglang-golden.json"),
        ),
        "fake" => (
            include_str!("fixtures/candidate-fake.json"),
            include_str!("fixtures/effective-fake-golden.json"),
        ),
        _ => unreachable!(),
    };
    let ordinary: serde_json::Value = serde_json::from_str(ordinary).unwrap();
    (
        serde_json::from_str(candidate).unwrap(),
        ordinary["input"]["deployment"].clone(),
        ordinary["input"]["host"].clone(),
    )
}

#[test]
fn all_engines_share_literal_recipe_fingerprints() {
    for (engine, fingerprint) in [
        (
            "vllm",
            "8fca6812174ea5c21d212fce2b2923a52e6fcdd7ea38aef251a0a7ebf751a1ef",
        ),
        (
            "sglang",
            "b7863d64c7a21c4ed88c046154afdaa02f886e0ad723498ad58f267564913484",
        ),
        (
            "fake",
            "faa5d521b8427650c9edaa0746590c69e79a9acf45af6943fc3a0e75eafebc9b",
        ),
    ] {
        let (candidate, deployment, host) = engine_fixture(engine);
        let normalized = normalize_candidate_manifest(&candidate, &host).unwrap();
        let ordinary = resolve_effective(&deployment, &host).unwrap();
        assert_eq!(normalized.recipe_fingerprint(), fingerprint, "{engine}");
        assert_eq!(
            normalized.recipe_fingerprint(),
            ordinary.qualification_fingerprint,
            "{engine}"
        );
    }
}

#[test]
fn qualification_reference_presence_is_wrapper_specific_on_all_profiles() {
    for engine in ["vllm", "sglang", "fake"] {
        for selected in [true, false] {
            for (value, candidate_ok, ordinary_ok) in [
                (None, true, false),
                (Some(serde_json::json!("")), false, !selected),
                (Some(serde_json::Value::Null), false, false),
                (Some(serde_json::json!(17)), false, false),
                (Some(serde_json::json!("candidate-looking")), true, true),
            ] {
                let (candidate, deployment, mut host) = engine_fixture(engine);
                let name = if selected { "local" } else { "unselected" };
                if !selected {
                    host["runtime_profiles"][name] = host["runtime_profiles"]["local"].clone();
                }
                let profile = host["runtime_profiles"][name].as_object_mut().unwrap();
                if let Some(value) = &value {
                    profile.insert("qualification_id".into(), value.clone());
                } else {
                    profile.remove("qualification_id");
                }
                assert_eq!(
                    normalize_candidate_manifest(&candidate, &host).is_ok(),
                    candidate_ok,
                    "candidate {engine} {name} {value:?}"
                );
                assert_eq!(
                    resolve_effective(&deployment, &host).is_ok(),
                    ordinary_ok,
                    "ordinary {engine} {name} {value:?}"
                );
            }
        }
    }
}

#[test]
fn both_paths_reject_shared_recipe_and_host_failures() {
    for engine in ["vllm", "sglang", "fake"] {
        for (path, value) in [
            ("/model/path", serde_json::json!("relative")),
            ("/devices/0/id", serde_json::json!("unknown")),
            (
                "/resources/ready/devices/0/sharing",
                serde_json::json!("exclusive"),
            ),
            (
                "/resources/ready/allocations/0/domain",
                serde_json::json!("unknown"),
            ),
        ] {
            let (mut candidate, mut deployment, host) = engine_fixture(engine);
            *deployment.pointer_mut(path).unwrap() = value.clone();
            *candidate["effective_recipe"].pointer_mut(path).unwrap() = value;
            assert!(
                resolve_effective(&deployment, &host).is_err(),
                "ordinary {engine} {path}"
            );
            assert!(
                normalize_candidate_manifest(&candidate, &host).is_err(),
                "candidate {engine} {path}"
            );
        }
        for (path, value) in [
            (
                "/resource_policy/queue/max_pending_total",
                serde_json::json!(0),
            ),
            (
                "/resource_policy/endpoint_port_range/start",
                serde_json::json!(0),
            ),
            (
                "/runtime_profiles/local/executable",
                serde_json::json!("relative"),
            ),
            (
                "/runtime_profiles/local/log_policy/max_file_bytes",
                serde_json::json!("bad-unit"),
            ),
        ] {
            let (candidate, deployment, mut host) = engine_fixture(engine);
            *host.pointer_mut(path).unwrap() = value;
            assert!(
                resolve_effective(&deployment, &host).is_err(),
                "ordinary {engine} {path}"
            );
            assert!(
                normalize_candidate_manifest(&candidate, &host).is_err(),
                "candidate {engine} {path}"
            );
        }
    }
}

#[test]
fn optional_empty_credentials_preserve_ordinary_compatibility() {
    for (engine, field) in [
        ("vllm", "admin_credential_ref"),
        ("fake", "credential_ref"),
        ("fake", "admin_credential_ref"),
    ] {
        let (mut candidate, deployment, mut host) = engine_fixture(engine);
        host["runtime_profiles"]["local"]["security"][field] = "".into();
        let auth = if field == "credential_ref" {
            "runtime_auth"
        } else {
            "admin_auth"
        };
        candidate["effective_recipe"]["resolved_profile"][auth] = true.into();
        assert!(
            resolve_effective(&deployment, &host).is_ok(),
            "{engine} {field}"
        );
        assert!(
            normalize_candidate_manifest(&candidate, &host).is_err(),
            "{engine} {field}"
        );
    }
}

#[test]
fn candidate_and_ordinary_share_profile_validation_before_projection_comparison() {
    for engine in ["vllm", "sglang", "fake"] {
        let (mut candidate, deployment, mut host) = engine_fixture(engine);
        host["runtime_profiles"]["local"]["build_fingerprint"] = "".into();
        candidate["effective_recipe"]["resolved_profile"]["build_fingerprint"] = "".into();
        assert!(resolve_effective(&deployment, &host).is_err());
        assert!(normalize_candidate_manifest(&candidate, &host).is_err());
    }
    for engine in ["vllm", "sglang"] {
        let (mut candidate, deployment, mut host) = engine_fixture(engine);
        host["runtime_profiles"]["local"]["security"]["experimental_controls"] = false.into();
        candidate["effective_recipe"]["resolved_profile"]["experimental_controls"] = false.into();
        assert!(resolve_effective(&deployment, &host).is_err());
        assert!(normalize_candidate_manifest(&candidate, &host).is_err());
    }
}

#[test]
fn ordinary_routes_are_required_and_candidate_routes_are_forbidden() {
    let (mut candidate, mut deployment, host) = engine_fixture("vllm");
    deployment.as_object_mut().unwrap().remove("routes");
    assert!(resolve_effective(&deployment, &host).is_err());
    assert!(normalize_candidate_manifest(&candidate, &host).is_ok());
    candidate["effective_recipe"]["routes"] = serde_json::json!(["toy"]);
    assert!(normalize_candidate_manifest(&candidate, &host).is_err());
}

#[test]
fn non_marker_digest_field_is_forbidden_even_when_null() {
    for index in [0, 1, 4, 5, 6, 7] {
        let (mut candidate, host) = fixture();
        candidate["cases"][index]["corpus_digest"] = serde_json::Value::Null;
        let text = serde_json::to_string(&candidate).unwrap();
        assert!(normalize_candidate_manifest_text(&text, &host).is_err());
        assert!(
            normalize_candidate_manifest(&candidate, &host).is_err(),
            "case {index}"
        );
    }
}

#[test]
fn equal_local_profile_text_still_obeys_candidate_bounds() {
    for field in ["build_fingerprint", "executable"] {
        let (mut candidate, deployment, mut host) = engine_fixture("fake");
        let oversized = if field == "executable" {
            format!("/{}", "x".repeat(4096))
        } else {
            "x".repeat(4097)
        };
        host["runtime_profiles"]["local"][field] = oversized.clone().into();
        candidate["effective_recipe"]["resolved_profile"][field] = oversized.into();
        assert!(resolve_effective(&deployment, &host).is_ok());
        assert!(
            normalize_candidate_manifest(&candidate, &host).is_err(),
            "{field}"
        );
    }
    let (mut candidate, deployment, mut host) = engine_fixture("fake");
    host["runtime_profiles"]["local"]["env"]["RUST_LOG"] = "x".repeat(4097).into();
    candidate["effective_recipe"]["resolved_profile"]["env"]["RUST_LOG"] = "x".repeat(4097).into();
    assert!(resolve_effective(&deployment, &host).is_ok());
    assert!(normalize_candidate_manifest(&candidate, &host).is_err());
}

#[test]
fn selectors_accept_exact_utf8_local_keys_while_case_ids_stay_ascii() {
    for selector in [
        "local profile",
        "profiles/local",
        "配置",
        "x",
        &"é".repeat(128),
    ] {
        let (mut candidate, mut host) = fixture();
        host["name"] = selector.into();
        candidate["host"]["id"] = selector.into();
        let profile = host["runtime_profiles"]
            .as_object_mut()
            .unwrap()
            .remove("local")
            .unwrap();
        host["runtime_profiles"][selector] = profile;
        candidate["effective_recipe"]["runtime_profile"] = selector.into();
        assert!(
            normalize_candidate_manifest(&candidate, &host).is_ok(),
            "selector with {} bytes",
            selector.len()
        );
    }
    let (mut candidate, host) = fixture();
    candidate["cases"][0]["id"] = "case/id".into();
    assert!(normalize_candidate_manifest(&candidate, &host).is_err());
}

#[test]
fn normalized_recipe_is_consumable_through_immutable_typed_views() {
    use mllm_config::effective::{
        candidate::{CandidateCaseKind, CandidateLaunch},
        Engine, Recovery, Residency, Sharing,
    };
    let (candidate, host) = fixture();
    let normalized = normalize_candidate_manifest(&candidate, &host).unwrap();
    let recipe = normalized.effective_recipe();
    assert_eq!(recipe.recipe(), "standard");
    assert_eq!(recipe.residency(), Residency::Warm);
    assert_eq!(recipe.recovery(), Recovery::Reconcile);
    assert_eq!(recipe.host_devices()["gpu0"].domain, "unified");
    assert_eq!(recipe.host_device_sharing(), Sharing::Shared);
    let profile = recipe.profile();
    assert_eq!(profile.engine(), Engine::Vllm);
    assert_eq!(profile.revision(), 7);
    assert_eq!(profile.executable(), "/bin/true");
    assert_eq!(profile.build_fingerprint(), "vllm-build-1");
    assert_eq!(profile.args(), ["--max-model-len", "4096"]);
    assert_eq!(profile.env()["RUST_LOG"], "info");
    assert!(profile.experimental_controls());
    assert!(profile.runtime_auth());
    assert!(!profile.admin_auth());
    assert_eq!(profile.log_policy().max_file_bytes(), 16777216);
    assert_eq!(profile.log_policy().retained_files(), 3);
    let CandidateLaunch::Vllm {
        requested_budget, ..
    } = profile.launch_settings()
    else {
        panic!("vllm")
    };
    assert_eq!(requested_budget.kv_cache_bytes(), 4294967296);
    assert_eq!(requested_budget.swap_space_bytes(), 0);
    assert_eq!(requested_budget.gpu_utilization_pct(), 75);
    let resources = recipe.resources();
    assert_eq!(resources.cold().allocations()[0].domain(), "unified");
    assert_eq!(resources.cold().allocations()[0].bytes(), 10737418240);
    assert_eq!(
        resources.ready().allocations()[0].host_kv_bytes(),
        1073741824
    );
    assert_eq!(resources.parking().devices()[0].id, "gpu0");
    assert!(resources.parked().devices().is_empty());
    assert_eq!(resources.wake().allocations()[0].bytes(), 10737418240);
    assert_eq!(
        normalized.cases()[0].kind(),
        CandidateCaseKind::ColdInitialize
    );
    let (candidate, _, host) = engine_fixture("sglang");
    let normalized = normalize_candidate_manifest(&candidate, &host).unwrap();
    let CandidateLaunch::Sglang {
        requested_budget, ..
    } = normalized.effective_recipe().profile().launch_settings()
    else {
        panic!("sglang")
    };
    assert_eq!(requested_budget.kv_cache_bytes(), 4294967296);
    assert_eq!(requested_budget.static_memory_fraction_bps(), 7500);
}

#[test]
fn supplied_cycle_values_cannot_expand_work_beyond_case_count() {
    // Added only after the bounded-index implementation; never run OOM payloads
    // against the previous max_cycle-driven allocation.
    for cycle in [25, 128, 1000, u32::MAX - 1, u32::MAX] {
        let (mut candidate, host) = fixture();
        candidate["cases"][9]["cycle"] = cycle.into();
        assert!(
            normalize_candidate_manifest(&candidate, &host).is_err(),
            "{cycle}"
        );
    }
    let (mut candidate, host) = fixture();
    let repeated = candidate["cases"].as_array().unwrap()[5..10].to_vec();
    for cycle in 2..=24 {
        for mut case in repeated.clone() {
            case["id"] = format!("{}-{cycle}", case["id"].as_str().unwrap()).into();
            case["cycle"] = cycle.into();
            candidate["cases"].as_array_mut().unwrap().push(case);
        }
    }
    candidate["limits"]["max_requests"] = 128.into();
    let normalized = normalize_candidate_manifest(&candidate, &host).unwrap();
    assert_eq!(normalized.cases().len(), 125);
    assert_eq!(normalized.cases().last().unwrap().cycle(), 24);
}

fn rename_exact_key_and_value(value: &mut serde_json::Value, old: &str, new: &str) {
    match value {
        serde_json::Value::String(s) if s == old => *s = new.into(),
        serde_json::Value::Array(values) => {
            for value in values {
                rename_exact_key_and_value(value, old, new);
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(value) = map.remove(old) {
                map.insert(new.into(), value);
            }
            for value in map.values_mut() {
                rename_exact_key_and_value(value, old, new);
            }
        }
        _ => {}
    }
}

#[test]
fn topology_selectors_are_bounded_utf8_exact_keys() {
    for old in ["gpu0", "unified"] {
        for (selector, valid) in [
            ("显卡 / local".to_string(), true),
            ("é".repeat(128), true),
            ("x".repeat(257), false),
        ] {
            let (mut candidate, mut deployment, mut host) = engine_fixture("vllm");
            for value in [&mut candidate, &mut deployment, &mut host] {
                rename_exact_key_and_value(value, old, &selector);
            }
            assert!(resolve_effective(&deployment, &host).is_ok());
            assert_eq!(
                normalize_candidate_manifest(&candidate, &host).is_ok(),
                valid,
                "{old} {} bytes",
                selector.len()
            );
        }
    }
}

#[test]
fn profile_text_byte_boundaries_cover_args_and_environment() {
    for (text, valid) in [
        ("é".repeat(2048), true),
        ("x".repeat(4097), false),
        (String::new(), false),
    ] {
        for field in ["build_fingerprint", "args", "env"] {
            let (mut candidate, mut host) = fixture();
            let local = &mut host["runtime_profiles"]["local"];
            let asserted = &mut candidate["effective_recipe"]["resolved_profile"];
            match field {
                "args" => {
                    local["args"][1] = text.clone().into();
                    asserted["args"][1] = text.clone().into();
                }
                "env" => {
                    local["env"]["RUST_LOG"] = text.clone().into();
                    asserted["env"]["RUST_LOG"] = text.clone().into();
                }
                _ => {
                    local[field] = text.clone().into();
                    asserted[field] = text.clone().into();
                }
            }
            assert_eq!(
                normalize_candidate_manifest(&candidate, &host).is_ok(),
                valid,
                "{field} {} bytes",
                text.len()
            );
        }
    }
}

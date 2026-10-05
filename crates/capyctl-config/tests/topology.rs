use capyctl_config::group_support::{check_engine_shape, group_support};
use capyctl_config::instances::parse_instance_spec;
use serde_json::json;

fn doc(extra: serde_json::Value) -> serde_json::Value {
    let mut d = json!({"schema_version": 1, "kind": "deployment", "name": "g"});
    d.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    d
}

// T03: a two-host TP2 group parses; the first host is the head.
#[test]
fn two_host_tp2_parses_with_head_first() {
    let spec = parse_instance_spec(&doc(json!({
        "topology": {"tensor_parallel": 2, "pipeline_parallel": 1},
        "placement": {"hosts": ["host-b", "host-a"]}
    })))
    .unwrap();
    let group = spec.group.unwrap();
    assert_eq!(group.head(), "host-b");
    assert_eq!(group.topology.world_size(), 2);
    assert_eq!(group.local_ranks, 1);
}

// T03: TP2 x PP2 over four hosts is one rank per host.
#[test]
fn tp2_pp2_over_four_hosts_parses() {
    let spec = parse_instance_spec(&doc(json!({
        "topology": {"tensor_parallel": 2, "pipeline_parallel": 2},
        "placement": {"hosts": ["a", "b", "c", "d"]}
    })))
    .unwrap();
    assert_eq!(spec.group.unwrap().topology.world_size(), 4);
}

// T03, T14: every deploy-time refusal carries its closed code.
#[test]
fn group_refusals_carry_codes() {
    let cases = [
        (
            json!({"topology": {"tensor_parallel": 2}}),
            "group_placement_required",
        ),
        (
            json!({"topology": {"tensor_parallel": 2}, "host": "a"}),
            "group_placement_required",
        ),
        (
            json!({"topology": {"tensor_parallel": 2},
                "placement": {"hosts": ["a", "b"], "strategy": "spread"}}),
            "group_placement_required",
        ),
        (
            json!({"topology": {"tensor_parallel": 3},
                "placement": {"hosts": ["a", "b"]}}),
            "group_topology_invalid",
        ),
        (
            json!({"topology": {"tensor_parallel": 4},
                "placement": {"hosts": ["a", "b"]}}),
            "group_shape_unsupported",
        ),
        (
            json!({"topology": {"tensor_parallel": 2}, "instances": 2,
                "placement": {"hosts": ["a", "b"]}}),
            "group_instances_unsupported",
        ),
        (
            json!({"topology": {"tensor_parallel": 0},
                "placement": {"hosts": ["a", "b"]}}),
            "group_topology_invalid",
        ),
        (
            json!({"topology": {"tensor_parallel": 2},
                "placement": {"hosts": ["host-a", " host-a"]}}),
            "placement.hosts",
        ),
        // ADR 0028 §2: a plain repeat is a topology error too.
        (
            json!({"topology": {"tensor_parallel": 2},
                "placement": {"hosts": ["a", "a"]}}),
            "group_topology_invalid",
        ),
    ];
    for (extra, expected) in cases {
        let err = parse_instance_spec(&doc(extra.clone())).unwrap_err();
        assert!(err.to_string().contains(expected), "{extra} -> {expected}");
    }
    let err = parse_instance_spec(&doc(json!({"topology": {"tensor_parallel": 2},
        "placement": {"hosts": ["a", " b"]}})))
    .unwrap_err();
    assert!(err.to_string().contains("no surrounding space"));
}

// T39: world size 1 is a single-host deployment with an unchanged identity.
#[test]
fn world_size_one_is_single_host_and_identity_is_unchanged() {
    let plain = parse_instance_spec(&doc(json!({}))).unwrap();
    let tp1 = parse_instance_spec(&doc(json!({
        "topology": {"tensor_parallel": 1, "pipeline_parallel": 1}
    })))
    .unwrap();
    assert!(tp1.group.is_none());
    assert_eq!(plain.command_identity(), tp1.command_identity());
}

// T39 (R8): a world-size-1 topology keeps every single-host rule: `host`,
// selector, strategy and several instances behave as without `topology`.
#[test]
fn world_size_one_keeps_single_host_rules() {
    let tp1 = json!({"topology": {"tensor_parallel": 1, "pipeline_parallel": 1}});
    let with = |extra: serde_json::Value| {
        let mut d = doc(extra);
        d.as_object_mut()
            .unwrap()
            .extend(tp1.as_object().unwrap().clone());
        d
    };
    let spec = parse_instance_spec(&with(json!({"host": "a"}))).unwrap();
    assert_eq!(spec.placement.pinned_host(), Some("a"));
    let extra = json!({"instances": 2, "placement": {"strategy": "pack",
        "selector": {"zone": "x"}, "max_per_host": 2}});
    assert_eq!(
        parse_instance_spec(&with(extra.clone())).unwrap(),
        parse_instance_spec(&doc(extra)).unwrap()
    );
}

// T03, T22: engine shape support: TensorFold is TP 2 on 2 hosts and restart-only;
// vLLM and SGLang take PP and deep park.
#[test]
fn engine_shape_support() {
    let shape = |tp, pp, hosts: &[&str]| {
        parse_instance_spec(&doc(json!({
            "topology": {"tensor_parallel": tp, "pipeline_parallel": pp},
            "placement": {"hosts": hosts}
        })))
        .unwrap()
        .group
        .unwrap()
    };
    let two = shape(2, 1, &["a", "b"]);
    let pp4 = shape(2, 2, &["a", "b", "c", "d"]);
    for engine in ["vllm", "sglang"] {
        assert!(check_engine_shape(engine, &two, "deep").is_ok());
        assert!(check_engine_shape(engine, &pp4, "deep").is_ok());
    }
    assert!(check_engine_shape("tensorfold", &two, "restart_only").is_ok());
    let err = check_engine_shape("tensorfold", &pp4, "restart_only").unwrap_err();
    assert!(err
        .to_string()
        .contains("group_shape_unsupported:tensorfold"));
    let err = check_engine_shape("tensorfold", &two, "deep").unwrap_err();
    assert!(err.to_string().contains("capability_missing:deep_park"));
    assert!(group_support("sglang").unwrap().worker_listens);
    assert!(!group_support("vllm").unwrap().worker_listens);
}

fn group_fixture(engine: &str) -> (serde_json::Value, serde_json::Value) {
    let all: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    if engine == "tensorfold" {
        let profile = &mut host["runtime_profiles"]["local"];
        profile["engine"] = "tensorfold".into();
        profile["executable"] = "/opt/tf/bin/tensorfold".into();
        profile["build_fingerprint"] = "0.6.0".into();
        profile["args"] = json!([]);
        deployment["engine_config"] = json!({"context_length": 8192});
    }
    deployment.as_object_mut().unwrap().remove("host");
    deployment.as_object_mut().unwrap().remove("residency");
    deployment["topology"] = json!({"tensor_parallel": 2});
    deployment["placement"] = json!({"hosts": ["host-a", "host-b"]});
    (deployment, host)
}

// T03, T22: a two-host TensorFold group defaults to restart_only; an explicit
// deep residency is refused capability_missing:deep_park.
#[test]
fn tensorfold_group_resolves_restart_only_and_refuses_deep() {
    use capyctl_config::effective::{resolve_effective, Residency};
    let (mut deployment, mut host) = group_fixture("tensorfold");
    host["runtime_profiles"]["local"]["security"]["deep_park"] = "disabled".into();
    let resolved = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(resolved.residency, Residency::RestartOnly);
    deployment["residency"] = "deep".into();
    let err = resolve_effective(&deployment, &host).unwrap_err();
    assert!(
        err.to_string().contains("capability_missing:deep_park"),
        "{err}"
    );
}

// T03, T22: a vLLM TP2 group resolves with its default residency.
#[test]
fn vllm_group_resolves_with_default_residency() {
    use capyctl_config::effective::resolve_effective;
    let (deployment, host) = group_fixture("vllm");
    resolve_effective(&deployment, &host).unwrap();
}

// T03: a zero dimension with a repeated host is a topology error.
#[test]
fn zero_dimension_with_repeated_host_is_topology_invalid() {
    let err = parse_instance_spec(&doc(json!({
        "topology": {"tensor_parallel": 0},
        "placement": {"hosts": ["a", "a"]}
    })))
    .unwrap_err();
    assert!(err.to_string().contains("group_topology_invalid"), "{err}");
}

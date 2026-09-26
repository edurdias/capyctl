//! ADR 0013 §1–3: instance count and placement constraints in the deployment
//! document. Validation only; placement itself is the scheduler's (I2).
use mllm_config::effective::{deployment_command_fingerprint, resolve_effective};
use mllm_config::instances::{parse_instance_spec, InstanceSpec, PlacementStrategy};
use mllm_config::{parse_strict, ConfigErrorCode, ConfigKind};
use serde_json::json;

fn fixture() -> (serde_json::Value, serde_json::Value) {
    let all: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    (all["deployment"].clone(), all["host"].clone())
}

// T08: a document without instance fields keeps its pre-instance meaning.
#[test]
fn omitted_fields_mean_one_unconstrained_instance() {
    let (deployment, _) = fixture();
    assert_eq!(
        parse_instance_spec(&deployment).unwrap(),
        InstanceSpec::default()
    );
}

// T14: the declared shape is normalized, with the documented defaults.
#[test]
fn declared_instances_and_placement_parse() {
    let (mut deployment, _) = fixture();
    deployment["devices"] = json!([]);
    deployment["instances"] = json!(2);
    deployment["placement"] = json!({
        "hosts": ["host-a", "host-b"],
        "selector": {"gpu": "gb10"},
        "strategy": "pack",
        "max_per_host": 1,
    });
    let spec = parse_instance_spec(&deployment).unwrap();
    assert_eq!(spec.instances, 2);
    assert_eq!(
        spec.placement.hosts.as_deref(),
        Some(&["host-a".to_string(), "host-b".to_string()][..])
    );
    assert_eq!(spec.placement.selector["gpu"], "gb10");
    assert_eq!(spec.placement.strategy, PlacementStrategy::Pack);
    assert_eq!(spec.placement.max_per_host, Some(1));
    assert!(spec.placement.allows("host-b") && !spec.placement.allows("control-host"));
}

// T14: `host` is shorthand for a one-host allowed set.
#[test]
fn host_is_shorthand_for_a_single_allowed_host() {
    let (mut deployment, _) = fixture();
    deployment["host"] = json!("host-a");
    let spec = parse_instance_spec(&deployment).unwrap();
    assert_eq!(spec.instances, 1);
    assert_eq!(spec.placement.pinned_host(), Some("host-a"));
}

// T03: contradictory or unplaceable declarations are refused by name.
#[test]
fn contradictory_and_unplaceable_declarations_are_refused() {
    let (base, _) = fixture();
    let cases = [
        (json!({"host": "a", "placement": {"hosts": ["a"]}}), "host"),
        (json!({"instances": 0}), "instances"),
        (json!({"instances": 65}), "instances"),
        (json!({"instances": "two"}), "instances"),
        (json!({"placement": {"hosts": []}}), "placement.hosts"),
        (
            json!({"placement": {"hosts": ["a", "a"]}}),
            "placement.hosts",
        ),
        (
            json!({"placement": {"strategy": "random"}}),
            "placement.strategy",
        ),
        (
            json!({"placement": {"max_per_host": 0}}),
            "placement.max_per_host",
        ),
        (
            json!({"instances": 3, "placement": {"hosts": ["a", "b"], "max_per_host": 1}}),
            "instances",
        ),
        // ADR 0013 §2: a named device is local to one host.
        (json!({"placement": {"hosts": ["a", "b"]}}), "devices"),
    ];
    for (patch, path) in cases {
        let mut deployment = base.clone();
        for (key, value) in patch.as_object().unwrap() {
            deployment[key] = value.clone();
        }
        let error = parse_instance_spec(&deployment).unwrap_err();
        assert_eq!(error.path, path, "{patch}: {error}");
    }
    let mut unknown = base.clone();
    unknown["placement"] = json!({"zone": "x"});
    assert_eq!(
        parse_instance_spec(&unknown).unwrap_err().code,
        ConfigErrorCode::UnknownField
    );
}

// T03, T14: the strict schema accepts the fields, and resolution against a
// host accepts them without changing the per-host recipe.
#[test]
fn resolution_accepts_instances_without_changing_the_recipe() {
    let (deployment, host) = fixture();
    let before = resolve_effective(&deployment, &host).unwrap();
    let mut scaled = deployment.clone();
    scaled["instances"] = json!(3);
    scaled["placement"] = json!({"hosts": [host["name"].clone()], "strategy": "spread"});
    let yaml = serde_json::to_string(&scaled).unwrap();
    parse_strict(ConfigKind::Deployment, &yaml).unwrap();
    let after = resolve_effective(&scaled, &host).unwrap();
    assert_eq!(before.recipe_fingerprint, after.recipe_fingerprint);
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&after).unwrap()
    );
    let mut refused = deployment.clone();
    refused["instances"] = json!(0);
    assert!(resolve_effective(&refused, &host).is_err());
}

// T08: a command without instance fields keeps its fingerprint; a count is
// part of the command identity (ADR 0013 §7).
#[test]
fn command_identity_includes_count_only_when_declared() {
    let (deployment, _) = fixture();
    let base = deployment_command_fingerprint(&deployment, 30_000).unwrap();
    let mut one = deployment.clone();
    one["instances"] = json!(1);
    assert_eq!(deployment_command_fingerprint(&one, 30_000).unwrap(), base);
    let mut two = deployment.clone();
    two["instances"] = json!(2);
    assert_ne!(deployment_command_fingerprint(&two, 30_000).unwrap(), base);
}

// T14 T16 (SPEC §6.5, ADR 0013 amendment 2026-09-23): `lifecycle.warm` is the
// warm-residency commitment. Omitted it is false and changes neither the
// spec's command identity nor the per-host recipe; a non-boolean or an
// unknown lifecycle field is refused by name.
#[test]
fn lifecycle_warm_parses_and_leaves_the_recipe_unchanged() {
    let (deployment, host) = fixture();
    assert!(!parse_instance_spec(&deployment).unwrap().warm);
    assert!(InstanceSpec::default().command_identity().is_none());
    let mut warm = deployment.clone();
    warm["lifecycle"] = json!({"warm": true});
    let spec = parse_instance_spec(&warm).unwrap();
    assert!(spec.warm);
    assert!(spec.command_identity().is_some());
    assert!(parse_strict(ConfigKind::Deployment, &warm.to_string()).is_ok());
    assert_eq!(
        resolve_effective(&warm, &host).unwrap().recipe_fingerprint,
        resolve_effective(&deployment, &host)
            .unwrap()
            .recipe_fingerprint
    );
    warm["lifecycle"] = json!({"warm": "yes"});
    assert_eq!(
        parse_instance_spec(&warm).unwrap_err().path,
        "lifecycle.warm"
    );
    warm["lifecycle"] = json!({"pinned": true});
    assert_eq!(
        parse_instance_spec(&warm).unwrap_err().code,
        ConfigErrorCode::UnknownField
    );
    assert!(parse_strict(ConfigKind::Deployment, &warm.to_string()).is_err());
}

// T03 T14 (ADR 0013 §2): an unnamed device claim takes one of the host's
// devices that no named claim of the same deployment already holds. Picking a
// named device again doubled the claim and the deployment was refused for a
// conflict it never declared.
#[test]
fn an_unnamed_claim_never_takes_a_device_named_by_another_claim() {
    let host = json!({"resource_policy": {"devices": {"gpu0": {}, "gpu1": {}}}});
    let deployment = json!({"devices": [{"id": "gpu0"}, {}]});
    let assigned = mllm_config::instances::assign_devices(&deployment, &host).unwrap();
    // The named claim takes the host's sharing for its device (the short pin
    // form, final review I9); the host states none, so exclusive.
    assert_eq!(
        assigned["devices"],
        json!([{"id": "gpu0", "sharing": "exclusive"}, {"id": "gpu1"}])
    );
    // With only the named device on the host, the unnamed claim has none.
    let host = json!({"resource_policy": {"devices": {"gpu0": {}}}});
    assert!(mllm_config::instances::assign_devices(&deployment, &host).is_err());
}

/// A discrete host with two GPUs, each its own device-memory domain.
fn two_gpu_host() -> serde_json::Value {
    json!({"resource_policy": {
        "device_sharing": "shared",
        "domains": {
            "system": {"memory": "distinct"},
            "gpu0": {"memory": "device", "device": "gpu0"},
            "gpu1": {"memory": "device", "device": "gpu1"}
        },
        "devices": {
            "gpu1": {"domain": "gpu1", "sharing": "exclusive"},
            "gpu0": {"domain": "gpu0"}
        }
    }})
}

// T27: discrete GPU design §7: a deployment that pins no device is resolved
// once per GPU of a discrete host, lowest index first.
#[test]
fn an_unpinned_deployment_has_one_choice_per_gpu() {
    use mllm_config::instances::device_choices;
    let host = two_gpu_host();
    let choices = device_choices(&json!({"name": "d"}), &host).unwrap();
    let devices: Vec<_> = choices.iter().map(|(d, _)| d.as_str()).collect();
    assert_eq!(devices, ["gpu0", "gpu1"]);
    assert_eq!(
        choices[0].1["devices"],
        json!([{"id": "gpu0", "sharing": "shared"}])
    );
    assert_eq!(
        choices[1].1["devices"],
        json!([{"id": "gpu1", "sharing": "exclusive"}])
    );
    // An unnamed claim keeps its sharing, and takes only devices allowing it.
    let shared = json!({"devices": [{"sharing": "shared"}]});
    let choices = device_choices(&shared, &host).unwrap();
    assert_eq!(choices.len(), 1);
    assert_eq!(
        choices[0].1["devices"],
        json!([{"id": "gpu0", "sharing": "shared"}])
    );
    let exclusive = json!({"devices": [{"sharing": "exclusive"}]});
    assert_eq!(device_choices(&exclusive, &host).unwrap().len(), 2);
}

// T27: a pin, explicit resources, several claims or a host without device
// domains offer no choice: resolution proceeds as before.
#[test]
fn a_pinned_or_unified_deployment_has_no_choice() {
    use mllm_config::instances::device_choices;
    let host = two_gpu_host();
    for deployment in [
        json!({"devices": [{"id": "gpu1", "sharing": "shared"}]}),
        json!({"devices": [{}, {}]}),
        json!({"devices": [], "resources": {}}),
    ] {
        assert!(device_choices(&deployment, &host).unwrap().is_empty());
    }
    let (deployment, unified) = fixture();
    let mut unpinned = deployment.clone();
    unpinned.as_object_mut().unwrap().remove("devices");
    assert!(device_choices(&unpinned, &unified).unwrap().is_empty());
}

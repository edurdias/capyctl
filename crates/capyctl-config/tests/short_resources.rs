//! The short form of a deployment's `resources`: one GPU figure and one RAM
//! figure (`resources: {gpu: 11GiB, ram: 2GiB}`) stand for the five phases of
//! a model that restarts instead of parking. CPU-only resolution tests; none of
//! this qualifies an engine recipe (SPEC §18).

use capyctl_config::effective::{resolve_effective, validate_declared_resources, Residency};
use capyctl_config::instances::assign_devices;
use capyctl_config::short_resources::offline_phases;
use capyctl_config::{parse_strict, ConfigErrorCode, ConfigKind};
use serde_json::{json, Value};

/// The lab fixture with its profile turned into TensorFold, as the
/// TensorFold tests use it: one unified domain, one shared `gpu0`.
fn unified() -> (Value, Value) {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "tensorfold".into();
    profile["executable"] = "/opt/tf/bin/tensorfold".into();
    profile["build_fingerprint"] = "0.6.3".into();
    profile["args"] = json!([]);
    profile["security"]["deep_park"] = "disabled".into();
    deployment["residency"] = "restart_only".into();
    deployment["engine_config"] = json!({"context_length": 8192});
    (deployment, host)
}

/// The same with a discrete host: host RAM in `system`, the card in `gpu0`.
fn discrete() -> (Value, Value) {
    let (deployment, mut host) = unified();
    host["resource_policy"]["domains"] = json!({
        "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB",
                   "parked_limit": "12GiB", "host_kv_limit": "4GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "14848MiB",
                 "free_reserve": "1536MiB", "parked_limit": "2GiB"}
    });
    host["resource_policy"]["devices"] = json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
    (deployment, host)
}

fn long_form(allocations: Value) -> Value {
    let gpu = json!([{"id": "gpu0", "sharing": "shared"}]);
    let zero: Vec<Value> = allocations
        .as_array()
        .unwrap()
        .iter()
        .map(|a| json!({"domain": a["domain"], "bytes": "0B", "host_kv_bytes": "0B"}))
        .collect();
    let active = json!({"allocations": allocations, "devices": gpu});
    json!({"cold": active, "ready": active, "parking": active,
           "parked": {"allocations": zero, "devices": []}, "wake": active})
}

fn resolved(deployment: &Value, host: &Value) -> Value {
    serde_json::to_value(resolve_effective(deployment, host).unwrap()).unwrap()
}

// T03 T41: on a discrete GPU the short form is the long form the TensorFold
// recipe spells out, phase for phase, and the snapshot is the same.
#[test]
fn the_short_form_resolves_as_the_long_form_on_a_discrete_gpu() {
    let (mut long, host) = discrete();
    long["resources"] = long_form(json!([
        {"domain": "gpu0", "bytes": "11GiB", "host_kv_bytes": "0B"},
        {"domain": "system", "bytes": "2GiB", "host_kv_bytes": "0B"}
    ]));
    let mut short = long.clone();
    short["resources"] = json!({"gpu": "11GiB", "ram": "2GiB"});
    assert_eq!(resolved(&short, &host), resolved(&long, &host));
}

// T03 T41: a unified machine has one memory pool, so the GPU and RAM figures
// are both charged to it.
#[test]
fn on_a_unified_machine_both_figures_charge_the_one_pool() {
    let (mut long, host) = unified();
    long["resources"] = long_form(json!([
        {"domain": "unified", "bytes": "32GiB", "host_kv_bytes": "0B"}
    ]));
    let mut short = long.clone();
    short["resources"] = json!({"gpu": "30GiB", "ram": "2GiB"});
    assert_eq!(resolved(&short, &host), resolved(&long, &host));
}

// T14: the short form is a model that restarts, so an undeclared residency is
// restart_only, on any engine that takes declared resources.
#[test]
fn the_short_form_means_restart_only_on_any_engine() {
    let (mut deployment, mut host) = unified();
    host["runtime_profiles"]["local"] = fixture_vllm_profile();
    deployment.as_object_mut().unwrap().remove("residency");
    deployment["engine_config"] = json!({"memory": {"kv_cache": "20GiB"}});
    deployment["resources"] = json!({"gpu": "28GiB", "ram": "2GiB"});
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(effective.residency, Residency::RestartOnly);
    assert_eq!(effective.resources.parked.allocations[0].bytes, 0);
    assert_eq!(effective.resources.ready.allocations[0].bytes, 30 << 30);
}

fn fixture_vllm_profile() -> Value {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    all["host"]["runtime_profiles"]["local"].clone()
}

// T03: what the short form cannot mean is refused with its path.
#[test]
fn the_short_form_refuses_what_it_cannot_mean() {
    let (base, host) = discrete();
    let cases: Vec<(Value, &str)> = vec![
        // Both figures are stated.
        (json!({"resources": {"gpu": "11GiB"}}), "resources.ram"),
        (json!({"resources": {"ram": "2GiB"}}), "resources.gpu"),
        // Not mixed with the phases.
        (
            json!({"resources": {"gpu": "11GiB", "ram": "2GiB", "cold": long_form(json!([]))["cold"]}}),
            "resources",
        ),
        // A parking model holds memory parked; the short form holds none.
        (
            json!({"residency": "deep", "resources": {"gpu": "11GiB", "ram": "2GiB"}}),
            "resources",
        ),
        // One device.
        (
            json!({"devices": [{"id": "gpu0"}, {"id": "gpu1"}],
                   "resources": {"gpu": "11GiB", "ram": "2GiB"}}),
            "resources",
        ),
        // A GPU figure.
        (
            json!({"resources": {"gpu": "0B", "ram": "2GiB"}}),
            "resources.gpu",
        ),
    ];
    for (patch, path) in cases {
        let mut deployment = base.clone();
        for (key, value) in patch.as_object().unwrap() {
            deployment[key] = value.clone();
        }
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, path, "{patch}: {error}");
        let offline = validate_declared_resources(&deployment).unwrap_err();
        assert_eq!(offline.path, path, "{patch} offline: {offline}");
    }
}

// T03: a discrete host without one system domain cannot place the RAM figure.
#[test]
fn a_discrete_host_without_a_system_domain_is_refused() {
    let (mut deployment, mut host) = discrete();
    host["resource_policy"]["domains"]
        .as_object_mut()
        .unwrap()
        .remove("system");
    deployment["resources"] = json!({"gpu": "11GiB", "ram": "2GiB"});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.path.starts_with("resource"), "{error}");
}

// T03 T15: the strict parse takes the short form and refuses another key.
#[test]
fn the_strict_parse_takes_the_short_form() {
    let text = "name: m\nengine: tensorfold\nmodel: m\nresidency: restart_only\n\
                devices: [{id: gpu0}]\nresources: {gpu: 11GiB, ram: 2GiB}\n\
                engine_config: {context_length: 8192}\n";
    let document = parse_strict(ConfigKind::Deployment, text).unwrap();
    validate_declared_resources(&document).unwrap();
    let error = parse_strict(
        ConfigKind::Deployment,
        &text.replace("ram: 2GiB", "vram: 2GiB"),
    )
    .unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnknownField);
    let error = parse_strict(ConfigKind::Deployment, &text.replace("2GiB", "lots")).unwrap_err();
    assert_eq!(error.path, "resources.ram");
}

// T03 T15: offline, `validate config` shows what the five phases are.
#[test]
fn the_offline_view_spells_the_five_phases() {
    let active = json!({"gpu": "11GiB", "ram": "2GiB"});
    assert_eq!(
        offline_phases(&json!({"gpu": "11GiB", "ram": "2GiB"})),
        Some(json!({"cold": active, "ready": active, "parking": active,
                    "parked": {"gpu": "0B", "ram": "0B"}, "wake": active}))
    );
    assert_eq!(offline_phases(&long_form(json!([]))), None);
}

// T14 (ADR 0013 §2): an unnamed claim takes its host's device first, and the
// short form then charges that device.
#[test]
fn an_unnamed_claim_takes_its_device_before_the_short_form_expands() {
    let (mut deployment, host) = discrete();
    deployment["devices"] = json!([{"sharing": "shared"}]);
    deployment["resources"] = json!({"gpu": "11GiB", "ram": "2GiB"});
    let assigned = assign_devices(&deployment, &host).unwrap();
    let effective = resolve_effective(&assigned, &host).unwrap();
    assert_eq!(effective.resources.ready.allocations[0].domain, "gpu0");
    assert_eq!(effective.resources.ready.devices[0].id, "gpu0");
}

// T08: a short-form command has an identity of its own: the same figures are
// the same command, other figures another one.
#[test]
fn a_short_form_command_has_an_identity() {
    use capyctl_config::effective::deployment_command_fingerprint;
    let (mut deployment, _) = discrete();
    deployment["resources"] = json!({"gpu": "11GiB", "ram": "2GiB"});
    let first = deployment_command_fingerprint(&deployment, 600_000).unwrap();
    deployment["resources"] = json!({"ram": "2048MiB", "gpu": "11GiB"});
    assert_eq!(
        deployment_command_fingerprint(&deployment, 600_000).unwrap(),
        first
    );
    deployment["resources"]["gpu"] = json!("12GiB");
    assert_ne!(
        deployment_command_fingerprint(&deployment, 600_000).unwrap(),
        first
    );
    deployment["resources"] = json!({"gpu": "12GiB"});
    assert!(deployment_command_fingerprint(&deployment, 600_000).is_err());
}

use capyctl_config::effective::{compose_current_resource_controls, resolve_effective};
use capyctl_config::resource_controls::{ResourceContext, ResourceControls};
use serde_json::{json, Value};

fn fixture() -> (Value, Value, ResourceContext, ResourceControls) {
    let input: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let deployment = input["deployment"].clone();
    let host = input["host"].clone();
    let effective = resolve_effective(&deployment, &host).unwrap();
    (
        deployment,
        host,
        ResourceContext::from_host(&effective.host),
        ResourceControls::from_host(&effective.host),
    )
}

// Reusing startup-file numbers would silently restore old limits after an API update.
#[test]
fn persisted_controls_replace_only_resource_controls_before_resolution() {
    let (deployment, host, context, mut controls) = fixture();
    let original = host.clone();
    let domain = controls.domains.get_mut("unified").unwrap();
    domain.managed_limit = 24 << 30;
    domain.free_reserve = 20 << 30;
    domain.host_kv_limit = None;
    domain.parked_limit = Some(0);
    controls.max_parked = 0;
    controls.observation_ttl_ms = 1250;
    controls.planner_max_states = 128;
    controls.queue.max_pending_per_deployment = 4;
    controls.queue.max_pending_total = 12;
    controls.queue.max_buffered_bytes_total = 8192;
    controls.queue.request_deadline_ms = 350_000;
    controls.queue.admission_window_ms = 1000;
    // SPEC §10: a configured stream idle bound survives composition.
    controls.queue.stream_idle_ms = 90_000;
    let composed = compose_current_resource_controls(&host, &context, &controls).unwrap();
    assert_eq!(host, original);
    for key in [
        "name",
        "hardware_fingerprint",
        "environment_fingerprint",
        "runtime_profiles",
    ] {
        assert_eq!(composed[key], original[key]);
    }
    assert_eq!(
        composed["resource_policy"]["domains"]["unified"]["managed_limit"],
        "25769803776B"
    );
    assert!(composed["resource_policy"]["domains"]["unified"]
        .get("host_kv_limit")
        .is_none());
    let effective = resolve_effective(&deployment, &composed).unwrap();
    assert_eq!(ResourceContext::from_host(&effective.host), context);
    assert_eq!(ResourceControls::from_host(&effective.host), controls);
}

#[test]
fn current_sharing_and_deadline_restrictions_are_not_bypassed_by_stale_file() {
    let (deployment, host, context, mut controls) = fixture();
    assert!(resolve_effective(&deployment, &host).is_ok());
    controls.queue.request_deadline_ms = 200_000;
    let composed = compose_current_resource_controls(&host, &context, &controls).unwrap();
    assert!(resolve_effective(&deployment, &composed).is_err());
    controls.queue.request_deadline_ms = 600_000;
    controls.device_sharing = capyctl_config::effective::Sharing::Exclusive;
    controls
        .device_sharing_overrides
        .insert("gpu0".into(), capyctl_config::effective::Sharing::Exclusive);
    let composed = compose_current_resource_controls(&host, &context, &controls).unwrap();
    assert_eq!(
        composed["resource_policy"]["devices"]["gpu0"]["sharing"],
        "exclusive"
    );
    assert!(resolve_effective(&deployment, &composed).is_err());
}

#[test]
fn mismatched_immutable_context_or_invalid_controls_fail_without_repair() {
    let (_, host, context, controls) = fixture();
    let mut contexts = Vec::new();
    let mut changed = context.clone();
    changed.host_id = "other-host".into();
    contexts.push(changed);
    let mut changed = context.clone();
    changed.endpoint_port_range.end += 1;
    contexts.push(changed);
    let mut changed = context.clone();
    changed
        .device_domains
        .insert("gpu1".into(), "unified".into());
    contexts.push(changed);
    for changed in contexts {
        assert!(compose_current_resource_controls(&host, &changed, &controls).is_err());
    }
    let mut invalid = controls.clone();
    invalid.queue.max_pending_total = 0;
    assert!(compose_current_resource_controls(&host, &context, &invalid).is_err());
    let mut malformed = host.clone();
    malformed["resource_policy"]["unreviewed"] = json!(true);
    assert!(compose_current_resource_controls(&malformed, &context, &controls).is_err());
}

// T16: changing resource controls must retain frozen physical placement.
#[test]
fn current_controls_preserve_physical_device_identity() {
    let (deployment, mut host, context, mut controls) = fixture();
    let uuid = "GPU-09631200-fdff-a345-295f-a1a6f84b2f84";
    host["resource_policy"]["devices"]["gpu0"]["physical_gpu_uuid"] = json!(uuid);
    controls.observation_ttl_ms = 1250;
    let composed = compose_current_resource_controls(&host, &context, &controls).unwrap();
    let effective = resolve_effective(&deployment, &composed).unwrap();
    assert_eq!(
        effective.host.devices["gpu0"].physical_gpu_uuid.as_deref(),
        Some(uuid)
    );
    assert_eq!(ResourceControls::from_host(&effective.host), controls);
}

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

// T14, T39: the groups block is host context a group deploy reads (ADR 0028
// §3); composing the current controls keeps it, and it changes no resolution.
#[test]
fn current_controls_preserve_the_groups_block() {
    let (deployment, mut host, context, controls) = fixture();
    let plain = compose_current_resource_controls(&host, &context, &controls).unwrap();
    assert!(plain["resource_policy"].get("groups").is_none());
    host["resource_policy"]["groups"] = json!({"peer_address": "192.0.2.10"});
    let composed = compose_current_resource_controls(&host, &context, &controls).unwrap();
    assert_eq!(
        composed["resource_policy"]["groups"],
        json!({"peer_address": "192.0.2.10"})
    );
    assert_eq!(
        capyctl_config::groups_policy::host_groups_policy(&composed)
            .unwrap()
            .peer_address,
        Some("192.0.2.10".parse().unwrap())
    );
    assert_eq!(
        resolve_effective(&deployment, &composed).unwrap(),
        resolve_effective(&deployment, &plain).unwrap()
    );
}

// T03 T14 (ADR 0014 amendment A18): a host that states no
// `parked_growth_limit`, or `auto`, resolves, composes and fingerprints
// exactly as before the setting existed; a stated bound is carried through
// composition and the frozen snapshot.
#[test]
fn an_absent_or_auto_parked_growth_limit_changes_no_digest() {
    use capyctl_config::effective::{decode_effective_snapshot, ParkedGrowthLimit};
    let (deployment, host, context, controls) = fixture();
    assert_eq!(controls.parked_growth_limit, ParkedGrowthLimit::Auto);
    let plain = resolve_effective(&deployment, &host).unwrap();
    let mut auto = host.clone();
    auto["resource_policy"]["parked_growth_limit"] = json!("auto");
    let with_auto = resolve_effective(&deployment, &auto).unwrap();
    assert_eq!(
        serde_json::to_string(&with_auto).unwrap(),
        serde_json::to_string(&plain).unwrap(),
        "the frozen revision is byte for byte the same"
    );
    assert!(!serde_json::to_string(&plain)
        .unwrap()
        .contains("parked_growth_limit"));
    assert!(!serde_json::to_string(&controls)
        .unwrap()
        .contains("parked_growth_limit"));
    let composed = compose_current_resource_controls(&host, &context, &controls).unwrap();
    assert!(composed["resource_policy"]
        .get("parked_growth_limit")
        .is_none());

    for (stated, limit) in [
        ("50%", ParkedGrowthLimit::Percent(50)),
        ("8GiB", ParkedGrowthLimit::Bytes(8 << 30)),
        ("off", ParkedGrowthLimit::Off),
    ] {
        let mut bounded = host.clone();
        bounded["resource_policy"]["parked_growth_limit"] = json!(stated);
        let effective = resolve_effective(&deployment, &bounded).unwrap();
        assert_eq!(effective.host.parked_growth_limit, limit, "{stated}");
        let snapshot = serde_json::to_string(&effective).unwrap();
        assert_eq!(decode_effective_snapshot(&snapshot).unwrap(), effective);
        let controls = ResourceControls::from_host(&effective.host);
        let composed = compose_current_resource_controls(&host, &context, &controls).unwrap();
        assert_eq!(
            resolve_effective(&deployment, &composed)
                .unwrap()
                .host
                .parked_growth_limit,
            limit,
            "{stated} survives composition"
        );
    }
    let mut bad = host.clone();
    bad["resource_policy"]["parked_growth_limit"] = json!("lots");
    let error = resolve_effective(&deployment, &bad).unwrap_err();
    assert_eq!(error.path, "resource_policy.parked_growth_limit");
}

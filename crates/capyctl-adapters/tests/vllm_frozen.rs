//! The shared vLLM launch plan built from an already-resolved effective
//! deployment (Spec §3). The embedded coordinator and the remote host agent
//! both render through this one builder, so the live-proven S1 recipe cannot
//! drift between the two paths. Nothing here qualifies a native engine recipe.

use capyctl_adapters::vllm::{park_policy, plan_from_effective, render_command, VllmPlanError};
use capyctl_adapters::ParkPolicy;
use capyctl_config::effective::{resolve_effective, CudaNamespace, EffectiveDeployment};
use serde_json::{json, Value};

fn fixture() -> (Value, Value) {
    let source: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    (
        source["input"]["deployment"].clone(),
        source["input"]["host"].clone(),
    )
}

fn effective(deep_park: &str) -> EffectiveDeployment {
    let (mut deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["security"]["deep_park"] = json!(deep_park);
    if deep_park != "enabled" {
        deployment["residency"] = json!("restart_only");
    }
    resolve_effective(&deployment, &host).unwrap()
}

/// Spec §3: capyctl owns the listener, the port and the served name; the plan is
/// the frozen profile's, never something a caller supplies as argv.
// T14
#[test]
fn a_plan_renders_the_frozen_profile_on_the_leased_port() {
    let effective = effective("disabled");
    let plan = plan_from_effective(
        &effective,
        8123,
        "/var/log/capyctl/i-1.log".into(),
        "/opt/capyctl/runtime".into(),
    )
    .unwrap();
    assert_eq!(plan.engine_bin, "/bin/true");
    assert_eq!(plan.engine_path_extra.as_deref(), Some("/bin"));
    assert_eq!(plan.model_path, "/srv/models/toy");
    assert_eq!(plan.port, 8123);
    assert_eq!(plan.served_model_name, "toy");
    assert_eq!(plan.engine_args, vec!["--max-model-len", "4096"]);
    assert_eq!(plan.granted.kv_cache_bytes, Some(4 << 30));
    // ADR 0014 §3: the utilization gate is reserved and rendered by capyctl.
    assert_eq!(
        plan.granted.gpu_utilization_pct,
        Some(capyctl_adapters::vllm::GPU_UTILIZATION_GATE_PCT)
    );
    assert_eq!(plan.granted.swap_space_bytes, None);
    assert!(plan.api_key.is_none());
    assert_eq!(plan.engine_log.as_deref(), Some("/var/log/capyctl/i-1.log"));
    assert_eq!(plan.runtime_dir.as_deref(), Some("/opt/capyctl/runtime"));
    let rendered = render_command(&plan).unwrap();
    let argv = rendered.argv.join(" ");
    // Owner decision Q11: the installation's interpreter runs capyctl's entry.
    // SPEC §9.1 / T21: -B, no bytecode beside the checked runtime source.
    assert!(argv.starts_with(
        "/bin/python3 -B /opt/capyctl/runtime/vllm_entry.py serve /srv/models/toy --host 127.0.0.1"
    ));
    assert!(argv.contains("--port 8123"));
    assert!(argv.contains("--served-model-name toy"));
    assert!(!argv.contains("--api-key"));
}

/// SPEC §9.1: the development mode vLLM's park controls live behind is rendered
/// only when the host enabled deep park and the profile asks for sleep mode.
// T21
#[test]
fn sleep_mode_follows_the_host_deep_park_switch() {
    let disabled = effective("disabled");
    let plan = plan_from_effective(&disabled, 8123, "l".into(), "/r".into()).unwrap();
    assert!(plan.sleep_flags.is_empty());
    assert_eq!(park_policy(&disabled), ParkPolicy::Disabled);
    assert_eq!(
        render_command(&plan)
            .unwrap()
            .env
            .get("VLLM_SERVER_DEV_MODE"),
        Some(&"0".to_string())
    );

    let enabled = effective("enabled");
    let plan = plan_from_effective(&enabled, 8123, "l".into(), "/r".into()).unwrap();
    assert_eq!(
        plan.sleep_flags,
        vec![
            "--enable-sleep-mode",
            "--safetensors-load-strategy",
            "eager"
        ]
    );
    assert_eq!(park_policy(&enabled), ParkPolicy::Enabled);
    let rendered = render_command(&plan).unwrap();
    assert_eq!(
        rendered.env.get("VLLM_SERVER_DEV_MODE"),
        Some(&"1".to_string())
    );
    assert!(rendered
        .argv
        .windows(2)
        .any(|w| w == ["--middleware", "capyctl_vllm_guard.RequireEngineKey"]));
}

/// SPEC §6.2: `restart_only` prohibits sleep calls, so an enabled host switch
/// (the ADR 0012 default) still yields no park policy for a deployment that
/// declared the restart-only tier.
// T21
#[test]
fn a_restart_only_deployment_has_no_park_policy() {
    let (mut deployment, host) = fixture();
    deployment["residency"] = json!("restart_only");
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert!(effective.profile.security.deep_park.is_enabled());
    assert_eq!(park_policy(&effective), ParkPolicy::Disabled);
}

/// Spec §3: a plan is refused rather than invented when the frozen profile
/// carries another family's settings or the deployment serves no route.
// T14
#[test]
fn another_family_or_a_routeless_deployment_is_refused() {
    let mut routeless = effective("disabled");
    routeless.routes.clear();
    assert!(matches!(
        plan_from_effective(&routeless, 8123, "l".into(), "/r".into()),
        Err(VllmPlanError::NoRoute)
    ));

    let source: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-sglang-golden.json"
    ))
    .unwrap();
    let sglang =
        resolve_effective(&source["input"]["deployment"], &source["input"]["host"]).unwrap();
    let refused = plan_from_effective(&sglang, 8123, "l".into(), "/r".into()).unwrap_err();
    assert!(matches!(refused, VllmPlanError::OtherFamily));
    assert_eq!(
        refused.to_string(),
        "the frozen profile declares vLLM but carries another family's launch settings"
    );
}

/// ADR 0014 WE1 interim: the deployment's accepted extra arguments render after
/// the installation's host-fixed ones, its typed KV-cache dtype and block size
/// render in capyctl's block, and a typed field this build does not render yet is
/// refused by name rather than silently dropped (WE2 renders it).
// T14 T22
#[test]
fn deployment_engine_config_reaches_the_plan_or_is_refused_by_name() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"] = json!({
        "memory": {"kv_cache": "2GiB"}, "kv_cache_dtype": "fp8",
        "vllm": {"block_size_tokens": 32},
        "accept_extra_args": true, "extra_args": ["--reasoning-parser", "qwen3"],
    });
    let effective = resolve_effective(&deployment, &host).unwrap();
    let plan = plan_from_effective(&effective, 8123, "l".into(), "/r".into()).unwrap();
    // ADR 0014 §8: host-fixed arguments and the deployment's extras stay
    // apart; the extras render after their own marker, with the host's
    // approvals beside them for the entry's parsed-destination gate.
    assert_eq!(plan.engine_args, ["--max-model-len", "4096"]);
    assert_eq!(plan.extra_args, ["--reasoning-parser", "qwen3"]);
    assert!(plan.extra_approvals.is_some());
    assert_eq!(plan.granted.kv_cache_bytes, Some(2 << 30));
    let argv = render_command(&plan).unwrap().argv;
    for pair in [["--kv-cache-dtype", "fp8"], ["--block-size", "32"]] {
        assert!(argv.windows(2).any(|w| w == pair), "{pair:?} in {argv:?}");
    }

    // ADR 0014 WE2: every typed field renders; nothing is refused as
    // "not rendered yet" any more.
    let (mut deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["args"] = json!([]);
    deployment["engine_config"]["dtype"] = json!("bfloat16");
    deployment["engine_config"]["quantization"] = json!("modelopt_fp4");
    deployment["engine_config"]["context_length"] = json!(32768);
    deployment["engine_config"]["max_concurrent_requests"] = json!(16);
    deployment["engine_config"]["cuda_graphs"] = json!(false);
    deployment["engine_config"]["language_model_only"] = json!(true);
    deployment["engine_config"]["vllm"] = json!({"max_num_batched_tokens": 8192});
    let effective = resolve_effective(&deployment, &host).unwrap();
    let plan = plan_from_effective(&effective, 8123, "l".into(), "/r".into()).unwrap();
    let argv = render_command(&plan).unwrap().argv;
    for pair in [
        ["--dtype", "bfloat16"],
        ["--quantization", "modelopt_fp4"],
        ["--max-model-len", "32768"],
        ["--max-num-seqs", "16"],
        ["--max-num-batched-tokens", "8192"],
    ] {
        assert!(argv.windows(2).any(|w| w == pair), "{pair:?} in {argv:?}");
    }
    for flag in ["--enforce-eager", "--language-model-only"] {
        assert!(argv.iter().any(|a| a == flag), "{flag} in {argv:?}");
    }
    // Omitted KV dtype and block size render nothing (engine defaults).
    let (deployment, host) = fixture();
    let effective = resolve_effective(&deployment, &host).unwrap();
    let plan = plan_from_effective(&effective, 8123, "l".into(), "/r".into()).unwrap();
    let argv = render_command(&plan).unwrap().argv;
    assert!(!argv
        .iter()
        .any(|a| a == "--kv-cache-dtype" || a == "--block-size"));
}

/// Discrete GPU design §§6–7: with several GPUs, the selected one narrows the
/// engine's CUDA namespace (its published UUID, else its PCI-ordered index);
/// a one-device unified host keeps the agent's own pass-through as before.
// T27 T21
#[test]
fn the_selected_gpu_narrows_the_namespace_only_where_there_is_a_choice() {
    const UUID: &str = "GPU-11111111-1111-1111-1111-111111111111";
    let render = |devices: Value, selected: &str| {
        let (mut deployment, mut host) = fixture();
        deployment["residency"] = json!("restart_only");
        host["runtime_profiles"]["local"]["security"]["deep_park"] = json!("disabled");
        host["resource_policy"]["devices"] = devices;
        let claim = json!([{"id": selected, "sharing": "shared"}]);
        deployment["devices"] = claim.clone();
        for phase in ["cold", "ready", "parking", "wake"] {
            deployment["resources"][phase]["devices"] = claim.clone();
        }
        let effective = resolve_effective(&deployment, &host).unwrap();
        let plan = plan_from_effective(&effective, 8123, "l".into(), "/r".into()).unwrap();
        (
            plan.cuda_namespace.clone(),
            render_command(&plan)
                .unwrap()
                .env
                .get("CUDA_VISIBLE_DEVICES")
                .cloned(),
        )
    };
    let one =
        json!({"gpu0": {"domain": "unified", "sharing": "shared", "physical_gpu_uuid": UUID}});
    assert_eq!(render(one, "gpu0"), (None, None));
    let two = json!({
        "gpu0": {"domain": "unified", "sharing": "shared"},
        "gpu1": {"domain": "unified", "sharing": "shared", "physical_gpu_uuid": UUID}
    });
    assert_eq!(
        render(two.clone(), "gpu1"),
        (
            Some(CudaNamespace::Uuid(UUID.to_string())),
            Some(UUID.to_string())
        )
    );
    // No UUID published for the selected GPU: its index pins it, never a
    // pass-through of every GPU (review decision; see device_namespace.rs).
    assert_eq!(
        render(two, "gpu0"),
        (Some(CudaNamespace::PciIndex(0)), Some("0".to_string()))
    );
}

/// The golden deployment on a discrete host: one 16 GB card (`gpu0`, a device
/// domain) beside host RAM (`system`), its phases derived from a 12 GiB device
/// request.
fn discrete_effective() -> EffectiveDeployment {
    let (mut deployment, mut host) = fixture();
    host["resource_policy"]["domains"] = json!({
        "system": {"memory": "distinct", "managed_limit": "30GiB", "free_reserve": "12GiB",
                   "parked_limit": "15GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "14848MiB",
                 "free_reserve": "1536MiB", "parked_limit": "2GiB"}
    });
    host["resource_policy"]["devices"] = json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
    let object = deployment.as_object_mut().unwrap();
    object.remove("resources");
    deployment["engine_config"]["memory"] =
        json!({"request": "12GiB", "kv_cache": "4GiB", "startup": "12GiB"});
    resolve_effective(&deployment, &host).unwrap()
}

/// Discrete GPU design §6 (ADR 0019): on a device domain the utilization vLLM
/// checks at start is the device request's share of the observed card, not
/// the unified gate, and the KV bytes are the grant's. A launch whose card
/// total was not observed is refused before a plan exists.
// T26
#[test]
fn a_discrete_plan_sizes_utilization_from_the_device_request() {
    let effective = discrete_effective();
    // The card is charged the request and the CUDA context and graphs; vLLM
    // is sized from the request alone (76 % of the card below).
    assert_eq!(
        effective.ready_device_allocation(),
        Some((
            Some(0),
            (12 << 30) + capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES
        ))
    );
    // Without the card's total the plan keeps the gate; the launch paths
    // refuse such a launch before building it (`with_device_total`).
    let gated = plan_from_effective(&effective, 8123, "l".into(), "/r".into()).unwrap();
    assert_eq!(
        gated.granted.gpu_utilization_pct,
        Some(capyctl_adapters::vllm::GPU_UTILIZATION_GATE_PCT)
    );
    assert!(effective.clone().with_device_total(|_| None).is_err());
    assert!(effective
        .clone()
        .with_device_total(|index| (index == 1).then_some(16376 << 20))
        .is_err());
    let sized = effective
        .with_device_total(|index| (index == 0).then_some(16376 << 20))
        .unwrap();
    let plan = plan_from_effective(&sized, 8123, "l".into(), "/r".into()).unwrap();
    assert_eq!(plan.granted.gpu_utilization_pct, Some(76));
    assert_eq!(plan.granted.kv_cache_bytes, Some(4 << 30));
    let argv = render_command(&plan).unwrap().argv;
    assert!(argv
        .windows(2)
        .any(|w| w == ["--gpu-memory-utilization", "0.76"]));
    // A unified deployment is never given a device total.
    let unified = effective_with_deep_park_disabled();
    let same = unified.clone().with_device_total(|_| Some(1)).unwrap();
    assert_eq!(same.engine_config.memory().device_total_bytes, None);
}

fn effective_with_deep_park_disabled() -> EffectiveDeployment {
    effective("disabled")
}

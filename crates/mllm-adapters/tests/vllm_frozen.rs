//! The shared vLLM launch plan built from an already-resolved effective
//! deployment (Spec §3). The embedded coordinator and the remote host agent
//! both render through this one builder, so the live-proven S1 recipe cannot
//! drift between the two paths. Nothing here qualifies a native engine recipe.

use mllm_adapters::vllm::{park_policy, plan_from_effective, render_command, VllmPlanError};
use mllm_adapters::ParkPolicy;
use mllm_config::effective::{resolve_effective, EffectiveDeployment};
use serde_json::{json, Value};

fn fixture() -> (Value, Value) {
    let source: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
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

/// Spec §3: mllm owns the listener, the port and the served name; the plan is
/// the frozen profile's, never something a caller supplies as argv.
// T14
#[test]
fn a_plan_renders_the_frozen_profile_on_the_leased_port() {
    let effective = effective("disabled");
    let plan = plan_from_effective(
        &effective,
        8123,
        "/var/log/mllm/i-1.log".into(),
        "/opt/mllm/runtime".into(),
    )
    .unwrap();
    assert_eq!(plan.engine_bin, "/bin/true");
    assert_eq!(plan.engine_path_extra.as_deref(), Some("/bin"));
    assert_eq!(plan.model_path, "/srv/models/toy");
    assert_eq!(plan.port, 8123);
    assert_eq!(plan.served_model_name, "toy");
    assert_eq!(plan.engine_args, vec!["--max-model-len", "4096"]);
    assert_eq!(plan.granted.kv_cache_bytes, Some(4 << 30));
    // ADR 0014 §3: the utilization gate is reserved and rendered by mllm.
    assert_eq!(
        plan.granted.gpu_utilization_pct,
        Some(mllm_adapters::vllm::GPU_UTILIZATION_GATE_PCT)
    );
    assert_eq!(plan.granted.swap_space_bytes, None);
    assert!(plan.api_key.is_none());
    assert_eq!(plan.engine_log.as_deref(), Some("/var/log/mllm/i-1.log"));
    assert_eq!(plan.runtime_dir.as_deref(), Some("/opt/mllm/runtime"));
    let rendered = render_command(&plan).unwrap();
    let argv = rendered.argv.join(" ");
    // Owner decision Q11: the installation's interpreter runs mllm's entry.
    // SPEC §9.1 / T21: -B, no bytecode beside the checked runtime source.
    assert!(argv.starts_with(
        "/bin/python3 -B /opt/mllm/runtime/vllm_entry.py serve /srv/models/toy --host 127.0.0.1"
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
        .any(|w| w == ["--middleware", "mllm_vllm_guard.RequireEngineKey"]));
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
        "../../mllm-config/tests/fixtures/effective-sglang-golden.json"
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
/// render in mllm's block, and a typed field this build does not render yet is
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
    assert!(!argv.iter().any(|a| a == "--kv-cache-dtype" || a == "--block-size"));
}

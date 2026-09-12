//! T14 contract: argument rendering with reserved-flag conflicts, budget
//! mapping, policy-scoped sleep flags, and secret redaction in
//! fingerprints.

use mllm_adapters::vllm::args::{
    fingerprint_of, render_command, ArgsError, GrantedBudget, PlanInputVllm, RESERVED_FLAGS,
};

fn base_input() -> PlanInputVllm {
    PlanInputVllm {
        engine_bin: "/opt/vllm/bin/vllm".into(),
        model_path: "/srv/models/toy-model".into(),
        port: 8150,
        granted: GrantedBudget {
            kv_cache_bytes: Some(16 * 1024 * 1024 * 1024),
            gpu_utilization_pct: Some(75),
            swap_space_bytes: None,
        },
        engine_args: vec!["--max-model-len".into(), "65536".into()],
        sleep_flags: vec![],
        api_key: None,
    }
}

#[test]
fn budgets_render_with_explicit_units() {
    let cmd = render_command(&base_input()).unwrap();
    let argv = &cmd.argv;
    // mllm-controlled flags first:
    let port = argv.iter().position(|a| a == "--port").unwrap();
    assert_eq!(argv[port + 1], "8150");
    let util = argv.iter().position(|a| a == "--gpu-memory-utilization").unwrap();
    assert_eq!(argv[util + 1], "0.75");
    let kv = argv.iter().position(|a| a == "--kv-cache-bytes").unwrap();
    assert_eq!(argv[kv + 1], (16u64 * 1024 * 1024 * 1024).to_string());
    // User pass-through args come after the mllm-controlled block:
    let max_len = argv.iter().position(|a| a == "--max-model-len").unwrap();
    assert!(max_len > kv, "user args after controlled args");
}

#[test]
fn reserved_flag_conflicts_fail() {
    let mut input = base_input();
    input.engine_args.push("--port".into());
    input.engine_args.push("9999".into());
    assert!(matches!(
        render_command(&input),
        Err(ArgsError::ReservedConflict(f)) if f == "--port"
    ));
}

#[test]
fn reserved_flag_list_is_the_pinned_set() {
    for flag in [
        "--port",
        "--device",
        "--gpu-memory-utilization",
        "--swap-space",
        "--kv-cache-bytes",
        "--enable-sleep-mode",
        "--api-key",
    ] {
        assert!(RESERVED_FLAGS.contains(&flag), "{flag} must be reserved");
    }
}

#[test]
fn user_sleep_flag_is_reserved_not_rendered() {
    let mut input = base_input();
    input.engine_args.push("--enable-sleep-mode".into());
    assert!(matches!(
        render_command(&input),
        Err(ArgsError::ReservedConflict(f)) if f == "--enable-sleep-mode"
    ));
}

#[test]
fn sleep_flags_render_only_when_profile_gated_in() {
    let mut gated = base_input();
    gated.sleep_flags = vec!["--enable-sleep-mode".into()];
    let cmd = render_command(&gated).unwrap();
    assert!(cmd.argv.contains(&"--enable-sleep-mode".to_string()));

    let ungated = base_input();
    let cmd2 = render_command(&ungated).unwrap();
    assert!(!cmd2.argv.contains(&"--enable-sleep-mode".to_string()));
}

#[test]
fn fingerprint_redacts_api_key_values() {
    let mut gated = base_input();
    gated.sleep_flags = vec!["--enable-sleep-mode".into()];
    gated.api_key = Some("secret123".into());
    let cmd = render_command(&gated).unwrap();
    // The rendered command itself carries the key (the engine needs it);
    // the fingerprint recorded in provenance must not.
    assert!(cmd.argv.contains(&"secret123".to_string()));
    let fp = fingerprint_of(&cmd);
    assert!(fp.contains("--api-key <redacted>"), "fp: {fp}");
    assert!(!fp.contains("secret123"));
}
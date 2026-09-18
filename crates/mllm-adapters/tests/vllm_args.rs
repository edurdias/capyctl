//! T14 contract: argument rendering with reserved-flag conflicts, budget
//! mapping, policy-scoped sleep flags, and secret redaction in
//! fingerprints.

use mllm_adapters::traits::RenderedCommand;
use mllm_adapters::vllm::args::{
    fingerprint_of, render_command, ArgsError, GrantedBudget, PlanInputVllm, RESERVED_FLAGS,
};

fn base_input() -> PlanInputVllm {
    PlanInputVllm {
        engine_bin: "/opt/vllm/bin/vllm".into(),
        model_path: "/srv/models/toy-model".into(),
        port: 8150,
        served_model_name: "gate-m".into(),
        tensor_parallel_size: 1,
        pipeline_parallel_size: 1,
        kv_cache_dtype: "auto".into(),
        block_size_tokens: 16,
        cpu_offload_bytes: 0,
        granted: GrantedBudget {
            kv_cache_bytes: Some(16 * 1024 * 1024 * 1024),
            gpu_utilization_pct: Some(75),
            swap_space_bytes: None,
        },
        engine_args: vec!["--max-model-len".into(), "65536".into()],
        sleep_flags: vec![],
        engine_path_extra: None,
        engine_log: None,
        api_key: None,
        runtime_dir: None,
    }
}

/// Alias matching the brief's fixture name.
fn plan() -> PlanInputVllm {
    base_input()
}

/// Spec §3: assert a `--flag value` pair is present in argv.
fn assert_flag(cmd: &RenderedCommand, flag: &str, value: &str) {
    let pos = cmd
        .argv
        .iter()
        .position(|a| a == flag)
        .unwrap_or_else(|| panic!("missing flag `{flag}` in {:?}", cmd.argv));
    assert_eq!(cmd.argv[pos + 1], value, "flag `{flag}`");
}

#[test]
fn budgets_render_with_explicit_units() {
    let cmd = render_command(&base_input()).unwrap();
    let argv = &cmd.argv;
    // mllm-controlled flags first:
    let port = argv.iter().position(|a| a == "--port").unwrap();
    assert_eq!(argv[port + 1], "8150");
    let util = argv
        .iter()
        .position(|a| a == "--gpu-memory-utilization")
        .unwrap();
    assert_eq!(argv[util + 1], "0.75");
    let kv = argv.iter().position(|a| a == "--kv-cache-memory").unwrap();
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
fn reserved_flags_are_normalized_and_equals_form_conflicts() {
    for argument in [
        "--HOST=127.0.0.1",
        "--served_model_name=other",
        "--tensor_parallel_size=2",
        "--disable-log-requests",
        "--api_key=secret",
    ] {
        let mut input = base_input();
        input.engine_args = vec![argument.into()];
        assert!(
            matches!(render_command(&input), Err(ArgsError::ReservedConflict(_))),
            "{argument}"
        );
    }
}

#[test]
fn aliases_normalize_before_duplicate_detection() {
    let mut input = base_input();
    input.engine_args = vec![
        "--max-model-len=4096".into(),
        "--max_model_len".into(),
        "8192".into(),
    ];
    assert!(matches!(
        render_command(&input),
        Err(ArgsError::DuplicateFlag(f)) if f == "--max-model-len"
    ));
}

#[test]
fn reserved_flag_cannot_hide_in_a_missing_ordinary_value() {
    let mut input = base_input();
    input.engine_args = vec!["--max-model-len".into(), "--port=9999".into()];
    assert!(matches!(
        render_command(&input),
        Err(ArgsError::MissingValue(f)) if f == "--max-model-len"
    ));
}

#[test]
fn unreviewed_flags_are_not_arbitrary_passthrough() {
    let mut input = base_input();
    input.engine_args = vec!["--future-unsafe-flag".into()];
    assert!(matches!(
        render_command(&input),
        Err(ArgsError::UnsupportedFlag(f)) if f == "--future-unsafe-flag"
    ));
}

#[test]
fn standalone_positionals_and_empty_values_are_rejected() {
    let mut input = base_input();
    input.engine_args = vec!["unreviewed-positional".into()];
    assert!(matches!(render_command(&input), Err(ArgsError::UnexpectedArgument(_))));
    input.engine_args = vec!["--max-model-len=".into()];
    assert!(matches!(render_command(&input), Err(ArgsError::MissingValue(_))));
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
fn odd_length_engine_args_parse_as_flag_value_pairs() {
    // An odd-length pass-through list parses by position: `flag value`
    // pairs plus a bare engine-native positional — never read as a flag
    // (the old chunks(2) parse misread the odd tail as a flag).
    let mut input = base_input();
    input.engine_args = vec![
        "--max-model-len".into(),
        "65536".into(),
        "--trust-remote-code".into(),
    ];
    let cmd = render_command(&input).unwrap();
    let pos = |a: &str| cmd.argv.iter().position(|x| x == a).unwrap();
    assert_eq!(cmd.argv[pos("--max-model-len") + 1], "65536");
    assert!(cmd.argv.contains(&"--trust-remote-code".to_string()));
}

#[test]
fn duplicate_ordinary_flag_is_duplicate_error_not_reserved() {
    // A duplicate NON-reserved flag is its own error class (T14): the old
    // behavior misreported it as ReservedConflict.
    let mut input = base_input();
    input.engine_args = vec![
        "--max-model-len".into(),
        "a".into(),
        "--max-model-len".into(),
        "b".into(),
    ];
    assert!(matches!(
        render_command(&input),
        Err(ArgsError::DuplicateFlag(f)) if f == "--max-model-len"
    ));
}

#[test]
fn duplicate_reserved_flag_still_conflicts() {
    // The reserved check wins: a reserved flag is a ReservedConflict on
    // first sight, duplicate or not.
    let mut input = base_input();
    input.engine_args = vec!["--port".into(), "9999".into(), "--port".into()];
    assert!(matches!(
        render_command(&input),
        Err(ArgsError::ReservedConflict(f)) if f == "--port"
    ));
}

#[test]
fn sleep_flags_render_only_when_profile_gated_in() {
    let mut gated = base_input();
    gated.sleep_flags = vec!["--enable-sleep-mode".into()];
    gated.runtime_dir = Some("/opt/mllm/runtime".into());
    let cmd = render_command(&gated).unwrap();
    assert!(cmd.argv.contains(&"--enable-sleep-mode".to_string()));
    assert_eq!(
        cmd.env.get("VLLM_SERVER_DEV_MODE").map(String::as_str),
        Some("1")
    );

    let ungated = base_input();
    let cmd2 = render_command(&ungated).unwrap();
    assert!(!cmd2.argv.contains(&"--enable-sleep-mode".to_string()));
    assert_eq!(
        cmd2.env.get("VLLM_SERVER_DEV_MODE").map(String::as_str),
        Some("0")
    );
}

/// Spec §3: mllm owns the listener address and the served name; profiles
/// cannot set them.
// T14
#[test]
fn render_emits_host_and_served_name() {
    let cmd = render_command(&plan()).unwrap();
    assert_flag(&cmd, "--host", "127.0.0.1");
    assert_flag(&cmd, "--served-model-name", "gate-m");
}

/// Spec §3: every validated launch setting reaches the engine.
// T14
#[test]
fn render_emits_the_five_launch_settings() {
    let mut p = plan();
    p.tensor_parallel_size = 2;
    p.pipeline_parallel_size = 1;
    p.kv_cache_dtype = "fp8".into();
    p.block_size_tokens = 32;
    p.cpu_offload_bytes = 4 * 1024 * 1024 * 1024;
    let cmd = render_command(&p).unwrap();
    assert_flag(&cmd, "--tensor-parallel-size", "2");
    assert_flag(&cmd, "--pipeline-parallel-size", "1");
    assert_flag(&cmd, "--kv-cache-dtype", "fp8");
    assert_flag(&cmd, "--block-size", "32");
    assert_flag(&cmd, "--cpu-offload-gb", "4");

    let mut none = plan();
    none.cpu_offload_bytes = 0;
    assert!(!render_command(&none)
        .unwrap()
        .argv
        .contains(&"--cpu-offload-gb".to_string()));
}

/// Spec §3: a positive CPU-offload budget below 1 GiB would silently round
/// to zero, so it is refused instead.
// T14
#[test]
fn cpu_offload_below_one_gib_is_invalid() {
    let mut p = plan();
    p.cpu_offload_bytes = 512 * 1024 * 1024;
    assert!(matches!(
        render_command(&p),
        Err(ArgsError::InvalidBudget(_))
    ));
}

/// Spec §3: development mode always comes with mllm's guard, and only
/// then; without a runtime dir it cannot be rendered at all.
// T21
#[test]
fn dev_mode_renders_the_guard_middleware() {
    let mut p = plan();
    p.sleep_flags = vec!["--enable-sleep-mode".into()];
    p.runtime_dir = Some("/opt/mllm/runtime".into());
    let cmd = render_command(&p).unwrap();
    assert_flag(&cmd, "--middleware", "mllm_vllm_guard.RequireEngineKey");
    assert_eq!(
        cmd.env.get("VLLM_SERVER_DEV_MODE").map(String::as_str),
        Some("1")
    );
    assert!(cmd
        .env
        .get("PYTHONPATH")
        .unwrap()
        .starts_with("/opt/mllm/runtime"));

    let mut off = plan();
    off.sleep_flags.clear();
    assert!(!render_command(&off)
        .unwrap()
        .argv
        .contains(&"--middleware".to_string()));

    let mut no_dir = plan();
    no_dir.sleep_flags = vec!["--enable-sleep-mode".into()];
    no_dir.runtime_dir = None;
    assert!(
        render_command(&no_dir).is_err(),
        "dev mode without a runtime dir cannot be rendered"
    );
}

/// Spec §3: the key never reaches argv even if a caller sets it on the plan.
// T37
#[test]
fn render_never_emits_api_key() {
    let mut p = plan();
    p.api_key = Some("secret".into());
    assert!(!render_command(&p)
        .unwrap()
        .argv
        .iter()
        .any(|a| a == "--api-key" || a == "secret"));
}

/// Spec §3/§8.2: `fingerprint_of`'s redaction path still compiles and still
/// redacts defensively, even though `render_command` itself never emits
/// `--api-key` on argv any more.
// T37
#[test]
fn fingerprint_still_redacts_api_key_if_present() {
    let cmd = RenderedCommand {
        argv: vec![
            "vllm".into(),
            "serve".into(),
            "--api-key".into(),
            "secret123".into(),
        ],
        env: Default::default(),
    };
    let fp = fingerprint_of(&cmd);
    assert!(fp.contains("--api-key <redacted>"), "fp: {fp}");
    assert!(!fp.contains("secret123"));
}

/// Spec §3: engine output is quoted into errors and journals, so the three
/// shapes a credential takes in it are blanked before it travels.
// T37
#[test]
fn redaction_blanks_the_credential_shapes_engine_output_carries() {
    let text = mllm_adapters::vllm::args::redact_text(
        "INFO header Authorization: Bearer sk-live-4242 accepted\n\
         env VLLM_API_KEY=deadbeefcafe started\n\
         token 0123456789abcdef0123456789abcdef0123456789abcdef logged\n",
    );
    assert!(text.contains("Bearer <redacted>"), "{text}");
    assert!(text.contains("VLLM_API_KEY=<redacted>"), "{text}");
    assert!(!text.contains("sk-live-4242"), "{text}");
    assert!(!text.contains("deadbeefcafe"), "{text}");
    assert!(
        !text.contains("0123456789abcdef0123456789abcdef0123456789abcdef"),
        "{text}"
    );
    // Redaction must leave the operator something to read.
    assert!(text.contains("INFO header"), "{text}");
    assert!(text.contains("started"), "{text}");
}

/// Redaction that ate ordinary words would make an engine log useless, and an
/// operator who cannot read the log has no reason to keep it. The two shapes that
/// used to be eaten are what vLLM prints on every load: the checkpoint directory
/// and the 40-character revision it resolved.
// T14
#[test]
fn redaction_leaves_ordinary_engine_output_alone() {
    use mllm_adapters::vllm::args::redact_text;
    let line = "loading weights: 12 shards, block_size=16, model gate-m ready";
    assert_eq!(redact_text(line), line);

    let path = "/srv/models/Qwen/Qwen3-4B-Instruct-2507/model-00001-of-00003";
    let revision = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(revision.len(), 40);
    let load = format!("loading {path} at revision {revision}");
    assert_eq!(redact_text(&load), load);

    // A key mllm issues is 32 bytes, hex-encoded to 64 characters, and still goes.
    let key = "a".repeat(64);
    let logged = format!("engine echoed {key} on startup");
    let redacted = redact_text(&logged);
    assert!(!redacted.contains(&key), "{redacted}");
    assert!(redacted.contains("<redacted>"), "{redacted}");

    // A path keeps its segments, and a key carried inside one is still blanked.
    let in_path = format!("GET /v1/models/{key}/info refused");
    let redacted = redact_text(&in_path);
    assert_eq!(redacted, "GET /v1/models/<redacted>/info refused");
    assert_eq!(redact_text("/a/b/"), "/a/b/");
}

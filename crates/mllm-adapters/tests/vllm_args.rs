//! T14 contract: argument rendering with reserved-flag conflicts, budget
//! mapping, policy-scoped sleep flags, and secret redaction in
//! fingerprints.

use mllm_adapters::traits::RenderedCommand;
use mllm_adapters::vllm::args::{
    fingerprint_of, interpreter_for, render_command, ArgsError, GrantedBudget, PlanInputVllm,
    EXTRA_ARGS_MARKER, RESERVED_FLAGS, USER_ARGS_MARKER,
};

fn base_input() -> PlanInputVllm {
    PlanInputVllm {
        engine_bin: "/opt/vllm/bin/vllm".into(),
        model_path: "/srv/models/toy-model".into(),
        port: 8150,
        served_model_name: "gate-m".into(),
        tensor_parallel_size: 1,
        pipeline_parallel_size: 1,
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
        runtime_dir: Some("/opt/mllm/runtime".into()),
        ..PlanInputVllm::default()
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
    let kv = argv
        .iter()
        .position(|a| a == "--kv-cache-memory-bytes")
        .unwrap();
    assert_eq!(argv[kv + 1], (16u64 * 1024 * 1024 * 1024).to_string());
    // User pass-through args come after the mllm-controlled block and the
    // marker the protected entry splits on (ADR 0014 §6):
    let marker = argv.iter().position(|a| a == USER_ARGS_MARKER).unwrap();
    let max_len = argv.iter().position(|a| a == "--max-model-len").unwrap();
    assert!(
        marker > kv && max_len > marker,
        "user args after controlled args"
    );
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
        "--mllm-user-args",
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

/// ADR 0014 §6: mllm does not know an option's arity, so a reserved option in
/// the value position of an ordinary one is still read as an option and refused.
// T14
#[test]
fn reserved_flag_cannot_hide_in_a_missing_ordinary_value() {
    let mut input = base_input();
    input.engine_args = vec!["--max-model-len".into(), "--port=9999".into()];
    assert!(matches!(
        render_command(&input),
        Err(ArgsError::ReservedConflict(f)) if f == "--port"
    ));
}

/// ADR 0014 §6 (owner decision Q10): ordinary engine options pass through and
/// render after mllm's block; reserved ones never do, however abbreviated.
// T14 T21
#[test]
fn ordinary_flags_pass_through_and_reserved_abbreviations_do_not() {
    let mut input = base_input();
    input.engine_args = vec!["--future-ordinary-flag".into(), "1".into()];
    let argv = render_command(&input).unwrap().argv;
    assert_eq!(&argv[argv.len() - 2..], ["--future-ordinary-flag", "1"]);
    for reserved in [
        "--gpu-memory-util=0.9",
        "--no-enable-sleep-mode",
        "--config=x",
    ] {
        input.engine_args = vec![reserved.into()];
        assert!(
            matches!(render_command(&input), Err(ArgsError::ReservedConflict(_))),
            "{reserved}"
        );
    }
}

#[test]
fn standalone_positionals_and_empty_values_are_rejected() {
    let mut input = base_input();
    input.engine_args = vec!["unreviewed-positional".into()];
    assert!(matches!(
        render_command(&input),
        Err(ArgsError::UnexpectedArgument(_))
    ));
    input.engine_args = vec!["--max-model-len=".into()];
    assert!(matches!(
        render_command(&input),
        Err(ArgsError::MissingValue(_))
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
fn render_emits_the_reserved_launch_settings() {
    let mut p = plan();
    p.tensor_parallel_size = 2;
    p.pipeline_parallel_size = 1;
    p.cpu_offload_bytes = 4 * 1024 * 1024 * 1024;
    let cmd = render_command(&p).unwrap();
    assert_flag(&cmd, "--tensor-parallel-size", "2");
    assert_flag(&cmd, "--pipeline-parallel-size", "1");
    assert_flag(&cmd, "--cpu-offload-gb", "4");

    let mut none = plan();
    none.cpu_offload_bytes = 0;
    assert!(!render_command(&none)
        .unwrap()
        .argv
        .contains(&"--cpu-offload-gb".to_string()));
}

/// ADR 0014 §2: every typed field renders in vLLM 0.29.0's spelling, after
/// the marker; omitted fields render nothing so the engine default applies.
// T14 T22
#[test]
fn typed_fields_render_after_the_marker_and_omitted_ones_do_not() {
    let mut p = plan();
    p.engine_args.clear();
    let bare = render_command(&p).unwrap().argv;
    for flag in [
        "--dtype",
        "--quantization",
        "--kv-cache-dtype",
        "--block-size",
        "--max-model-len",
        "--max-num-seqs",
        "--max-num-batched-tokens",
        "--enforce-eager",
        "--language-model-only",
        "--trust-remote-code",
    ] {
        assert!(!bare.contains(&flag.to_string()), "{flag} rendered unasked");
    }
    assert_eq!(bare.last().map(String::as_str), Some(USER_ARGS_MARKER));
    p.dtype = Some("bfloat16".into());
    p.quantization = Some("modelopt_fp4".into());
    p.kv_cache_dtype = Some("fp8".into());
    p.block_size_tokens = Some(32);
    p.context_length = Some(32768);
    p.max_concurrent_requests = Some(16);
    p.max_num_batched_tokens = Some(8192);
    p.enforce_eager = true;
    p.language_model_only = true;
    p.trust_remote_code = true;
    let cmd = render_command(&p).unwrap();
    let marker = cmd.argv.iter().position(|a| a == USER_ARGS_MARKER).unwrap();
    for (flag, value) in [
        ("--dtype", "bfloat16"),
        ("--quantization", "modelopt_fp4"),
        ("--kv-cache-dtype", "fp8"),
        ("--block-size", "32"),
        ("--max-model-len", "32768"),
        ("--max-num-seqs", "16"),
        ("--max-num-batched-tokens", "8192"),
    ] {
        assert_flag(&cmd, flag, value);
        let at = cmd.argv.iter().position(|a| a == flag).unwrap();
        assert!(at > marker, "{flag} after the marker");
    }
    for flag in [
        "--enforce-eager",
        "--language-model-only",
        "--trust-remote-code",
    ] {
        assert!(cmd.argv.contains(&flag.to_string()), "{flag}");
    }
}

/// ADR 0014 §2: a typed field the deployment set cannot be said again by a
/// host-fixed or extra argument; unset, the same option is ordinary.
// T14
#[test]
fn typed_fields_are_not_duplicated_by_pass_through_arguments() {
    let mut p = plan();
    p.context_length = Some(4096);
    assert!(matches!(
        render_command(&p),
        Err(ArgsError::DuplicateFlag(f)) if f == "--max-model-len"
    ));
    let mut p = plan();
    p.enforce_eager = true;
    p.engine_args = vec!["--enforce_eager".into()];
    assert!(matches!(
        render_command(&p),
        Err(ArgsError::DuplicateFlag(_))
    ));
    let mut p = plan();
    p.engine_args = vec!["--kv-cache-dtype".into(), "fp8".into()];
    assert!(render_command(&p).is_ok());
}

/// Owner decision Q11: the installation's interpreter runs mllm's protected
/// entry, which parses with vLLM's own parser and serves in process.
// T14 T21
#[test]
fn every_launch_execs_through_the_protected_entry() {
    let cmd = render_command(&plan()).unwrap();
    assert_eq!(
        &cmd.argv[..5],
        [
            "/opt/vllm/bin/python3",
            // SPEC §9.1 / T21: no bytecode is written beside checked source.
            "-B",
            "/opt/mllm/runtime/vllm_entry.py",
            "serve",
            "/srv/models/toy-model"
        ]
    );
    assert_eq!(
        cmd.argv.iter().filter(|a| *a == USER_ARGS_MARKER).count(),
        1
    );
    assert_eq!(
        interpreter_for("/venv/bin/python3.12").unwrap(),
        "/venv/bin/python3.12"
    );
    assert_eq!(
        interpreter_for("/venv/bin/vllm").unwrap(),
        "/venv/bin/python3"
    );
    assert!(interpreter_for("vllm").is_err());
    let mut p = plan();
    p.runtime_dir = None;
    assert!(matches!(
        render_command(&p),
        Err(ArgsError::MissingRuntimeDir)
    ));
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
    // SPEC §9.1 / T21: the runtime directory alone, never an inherited path.
    assert_eq!(
        cmd.env.get("PYTHONPATH").map(String::as_str),
        Some("/opt/mllm/runtime")
    );

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

/// ADR 0014 §8, SPEC §8.2: the deployment's own extras follow a second marker,
/// after the typed and host-fixed arguments, and the host's approvals ride the
/// environment, so the entry gates exactly what the extras resolved to.
// T21 T22
#[test]
fn extras_follow_their_own_marker_with_the_host_approvals() {
    let mut p = plan();
    p.extra_args = vec!["--reasoning-parser".into(), "qwen3".into()];
    p.extra_approvals = Some(r#"{"options":[],"paths":[],"trust_remote_code":false}"#.into());
    let cmd = render_command(&p).unwrap();
    let user = cmd.argv.iter().position(|a| a == USER_ARGS_MARKER).unwrap();
    let extra = cmd
        .argv
        .iter()
        .position(|a| a == EXTRA_ARGS_MARKER)
        .unwrap();
    let fixed = cmd
        .argv
        .iter()
        .position(|a| a == "--max-model-len")
        .unwrap();
    assert!(user < fixed && fixed < extra);
    assert_eq!(&cmd.argv[extra + 1..], ["--reasoning-parser", "qwen3"]);
    assert_eq!(
        cmd.env.get("MLLM_EXTRA_APPROVALS").map(String::as_str),
        p.extra_approvals.as_deref()
    );
    // An extra restating a host-fixed or typed option is a duplicate.
    let mut dup = plan();
    dup.extra_args = vec!["--max-model-len".into(), "1".into()];
    assert!(render_command(&dup).is_err());
    // Without extras there is no second marker.
    assert!(!render_command(&plan())
        .unwrap()
        .argv
        .contains(&EXTRA_ARGS_MARKER.to_string()));
}

/// SPEC §7.5 / T14: vLLM 0.29 has no `--swap-space`; a swap budget is never
/// rendered, so the parser never sees an option it would refuse.
// T14
#[test]
fn swap_space_is_never_rendered() {
    let mut p = plan();
    p.granted.swap_space_bytes = Some(4 << 30);
    let cmd = render_command(&p).unwrap();
    assert!(!cmd.argv.iter().any(|a| a == "--swap-space"));
}

/// Discrete GPU design §6 (ADR 0019): on a discrete device vLLM gets the KV
/// bytes of the grant and a `--gpu-memory-utilization` that is the device
/// request's share of the card, rounded up to 0.01, at least 0.75 (vLLM 0.29
/// with CUDA graphs does not start a 4B model on a 16 GB card below it) and at
/// most 0.99.
// T26
#[test]
fn a_discrete_launch_renders_kv_bytes_and_utilization() {
    use mllm_adapters::vllm::device_utilization_pct;
    assert_eq!(device_utilization_pct(12 << 30, 16376 << 20), 76);
    assert_eq!(device_utilization_pct(2 << 30, 16376 << 20), 75);
    assert_eq!(device_utilization_pct(20 << 30, 16376 << 20), 99);
    // A card total that was never observed cannot divide anything.
    assert_eq!(device_utilization_pct(12 << 30, 0), 99);
    let mut input = base_input();
    input.granted.gpu_utilization_pct = Some(76);
    input.granted.kv_cache_bytes = Some(4 << 30);
    let argv = render_command(&input).unwrap().argv;
    assert!(argv
        .windows(2)
        .any(|w| w == ["--kv-cache-memory-bytes", &(4i64 << 30).to_string()]));
    assert!(argv
        .windows(2)
        .any(|w| w == ["--gpu-memory-utilization", "0.76"]));
}

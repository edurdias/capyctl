//! ADR 0039 §4, §5, §7, §8: TensorFold's native line (1.x): the command, its
//! closed environment, the plan from a native profile, and the launch
//! failure's reading of the engine's start refusals. CPU tests only; they
//! prove the rendering, not that TensorFold native starts (the TF rows do).
use std::collections::{BTreeMap, BTreeSet};

use capyctl_adapters::engine_telemetry::EngineTelemetry;
use capyctl_adapters::launch_failure::{summary, EngineExit, MAX_SUMMARY_BYTES};
use capyctl_adapters::tensorfold::native::{expected_batch_size, ENGINE_ENV_ALLOWLIST};
use capyctl_adapters::tensorfold::{
    engine_environment, plan_from_effective, render_command, PlanInputTensorfold,
    TensorfoldArgsError,
};
use capyctl_config::effective::{resolve_effective, CudaNamespace};
use capyctl_config::tensorfold_native::{
    TensorfoldLine, CHECKPOINT_NOTE, DRAFTER_REFUSAL, KV_CACHE_DTYPE_REFUSAL, RESERVED_FLAGS,
};
use capyctl_testkit::tensorfold_native::{parse_serve, NativeRelease};
use serde_json::{json, Value};

const BIN: &str = "/home/u/tensorfold-1.0.0/bin/tensorfold-native";

fn native_plan() -> PlanInputTensorfold {
    PlanInputTensorfold {
        line: TensorfoldLine::Native,
        engine_bin: BIN.into(),
        model_path: "/srv/models/nemotron".into(),
        served_model_name: "nemotron".into(),
        port: 8101,
        context_length: 32768,
        parallel: Some(8),
        engine_log: Some("/var/lib/capyctl/logs/i.log".into()),
        ..PlanInputTensorfold::default()
    }
}

fn strings(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|token| (*token).to_owned()).collect()
}

/// ADR 0039 §4's fixed head, binary through `--snapshot-dir none`.
const HEAD: &[&str] = &[
    BIN,
    "serve",
    "/srv/models/nemotron",
    "--name",
    "nemotron",
    "--host",
    "127.0.0.1",
    "--port",
    "8101",
    "--no-update-check",
    "--backend",
    "cuda",
    "--snapshot-dir",
    "none",
];

fn with_head(tail: &[&str]) -> Vec<String> {
    strings(&[HEAD, tail].concat())
}

/// The rendered command is one both releases' parsers accept.
fn both_releases_accept(argv: &[String]) {
    for release in [NativeRelease::V1_0_0, NativeRelease::V1_1_5] {
        parse_serve(release, &argv[2..]).unwrap_or_else(|e| panic!("{release:?}: {e}"));
    }
}

// T41 T14 (ADR 0039 §4): the native command, whole: the fixed head, then
// `--context` and the default `--parallel 8`, nothing for drafts.
#[test]
fn the_native_command_renders_exactly_as_section_4() {
    let argv = render_command(&native_plan()).unwrap().argv;
    assert_eq!(argv, with_head(&["--context", "32768", "--parallel", "8"]));
    both_releases_accept(&argv);
}

// T41 T14 (ADR 0039 §4): a declared `max_concurrent_requests` is the
// `--parallel` count; the typed fields follow; then host and extra args.
#[test]
fn typed_fields_host_args_and_extras_render_in_order() {
    let mut input = native_plan();
    input.parallel = Some(4);
    input.max_tokens = Some(1024);
    input.thinking = Some(false);
    input.engine_args = strings(&["--segments", "2"]);
    input.extra_args = strings(&["--no-drafts", "--compact-at", "auto"]);
    let argv = render_command(&input).unwrap().argv;
    assert_eq!(
        argv,
        with_head(&[
            "--context",
            "32768",
            "--parallel",
            "4",
            "--max-tokens",
            "1024",
            "--no-thinking",
            "--segments",
            "2",
            "--no-drafts",
            "--compact-at",
            "auto",
        ])
    );
    both_releases_accept(&argv);
    input.thinking = Some(true);
    let argv = render_command(&input).unwrap().argv;
    assert!(argv.contains(&"--thinking".to_owned()), "{argv:?}");
}

// T41 (ADR 0039 §4, ADR 0023 §4 amendment): arguments carrying their own
// `--parallel` render without CapyCTL's pair; both together are refused.
#[test]
fn arguments_with_their_own_parallel_render_no_second_one() {
    let mut input = native_plan();
    input.parallel = None;
    input.extra_args = strings(&["--parallel=3"]);
    let argv = render_command(&input).unwrap().argv;
    assert_eq!(argv, with_head(&["--context", "32768", "--parallel=3"]));
    both_releases_accept(&argv);
    assert_eq!(expected_batch_size(&input), Some(3));
    input.extra_args = strings(&["--parallel", "auto"]);
    assert_eq!(expected_batch_size(&input), None, "auto is not a count");

    for (engine_args, extra_args) in [
        (strings(&["--parallel", "2"]), vec![]),
        (vec![], strings(&["--parallel=2"])),
    ] {
        let mut both = native_plan();
        both.engine_args = engine_args;
        both.extra_args = extra_args;
        assert_eq!(
            render_command(&both).unwrap_err(),
            TensorfoldArgsError::Duplicate("--parallel".into())
        );
    }
    assert_eq!(expected_batch_size(&native_plan()), Some(8));
}

// T41 (ADR 0039 §4, §6): never `--drafter`, `--kv-dtype` or `--idle-unload`:
// a KV cache type and a drafter are refused with their capability text,
// `--idle-unload` as reserved.
#[test]
fn never_a_drafter_a_kv_dtype_or_idle_unload() {
    let argv = render_command(&native_plan()).unwrap().argv;
    for never in ["--drafter", "--kv-dtype", "--idle-unload"] {
        assert!(!argv.iter().any(|a| a.starts_with(never)), "{never}");
    }
    let mut kv = native_plan();
    kv.kv_dtype = Some("int8".into());
    assert_eq!(
        render_command(&kv).unwrap_err(),
        TensorfoldArgsError::NativeRefused(KV_CACHE_DTYPE_REFUSAL.into())
    );
    for args in [strings(&["--drafter", "none"]), strings(&["--drafter=/d"])] {
        let mut drafter = native_plan();
        drafter.extra_args = args;
        assert_eq!(
            render_command(&drafter).unwrap_err(),
            TensorfoldArgsError::NativeRefused(DRAFTER_REFUSAL.into())
        );
    }
    let mut idle = native_plan();
    idle.engine_args = strings(&["--idle-unload", "0"]);
    assert_eq!(
        render_command(&idle).unwrap_err(),
        TensorfoldArgsError::Reserved("--idle-unload".into())
    );
    // The Python line is unchanged: it still renders `--drafter none`.
    let mut python = native_plan();
    python.line = TensorfoldLine::Python;
    assert!(render_command(&python)
        .unwrap()
        .argv
        .ends_with(&strings(&["--drafter", "none"])));
}

// T41 T21 (ADR 0039 §4): each reserved option is refused by its exact name,
// spelled apart and with `=value`, in host and extra arguments alike, on
// every native version; an abbreviation is not expanded (the binary
// refuses it at start).
#[test]
fn each_reserved_option_is_refused_by_exact_name_and_with_value() {
    for flag in RESERVED_FLAGS {
        for args in [strings(&[flag, "v"]), vec![format!("{flag}=v")]] {
            for host in [true, false] {
                let mut input = native_plan();
                if host {
                    input.engine_args = args.clone();
                } else {
                    input.extra_args = args.clone();
                }
                assert_eq!(
                    render_command(&input).unwrap_err(),
                    TensorfoldArgsError::Reserved((*flag).to_owned()),
                    "{args:?} host={host}"
                );
            }
        }
    }
    let mut input = native_plan();
    input.extra_args = strings(&["--hos", "0.0.0.0", "--par", "2"]);
    let argv = render_command(&input).unwrap().argv;
    assert!(argv.ends_with(&strings(&["--hos", "0.0.0.0", "--par", "2"])));
    assert!(argv.windows(2).any(|w| w == ["--parallel", "8"]));
    assert_eq!(
        parse_serve(NativeRelease::V1_0_0, &argv[2..]).unwrap_err(),
        "tensorfold serve: error: unrecognized arguments: --hos"
    );
}

// T41 (ADR 0039 §4): a typed field's spelling is refused in the extras, and
// in the host arguments when CapyCTL renders that field.
#[test]
fn typed_spellings_are_refused_by_exact_name() {
    let mut extra = native_plan();
    extra.extra_args = strings(&["--max-tokens=5"]);
    assert_eq!(
        render_command(&extra).unwrap_err(),
        TensorfoldArgsError::Duplicate("--max-tokens".into())
    );
    let mut host = native_plan();
    host.engine_args = strings(&["--no-thinking"]);
    assert!(render_command(&host).is_ok(), "thinking is not typed here");
    host.thinking = Some(true);
    assert_eq!(
        render_command(&host).unwrap_err(),
        TensorfoldArgsError::Duplicate("--no-thinking".into())
    );
}

// T41 (ADR 0039 §13): the native line serves no group.
#[test]
fn a_group_member_is_refused_on_the_native_line() {
    let mut input = native_plan();
    input.group = Some(capyctl_domain::group::GroupMemberArgs {
        tensor_parallel: 2,
        pipeline_parallel: 1,
        nnodes: 2,
        node_rank: 0,
        head_address: "10.0.0.1".parse().unwrap(),
        own_address: "10.0.0.1".parse().unwrap(),
        rendezvous_port: 29500,
        worker_port: None,
        own_interface: None,
    });
    let error = render_command(&input).unwrap_err();
    assert!(
        error.to_string().starts_with("group_unsupported"),
        "{error}"
    );
}

// T41 T21 (ADR 0039 §5, SPEC §13.3): the closed environment holds exactly
// §5's names and the resolved env's approved ones, with the reserve at 2;
// the 0.6 build and group variables and the refused names never reach it.
#[test]
fn the_closed_environment_holds_exactly_section_5s_names() {
    let mut plan = native_plan();
    plan.memory_limit_bytes = Some((30 << 30) + (512 << 20));
    plan.cuda_namespace = Some(CudaNamespace::PciIndex(1));
    plan.build_env = BTreeMap::from(
        [
            ("HF_HOME", "/srv/hf"),
            ("TENSORFOLD_REQUEST_LOG", "/tmp/requests"),
            ("TENSORFOLD_CUDA_KERNELS", "/tmp/kernels"),
            ("TORCH_EXTENSIONS_DIR", "/tmp/ext"),
            ("MAX_JOBS", "4"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned())),
    );
    let cmd = render_command(&plan).unwrap();
    let toolchain = BTreeMap::from(
        [
            ("CUDA_HOME", "/usr/local/cuda"),
            ("MAX_JOBS", "8"),
            ("FLASHINFER_NVCC_THREADS", "1"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned())),
    );
    let inherited = |name: &str| match name {
        "HOME" => Some("/home/u".to_owned()),
        "CUDA_VISIBLE_DEVICES" => Some("7".to_owned()),
        "PATH" | "LD_LIBRARY_PATH" => Some("/evil".to_owned()),
        _ => None,
    };
    let env = engine_environment(
        &cmd.env,
        &plan,
        &inherited,
        &toolchain,
        EngineTelemetry::Off,
    );
    let names: BTreeSet<&str> = env.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "CAPYCTL_ENGINE_LOG",
            "CUDA_DEVICE_ORDER",
            "CUDA_VISIBLE_DEVICES",
            "DO_NOT_TRACK",
            "HF_HOME",
            "HF_HUB_OFFLINE",
            "HOME",
            "PATH",
            "TENSORFOLD_CUDA_MEMORY_LIMIT_GB",
            "TENSORFOLD_MEMORY_RESERVE_GIB",
            "TENSORFOLD_NO_UPDATE_CHECK",
        ])
    );
    assert!(names
        .iter()
        .all(|name| ENGINE_ENV_ALLOWLIST.contains(name) || *name == "HF_HOME"));
    assert_eq!(env["TENSORFOLD_MEMORY_RESERVE_GIB"], "2");
    assert_eq!(env["TENSORFOLD_CUDA_MEMORY_LIMIT_GB"], "30.5");
    assert_eq!(env["TENSORFOLD_NO_UPDATE_CHECK"], "1");
    assert_eq!(env["HF_HUB_OFFLINE"], "1");
    assert_eq!(env["PATH"], capyctl_adapters::engine_env::SYSTEM_PATH);
    assert_eq!(
        env["CUDA_VISIBLE_DEVICES"], "1",
        "the namespace pins the GPU"
    );
    assert_eq!(env["CUDA_DEVICE_ORDER"], "PCI_BUS_ID");
    assert_eq!(env["HOME"], "/home/u");
    assert_eq!(env["DO_NOT_TRACK"], "1");
    let allowed = engine_environment(
        &cmd.env,
        &plan,
        &inherited,
        &toolchain,
        EngineTelemetry::Allow,
    );
    assert!(!allowed.contains_key("DO_NOT_TRACK"));
    assert_eq!(allowed["TENSORFOLD_NO_UPDATE_CHECK"], "1");
}

fn fixture() -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .expect("fixture JSON");
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "tensorfold".into();
    profile["line"] = "native".into();
    profile["executable"] = BIN.into();
    profile["build_fingerprint"] = "1.0.0".into();
    profile["args"] = json!([]);
    profile["security"]["deep_park"] = "disabled".into();
    deployment["residency"] = "restart_only".into();
    deployment["engine_config"] = json!({"context_length": 8192});
    (deployment, host)
}

// T41 (ADR 0039 §2, §4): a native profile's plan: the line, no extensions
// directory, no installation `bin` on the PATH, `--parallel` from the
// declared count else 8, else the extras' own, detected by exact name.
#[test]
fn the_native_plan_comes_from_the_resolved_deployment() {
    let plan_of = |deployment: &Value, host: &Value| {
        let effective = resolve_effective(deployment, host).unwrap();
        plan_from_effective(
            &effective,
            8101,
            "/var/log/i.log".into(),
            Some("/var/lib/capyctl/torch_extensions".into()),
        )
        .unwrap()
    };
    let (deployment, host) = fixture();
    let plan = plan_of(&deployment, &host);
    assert_eq!(plan.line, TensorfoldLine::Native);
    assert_eq!(plan.extensions_dir, None);
    assert_eq!(plan.engine_path_extra, None);
    assert_eq!(plan.cuda_home, None);
    assert_eq!(plan.parallel, Some(8));
    let argv = render_command(&plan).unwrap().argv;
    assert!(argv.windows(2).any(|w| w == ["--parallel", "8"]));
    assert!(!argv.iter().any(|a| a == "--drafter"));

    let (mut deployment, host) = fixture();
    deployment["engine_config"]["max_concurrent_requests"] = 4.into();
    assert_eq!(plan_of(&deployment, &host).parallel, Some(4));

    let (mut deployment, host) = fixture();
    deployment["engine_config"]["accept_extra_args"] = true.into();
    deployment["engine_config"]["extra_args"] = json!(["--parallel", "3"]);
    let plan = plan_of(&deployment, &host);
    assert_eq!(plan.parallel, None);
    let argv = render_command(&plan).unwrap().argv;
    assert_eq!(
        argv.iter().filter(|a| a.starts_with("--parallel")).count(),
        1
    );
    assert_eq!(expected_batch_size(&plan), Some(3));
}

// T41 T29 (ADR 0039 §4): the 1.0 and 1.1.5 refusals of a flag the binary
// does not serve, as the engine prints them, name the flag (never the
// value) and say this native release does not serve it.
#[test]
fn launch_failure_maps_the_unserved_flag_messages() {
    for release in [NativeRelease::V1_0_0, NativeRelease::V1_1_5] {
        let refused = parse_serve(release, &strings(&["/m", "--kv-dtype", "int8"])).unwrap_err();
        let output = format!("usage: tensorfold serve [-h] [--host HOST] model\n{refused}\n");
        let text = summary(&output, Some(EngineExit::Code(2)));
        assert_eq!(
            text,
            "the engine exited before readiness with exit code 2; this TensorFold native \
             release does not serve --kv-dtype; a TensorFold 0.6 installation may",
            "{release:?}"
        );
        let refused =
            parse_serve(release, &strings(&["/m", "--snapshot-dir", "/secret/dir"])).unwrap_err();
        let text = summary(&refused, Some(EngineExit::Code(2)));
        assert!(text.contains("does not serve --snapshot-dir;"), "{text}");
        assert!(!text.contains("secret"), "{text}");
        assert!(
            !text.contains("--engine python") && !text.contains("pip"),
            "{text}"
        );
        assert!(text.len() <= MAX_SUMMARY_BYTES);
    }
}

// T41 T29 (ADR 0039 §8): the native memory refusals (1.0.0
// `native/cuda.zig:339`, `:352-353`) name the settings that fit, never a
// figure from the engine's line.
#[test]
fn launch_failure_maps_the_memory_messages() {
    let streams = [
        "tensorfold serve: error: --parallel 8 needs 7.53 GiB for its streams at a 131072-token \
         window, and the CUDA memory budget leaves 3.10 GiB: serve --parallel 3, or lower --context",
        "tensorfold serve: error: the CUDA memory budget fits no stream: one at a 262144-token \
         window takes 1.82 GiB, and 0.40 GiB is left after the model's 17.25 GiB and the 2.0 GiB \
         reserve; lower --context, or free device memory",
    ];
    for line in streams {
        let text = summary(line, Some(EngineExit::Code(1)));
        assert_eq!(
            text,
            "the engine exited before readiness with exit code 1; TensorFold native's memory \
             budget cannot hold its streams: lower max_concurrent_requests or context_length, \
             or raise resources"
        );
    }
    let weights = "tensorfold serve: error: the checkpoint's 17.3 GiB of weights do not fit the \
                   12.0 GiB the CUDA memory budget grants (14.0 GiB free less a 2.0 GiB reserve); \
                   free device memory or adjust TENSORFOLD_MEMORY_RESERVE_GIB / \
                   TENSORFOLD_CUDA_MEMORY_LIMIT_GB";
    let text = summary(weights, Some(EngineExit::Code(1)));
    assert_eq!(
        text,
        "the engine exited before readiness with exit code 1; TensorFold native's memory budget \
         cannot hold the checkpoint's weights: raise resources or free device memory"
    );
    assert!(!text.contains("17.3"));
}

// T41 T29 (ADR 0039 §7): an unlisted family's refusal carries the
// checkpoint note and names the family and the 0.6 line, within the bound.
#[test]
fn launch_failure_maps_the_unlisted_family_message() {
    let line = "tensorfold serve: error: the native CUDA engine has no backend for qwen3_5 \
                checkpoints yet";
    let text = summary(line, Some(EngineExit::Code(1)));
    assert_eq!(
        text,
        format!(
            "the engine exited before readiness with exit code 1; {CHECKPOINT_NOTE}; family \
             qwen3_5 is not served by this native release; TensorFold 0.6 may serve it"
        )
    );
    assert!(text.len() <= MAX_SUMMARY_BYTES);
    let long = format!(
        "the native CUDA engine has no backend for {} checkpoints yet",
        "a".repeat(32)
    );
    let text = summary(&long, Some(EngineExit::Code(i32::MIN)));
    assert!(text.len() <= MAX_SUMMARY_BYTES, "{text}");
    assert!(text.contains(CHECKPOINT_NOTE) && text.contains("TensorFold 0.6"));
    let odd = "the native CUDA engine has no backend for /etc/passwd checkpoints yet";
    let text = summary(odd, None);
    assert!(
        !text.contains("passwd") && text.contains(CHECKPOINT_NOTE),
        "{text}"
    );
}

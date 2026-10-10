//! ADR 0029 §4–§6, §9: the llama-server command and its closed environment.
//! CPU tests only; they prove the rendering, not that llama-server starts,
//! and they do not qualify llama.cpp (only the live rows LC1–LC6 do).
use capyctl_adapters::llamacpp::{
    engine_environment, plan_from_effective, recheck_rendered, render_command, LlamacppArgsError,
    LlamacppDirs, LlamacppPlanError, PlanInputLlamacpp,
};
use capyctl_config::effective::resolve_effective;
use capyctl_domain::launch::LlamacppGpuLayers;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn plan() -> PlanInputLlamacpp {
    PlanInputLlamacpp {
        engine_bin: "/opt/llama.cpp/bin/llama-server".into(),
        engine_path_extra: Some("/opt/llama.cpp/bin".into()),
        model_file: "/srv/models/qwen/qwen-Q4_K_M.gguf".into(),
        served_model_name: "qwen".into(),
        port: 8101,
        context_length: 15000,
        slots: 4,
        n_gpu_layers: LlamacppGpuLayers::All,
        cache_type: "f16".into(),
        config_dir: "/var/lib/capyctl/engines/llamacpp/config".into(),
        cache_dir: "/var/lib/capyctl/engines/llamacpp/cache".into(),
        engine_log: Some("/var/lib/capyctl/logs/i.log".into()),
        ..PlanInputLlamacpp::default()
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// The reserved block CapyCTL renders, in order, for [`plan`].
const BLOCK: &[&str] = &[
    "/opt/llama.cpp/bin/llama-server",
    "--host",
    "127.0.0.1",
    "--port",
    "8101",
    "--model",
    "/srv/models/qwen/qwen-Q4_K_M.gguf",
    "--alias",
    "qwen",
    "--ctx-size",
    "60416",
    "--parallel",
    "4",
    "--no-kv-unified",
    "--gpu-layers",
    "all",
    "--cache-type-k",
    "f16",
    "--cache-type-v",
    "f16",
    "--fit",
    "off",
    "--cache-ram",
    "0",
    "--no-context-shift",
    "--metrics",
    "--slots",
    "--offline",
    "--no-webui",
    "--cors-origins",
    "localhost",
    "--no-cors-credentials",
];

// T42 T14 T37 (ADR 0029 §4, §5): `context_length: 15000` with 4 slots renders
// `--ctx-size 60416 --parallel 4` (15104 × 4, each slot padded to 256), and
// the reserved block is complete and in order, CORS and the web UI closed.
#[test]
fn the_reserved_block_is_complete_and_in_order() {
    let rendered = render_command(&plan()).unwrap();
    assert_eq!(rendered.argv, strings(BLOCK));
    let mut input = plan();
    input.n_gpu_layers = LlamacppGpuLayers::Count(20);
    input.cache_type = "q8_0".into();
    input.slots = 1;
    input.context_length = 256;
    input.mmproj_file = Some("/srv/models/qwen/mmproj-F16.gguf".into());
    input.engine_args = strings(&["--threads", "8"]);
    input.extra_args = strings(&["--cache-reuse", "256", "--spec-type", "ngram-mod"]);
    let argv = render_command(&input).unwrap().argv;
    let at = |option: &str| argv.iter().position(|token| token == option).unwrap();
    assert_eq!(argv[at("--ctx-size") + 1], "256");
    assert_eq!(argv[at("--parallel") + 1], "1");
    assert_eq!(argv[at("--gpu-layers") + 1], "20");
    assert_eq!(argv[at("--cache-type-k") + 1], "q8_0");
    assert_eq!(argv[at("--cache-type-v") + 1], "q8_0");
    assert_eq!(argv[at("--mmproj") + 1], "/srv/models/qwen/mmproj-F16.gguf");
    // The host-fixed arguments, then the deployment's, after CapyCTL's.
    assert!(at("--mmproj") < at("--threads") && at("--threads") < at("--cache-reuse"));
    assert_eq!(
        argv[argv.len() - 6..],
        strings(&[
            "--threads",
            "8",
            "--cache-reuse",
            "256",
            "--spec-type",
            "ngram-mod"
        ])
    );
}

// T42 T14 (ADR 0029 §6): a reserved option among the pass-through arguments
// is refused at render in every spelling, and so is `--name=value`.
#[test]
fn reserved_pass_through_options_are_refused_at_render() {
    for (fixed, extra) in [
        (&[][..], &["--n_gpu_layers", "20"][..]),
        (&["--ctx-size", "4096"], &[]),
        (&[], &["--no-webui"]),
        (&[], &["--api-key", "k"]),
    ] {
        let mut input = plan();
        input.engine_args = strings(fixed);
        input.extra_args = strings(extra);
        assert!(
            matches!(render_command(&input), Err(LlamacppArgsError::Reserved(_))),
            "{fixed:?} {extra:?}"
        );
    }
    let mut input = plan();
    input.extra_args = strings(&["--cache-reuse=256"]);
    assert!(matches!(
        render_command(&input),
        Err(LlamacppArgsError::Malformed(_))
    ));
    input.extra_args = strings(&["--temp", "1", "--temp", "2"]);
    assert!(matches!(
        render_command(&input),
        Err(LlamacppArgsError::Duplicate(_))
    ));
    for (context_length, slots) in [(0, 4), (8192, 0), (1_000_000_000, 4)] {
        let mut input = plan();
        input.context_length = context_length;
        input.slots = slots;
        assert!(matches!(
            render_command(&input),
            Err(LlamacppArgsError::NoContext)
        ));
    }
    let mut input = plan();
    input.cache_type = "fp8".into();
    assert!(matches!(
        render_command(&input),
        Err(LlamacppArgsError::CacheType(_))
    ));
}

// T42 T14 (ADR 0029 §6, SPEC §8.2): the rendered-argument recheck refuses a
// reserved option that reached the final vector, a rendered one twice or
// not at all, and a `--name=value` spelling.
#[test]
fn the_recheck_refuses_a_reserved_option_in_the_final_vector() {
    let block = strings(BLOCK);
    recheck_rendered(&block).unwrap();
    for (tail, refused) in [
        (&["--ctx-size", "4096"][..], "--ctx-size"),
        (&["--ctx_size", "4096"], "--ctx-size"),
        (&["--n-gpu-layers", "10"], "--gpu-layers"),
        (&["--webui"], "--ui"),
        (&["--api-key", "k"], "--api-key"),
        (&["--tools", "all"], "--tools"),
        (&["--mmproj", "/a.gguf", "--mmproj", "/b.gguf"], "--mmproj"),
        (&["--port=1"], "--port=1"),
    ] {
        let mut argv = block.clone();
        argv.extend(strings(tail));
        assert_eq!(
            recheck_rendered(&argv),
            Err(LlamacppArgsError::RenderedReserved(refused.into())),
            "{tail:?}"
        );
    }
    let without: Vec<String> = block
        .iter()
        .filter(|token| *token != "--metrics")
        .cloned()
        .collect();
    assert_eq!(
        recheck_rendered(&without),
        Err(LlamacppArgsError::RenderedReserved("--metrics".into()))
    );
}

// T42 T37 (ADR 0029 §6, SPEC §13.3): the closed environment holds CapyCTL's
// XDG_CONFIG_HOME and LLAMA_CACHE, the closed PATH and the GPU pin, and no
// LLAMA_* variable, HOME or XDG_CONFIG_HOME from the caller.
#[test]
fn the_environment_is_closed() {
    let mut input = plan();
    input.cuda_namespace = Some(capyctl_config::effective::CudaNamespace::Uuid(
        "GPU-1".into(),
    ));
    input.build_env = BTreeMap::from([
        ("RUST_LOG".to_owned(), "info".to_owned()),
        ("GGML_CUDA_NO_PINNED".to_owned(), "1".to_owned()),
        ("LLAMA_ARG_CTX_SIZE".to_owned(), "4096".to_owned()),
        ("XDG_CONFIG_HOME".to_owned(), "/home/u/.config".to_owned()),
        ("LLAMA_CACHE".to_owned(), "/tmp/c".to_owned()),
    ]);
    let rendered = render_command(&input).unwrap().env;
    let inherited = |name: &str| match name {
        "HOME" => Some("/home/u".to_owned()),
        "LLAMA_API_KEY" => Some("k".to_owned()),
        "CUDA_VISIBLE_DEVICES" => Some("GPU-0".to_owned()),
        _ => None,
    };
    let env = engine_environment(&rendered, &input, &inherited);
    assert_eq!(
        env.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "CAPYCTL_ENGINE_LOG",
            "CUDA_VISIBLE_DEVICES",
            "GGML_CUDA_NO_PINNED",
            "LLAMA_CACHE",
            "PATH",
            "RUST_LOG",
            "XDG_CONFIG_HOME",
        ]
    );
    assert_eq!(
        env["XDG_CONFIG_HOME"],
        "/var/lib/capyctl/engines/llamacpp/config"
    );
    assert_eq!(
        env["LLAMA_CACHE"],
        "/var/lib/capyctl/engines/llamacpp/cache"
    );
    assert_eq!(env["CUDA_VISIBLE_DEVICES"], "GPU-1", "the pin wins");
    assert!(
        env["PATH"].starts_with("/opt/llama.cpp/bin:"),
        "{}",
        env["PATH"]
    );
    assert_eq!(env["CAPYCTL_ENGINE_LOG"], "/var/lib/capyctl/logs/i.log");
}

// T42 T37 (ADR 0029 §6, SPEC §8.2): the final filter drops the variables that
// replace the leased port and routes, the projector device, router mode or the
// device choice, even when the resolved environment carries them.
#[test]
fn the_environment_drops_listener_and_device_overrides() {
    let mut input = plan();
    let overrides = [
        "AIP_MODE",
        "AIP_HTTP_PORT",
        "AIP_HEALTH_ROUTE",
        "AIP_PREDICT_ROUTE",
        "MTMD_BACKEND_DEVICE",
        "HF_TOKEN",
        "LLAMA_SERVER_ROUTER_PORT",
        "LLAMA_SERVER_CHILD_MODE",
        "GGML_CUDA_DEVICES",
        "GGML_CUDA_ENABLE_UNIFIED_MEMORY",
        "GGML_BACKEND_PATH",
    ];
    input.build_env = overrides
        .iter()
        .map(|name| ((*name).to_owned(), "1".to_owned()))
        .chain([("GGML_CUDA_NO_PINNED".to_owned(), "1".to_owned())])
        .collect();
    let rendered = render_command(&input).unwrap().env;
    let env = engine_environment(&rendered, &input, &|_| None);
    for name in overrides {
        assert!(!env.contains_key(name), "{name} reached the engine");
    }
    assert_eq!(env["GGML_CUDA_NO_PINNED"], "1");
}

fn fixture(checkpoint: &Path) -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "llamacpp".into();
    profile["executable"] = "/opt/llama.cpp/bin/llama-server".into();
    profile["build_fingerprint"] = "0.6.0+d812350".into();
    profile["args"] = json!(["--threads", "8"]);
    profile["security"]["deep_park"] = "disabled".into();
    deployment["residency"] = "restart_only".into();
    deployment["model"]["path"] = json!(checkpoint);
    deployment["engine_config"] = json!({"context_length": 15000});
    (deployment, host)
}

fn touch(root: &Path, files: &[&str]) {
    for file in files {
        std::fs::write(root.join(file), b"GGUF").unwrap();
    }
}

fn dirs() -> LlamacppDirs {
    LlamacppDirs {
        config: "/var/lib/capyctl/engines/llamacpp/config".into(),
        cache: "/var/lib/capyctl/engines/llamacpp/cache".into(),
    }
}

// T42 (ADR 0029 §5, §9): the shared builder renders the checkpoint's one
// GGUF, the projector the deployment names, CapyCTL's defaults and the
// installation's own arguments.
#[test]
fn the_builder_renders_the_checkpoint_gguf() {
    let checkpoint = tempfile::tempdir().unwrap();
    touch(checkpoint.path(), &["qwen-Q4_K_M.gguf", "mmproj-F16.gguf"]);
    let (mut deployment, host) = fixture(checkpoint.path());
    let effective = resolve_effective(&deployment, &host).unwrap();
    let plan = plan_from_effective(&effective, 8101, "/l/i.log".into(), &dirs()).unwrap();
    assert_eq!(
        PathBuf::from(&plan.model_file),
        checkpoint.path().join("qwen-Q4_K_M.gguf")
    );
    assert_eq!(plan.mmproj_file, None);
    assert_eq!((plan.context_length, plan.slots), (15000, 4));
    assert_eq!(plan.cache_type, "f16");
    assert_eq!(plan.n_gpu_layers, LlamacppGpuLayers::All);
    assert_eq!(plan.served_model_name, "toy");
    assert_eq!(plan.engine_args, strings(&["--threads", "8"]));
    let argv = render_command(&plan).unwrap().argv;
    assert!(argv.windows(2).any(|w| w == ["--ctx-size", "60416"]));
    deployment["engine_config"]["llamacpp"] = json!({"mmproj_file": "mmproj-F16.gguf"});
    let effective = resolve_effective(&deployment, &host).unwrap();
    let plan = plan_from_effective(&effective, 8101, "/l/i.log".into(), &dirs()).unwrap();
    assert_eq!(
        plan.mmproj_file.map(PathBuf::from),
        Some(checkpoint.path().join("mmproj-F16.gguf"))
    );
}

// T42 (ADR 0029 §9, review focus 4): two quantizations without `gguf_file`
// are refused naming both; with it, the one named renders.
#[test]
fn several_quantizations_need_gguf_file_at_launch() {
    let checkpoint = tempfile::tempdir().unwrap();
    touch(checkpoint.path(), &["m-Q4_K_M.gguf", "m-Q8_0.gguf"]);
    let (mut deployment, host) = fixture(checkpoint.path());
    let effective = resolve_effective(&deployment, &host).unwrap();
    let error = plan_from_effective(&effective, 8101, "/l/i.log".into(), &dirs()).unwrap_err();
    assert!(matches!(error, LlamacppPlanError::Checkpoint(_)), "{error}");
    let text = error.to_string();
    assert!(
        text.contains("m-Q4_K_M.gguf") && text.contains("m-Q8_0.gguf"),
        "{text}"
    );
    deployment["engine_config"]["llamacpp"] = json!({"gguf_file": "m-Q8_0.gguf"});
    let effective = resolve_effective(&deployment, &host).unwrap();
    let plan = plan_from_effective(&effective, 8101, "/l/i.log".into(), &dirs()).unwrap();
    assert!(
        plan.model_file.ends_with("/m-Q8_0.gguf"),
        "{}",
        plan.model_file
    );
}

// T42 T37 (ADR 0014 §8, ADR 0029 §8): a draft model inside the approved paths
// lexically but outside them through a symlink is refused before launch.
#[test]
fn a_draft_model_outside_the_approved_paths_is_refused_at_launch() {
    let checkpoint = tempfile::tempdir().unwrap();
    touch(checkpoint.path(), &["m.gguf"]);
    let approved = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    touch(outside.path(), &["d.gguf"]);
    touch(approved.path(), &["inside.gguf"]);
    std::os::unix::fs::symlink(
        outside.path().join("d.gguf"),
        approved.path().join("d.gguf"),
    )
    .unwrap();
    let (mut deployment, mut host) = fixture(checkpoint.path());
    let security = &mut host["runtime_profiles"]["local"]["security"];
    security["approved_options"] = json!(["--model-draft"]);
    security["approved_paths"] = json!([approved.path()]);
    for (draft, admitted) in [("inside.gguf", true), ("d.gguf", false)] {
        deployment["engine_config"] = json!({"context_length": 8192, "accept_extra_args": true,
            "extra_args": ["--model-draft", approved.path().join(draft)]});
        let effective = resolve_effective(&deployment, &host).unwrap();
        let plan = plan_from_effective(&effective, 8101, "/l/i.log".into(), &dirs());
        assert_eq!(plan.is_ok(), admitted, "{draft}: {plan:?}");
        if !admitted {
            assert!(matches!(
                plan,
                Err(LlamacppPlanError::PathNotApproved(name)) if name == "--model-draft"
            ));
        }
    }
}

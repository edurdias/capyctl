//! ADR 0038 §3, §4, §10, §14: the `trtllm-serve` command, the configuration
//! file CapyCTL writes, the launch directory and the closed environment.
//! CPU tests only; they prove the rendering, not that TensorRT-LLM starts,
//! and they do not qualify TensorRT-LLM (only the live rows TR1–TR11,
//! TR1a–TR11a and TR1b–TR11b do).
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use capyctl_adapters::engine_telemetry::EngineTelemetry;
use capyctl_adapters::tensorrt_llm::{
    engine_environment, plan_from_effective, prepare_launch_dir, recheck_rendered, render_command,
    render_llm_api_yaml, PlanInputTensorrtLlm, TensorrtLlmArgsError, TensorrtLlmPlanError,
};
use capyctl_config::effective::{resolve_effective_with_checkpoint, CheckpointFacts};
use capyctl_config::tensorrt_llm::{
    PROCESS_ENV, RESERVED_NEVER_RENDERED, RESERVED_RENDERED, TYPED_OPTIONS,
};
use serde_json::{json, Value};

const LAUNCH_DIR: &str = "/var/lib/capyctl/runtime/tensorrt-llm/i-1";
const BIN: &str = "/home/u/tensorrt-llm-1.2.1-venv/bin/trtllm-serve";

fn plan(version: &str) -> PlanInputTensorrtLlm {
    PlanInputTensorrtLlm {
        engine_bin: BIN.into(),
        engine_path_extra: Some("/home/u/tensorrt-llm-1.2.1-venv/bin".into()),
        cuda_home: Some("/usr/local/cuda-13.1".into()),
        version: version.into(),
        served_model_name: "qwen3-4b".into(),
        checkpoint_dir: "/srv/models/qwen3-4b".into(),
        port: 8101,
        max_seq_len: 16384,
        max_batch_size: 32,
        max_tokens: 29127,
        tokens_per_block: 32,
        launch_dir: LAUNCH_DIR.into(),
        engine_log: Some("/var/lib/capyctl/logs/i-1.log".into()),
        ..PlanInputTensorrtLlm::default()
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// ADR 0038 §3: the command for [`plan`], every token, in order.
const COMMAND: &[&str] = &[
    BIN,
    "serve",
    "/var/lib/capyctl/runtime/tensorrt-llm/i-1/model/qwen3-4b",
    "--host",
    "127.0.0.1",
    "--port",
    "8101",
    "--backend",
    "pytorch",
    "--max_seq_len",
    "16384",
    "--max_batch_size",
    "32",
    "--tp_size",
    "1",
    "--pp_size",
    "1",
    "--gpus_per_node",
    "1",
    "--kv_cache_free_gpu_memory_fraction",
    "0.9",
    "--log_level",
    "info",
    "--config",
    "/var/lib/capyctl/runtime/tensorrt-llm/i-1/llm_api_options.yaml",
];

/// ADR 0038 §3: the file for [`plan`] on the 1.2.1 line, which still has
/// `enable_sleep` and renders it `false`.
const YAML_1_2: &str = "dtype: \"auto\"\n\
    enable_iter_perf_stats: true\n\
    enable_sleep: false\n\
    iter_stats_max_iterations: 1000\n\
    kv_cache_config:\n  \
      dtype: \"auto\"\n  \
      free_gpu_memory_fraction: 0.9\n  \
      max_tokens: 29127\n\
    return_perf_metrics: false\n";

/// The same on the 1.3 and 1.4 lines, whose strict model no longer has
/// `enable_sleep` (their sleep is the reserved `sleep_config`).
const YAML_1_3_AND_1_4: &str = "dtype: \"auto\"\n\
    enable_iter_perf_stats: true\n\
    iter_stats_max_iterations: 1000\n\
    kv_cache_config:\n  \
      dtype: \"auto\"\n  \
      free_gpu_memory_fraction: 0.9\n  \
      max_tokens: 29127\n\
    return_perf_metrics: false\n";

// T49 T14 (ADR 0038 §3, §2a): a minimal deployment's command and file, byte
// for byte, on each qualified line (1.2.1, the 1.3 pre-release and 1.4.0rc1).
#[test]
fn the_command_and_the_file_per_line() {
    for (version, yaml) in [
        ("1.2.1", YAML_1_2),
        ("1.3.0rc29", YAML_1_3_AND_1_4),
        ("1.4.0rc1", YAML_1_3_AND_1_4),
    ] {
        let rendered = render_command(&plan(version)).unwrap();
        assert_eq!(rendered.argv, strings(COMMAND), "{version}");
        assert_eq!(
            rendered.env,
            BTreeMap::from([("TMPDIR".to_owned(), format!("{LAUNCH_DIR}/tmp"))]),
            "{version}"
        );
        assert_eq!(
            render_llm_api_yaml(&plan(version)).unwrap(),
            yaml,
            "{version}"
        );
    }
    // The typed fields, the parsers and the host-fixed and extra arguments,
    // in that order after CapyCTL's block.
    let mut input = plan("1.2.1");
    input.reasoning_parser = Some("qwen3".into());
    input.tool_parser = Some("qwen3".into());
    input.trust_remote_code = true;
    input.engine_args = strings(&["--max_num_tokens", "8192"]);
    input.extra_args = strings(&["--enable_chunked_prefill"]);
    input.dtype = Some("bfloat16".into());
    input.kv_cache_dtype = Some("fp8".into());
    input.cuda_graphs = Some(false);
    let argv = render_command(&input).unwrap().argv;
    assert_eq!(argv[..COMMAND.len()], strings(COMMAND));
    assert_eq!(
        argv[COMMAND.len()..],
        strings(&[
            "--reasoning_parser",
            "qwen3",
            "--tool_parser",
            "qwen3",
            "--trust_remote_code",
            "--max_num_tokens",
            "8192",
            "--enable_chunked_prefill",
        ])
    );
    let yaml = render_llm_api_yaml(&input).unwrap();
    assert!(yaml.contains("cuda_graph_config: null\n"), "{yaml}");
    assert!(
        yaml.starts_with("cuda_graph_config: null\ndtype: \"bfloat16\"\n"),
        "{yaml}"
    );
    assert!(yaml.contains("  dtype: \"fp8\"\n"), "{yaml}");
}

// T49 T14 (ADR 0038 §2a, §8): 1.4's `--enable_sleep_mode` is reserved and
// never rendered: refused as an extra or host-fixed argument on every line.
#[test]
fn enable_sleep_mode_is_refused() {
    for version in ["1.2.1", "1.3.0rc29", "1.4.0rc1"] {
        let mut input = plan(version);
        input.extra_args = strings(&["--enable_sleep_mode"]);
        assert_eq!(
            render_command(&input).unwrap_err(),
            TensorrtLlmArgsError::Reserved("--enable-sleep-mode".into()),
            "{version}"
        );
        let mut input = plan(version);
        input.engine_args = strings(&["--enable_sleep_mode=true"]);
        assert!(render_command(&input).is_err(), "{version}");
    }
}

// T49 T14 (ADR 0038 §3, ADR 0014 §6): every reserved option, in every alias,
// with `_` or `-`, as `--name value` and as `--name=value`, is refused at
// render, as an extra and as a host-fixed argument; a typed one names its
// field.
#[test]
fn every_reserved_option_alias_and_spelling_is_refused() {
    for row in RESERVED_RENDERED.iter().chain(RESERVED_NEVER_RENDERED) {
        for spelling in *row {
            let dashed = spelling.replace('_', "-");
            let equals = format!("{spelling}=1");
            for tokens in [
                vec![(*spelling).to_owned(), "1".to_owned()],
                vec![dashed.clone(), "1".to_owned()],
                vec![equals.clone()],
            ] {
                for host_fixed in [false, true] {
                    let mut input = plan("1.2.1");
                    if host_fixed {
                        input.engine_args = tokens.clone();
                    } else {
                        input.extra_args = tokens.clone();
                    }
                    let error = render_command(&input).unwrap_err();
                    assert!(
                        matches!(
                            error,
                            TensorrtLlmArgsError::Reserved(_)
                                | TensorrtLlmArgsError::ReservedField { .. }
                        ),
                        "{tokens:?} (host-fixed {host_fixed}): {error:?}"
                    );
                }
            }
        }
    }
    for (option, field) in TYPED_OPTIONS {
        let mut input = plan("1.2.1");
        input.extra_args = strings(&[option, "1"]);
        match render_command(&input).unwrap_err() {
            TensorrtLlmArgsError::ReservedField { field: named, .. } => {
                assert_eq!(named, *field, "{option}")
            }
            other => panic!("{option}: {other:?}"),
        }
    }
    // A positional token (a model, or a subcommand) and a duplicate.
    let mut input = plan("1.2.1");
    input.extra_args = strings(&["disaggregated"]);
    assert!(matches!(
        render_command(&input),
        Err(TensorrtLlmArgsError::Malformed(_))
    ));
    input.extra_args = strings(&["--max_num_tokens", "1", "--max-num-tokens", "2"]);
    assert!(matches!(
        render_command(&input),
        Err(TensorrtLlmArgsError::Duplicate(_))
    ));
}

// T49 T14 (ADR 0038 §3, SPEC §8.2): the rendered-argument recheck refuses a
// reserved option that reached the final vector, one rendered twice or not
// at all, a reserved `--name=value`, a listener off loopback, a parallel
// size other than 1, and a command without `serve`.
#[test]
fn the_recheck_refuses_a_reserved_option_in_the_final_vector() {
    let command = strings(COMMAND);
    recheck_rendered(&command).unwrap();
    for (tail, refused) in [
        (&["--cp_size", "2"][..], "--cp-size"),
        (&["--tp_size", "2"], "--tp-size"),
        (&["--tensor_parallel_size", "1"], "--tensor_parallel_size"),
        (&["--extra_llm_api_options", "/x.yaml"], "--config"),
        (&["--port=1"], "--port=1"),
        (&["--enable_sleep_mode"], "--enable-sleep-mode"),
        (
            &["--tool_parser", "a", "--tool_parser", "b"],
            "--tool_parser",
        ),
    ] {
        let mut argv = command.clone();
        argv.extend(strings(tail));
        assert_eq!(
            recheck_rendered(&argv),
            Err(TensorrtLlmArgsError::RenderedReserved(refused.into())),
            "{tail:?}"
        );
    }
    let mut exposed = command.clone();
    exposed[4] = "0.0.0.0".into();
    assert_eq!(
        recheck_rendered(&exposed),
        Err(TensorrtLlmArgsError::RenderedReserved("--host".into()))
    );
    let without: Vec<String> = command
        .iter()
        .filter(|token| !token.ends_with(".yaml") && *token != "--config")
        .cloned()
        .collect();
    assert_eq!(
        recheck_rendered(&without),
        Err(TensorrtLlmArgsError::RenderedReserved("--config".into()))
    );
    let mut no_serve = command.clone();
    no_serve.remove(1);
    assert_eq!(
        recheck_rendered(&no_serve),
        Err(TensorrtLlmArgsError::RenderedReserved("serve".into()))
    );
}

/// ADR 0038 §4: Qwen3-4B's `config.json`.
fn qwen3_4b() -> Value {
    json!({
        "architectures": ["Qwen3ForCausalLM"], "model_type": "qwen3",
        "num_hidden_layers": 36, "num_attention_heads": 32, "num_key_value_heads": 8,
        "head_dim": 128, "hidden_size": 2560, "torch_dtype": "bfloat16",
        "max_position_embeddings": 40960, "sliding_window": null,
        "use_sliding_window": false, "max_window_layers": 36, "vocab_size": 151936
    })
}

fn checkpoint(config: &Value) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("config.json"), config.to_string()).unwrap();
    std::fs::write(
        root.path().join("tokenizer_config.json"),
        json!({"chat_template": "{% if tools %}<tool_call>{% endif %}<think>\n"}).to_string(),
    )
    .unwrap();
    root
}

fn resolved(checkpoint: &Path) -> capyctl_config::effective::EffectiveDeployment {
    let all: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "tensorrt-llm".into();
    profile["executable"] = BIN.into();
    profile["build_fingerprint"] = "1.2.1".into();
    profile["args"] = json!([]);
    profile["env"] = json!({});
    profile["security"]["deep_park"] = "disabled".into();
    deployment["residency"] = "restart_only".into();
    deployment.as_object_mut().unwrap().remove("resources");
    deployment["model"]["path"] = json!(checkpoint);
    deployment["engine_config"] = json!({"memory": {"kv_cache": "4GiB"}});
    let facts = CheckpointFacts {
        weights_bytes: Some(8 << 30),
        ..CheckpointFacts::default()
    };
    resolve_effective_with_checkpoint(&deployment, &host, facts).unwrap()
}

// T49 T14 (ADR 0038 §4, §14): the shared builder renders `max_tokens` from
// the grant and the checkpoint's geometry (Qwen3-4B in BF16 with 4 GiB:
// 29,127 tokens), 32 slots by default, a window that fits the pool, the
// profile's version and Qwen3's parsers.
#[test]
fn the_builder_renders_max_tokens_from_the_grant() {
    let root = checkpoint(&qwen3_4b());
    let effective = resolved(root.path());
    let plan =
        plan_from_effective(&effective, 8101, "/l/i.log".into(), Path::new(LAUNCH_DIR)).unwrap();
    assert_eq!(plan.max_tokens, 29_127);
    assert_eq!(plan.max_batch_size, 32);
    assert_eq!(plan.tokens_per_block, 32);
    assert!(plan.max_seq_len > 0 && u64::from(plan.max_seq_len) <= plan.max_tokens);
    assert_eq!(plan.version, "1.2.1");
    assert_eq!(plan.served_model_name, "toy");
    assert_eq!(Path::new(&plan.checkpoint_dir), root.path());
    assert_eq!(plan.reasoning_parser.as_deref(), Some("qwen3"));
    assert_eq!(plan.tool_parser.as_deref(), Some("qwen3"));
    let argv = render_command(&plan).unwrap().argv;
    assert_eq!(argv[2], format!("{LAUNCH_DIR}/model/toy"));
    assert!(argv
        .windows(2)
        .any(|w| w == ["--reasoning_parser", "qwen3"]));
    assert!(render_llm_api_yaml(&plan)
        .unwrap()
        .contains("  max_tokens: 29127\n"));
}

// T49 (ADR 0038 §4): a geometry CapyCTL cannot compute is refused at launch
// with `kv_geometry_unsupported`, whatever resolution saw.
#[test]
fn a_geometry_capyctl_cannot_compute_is_refused() {
    let root = checkpoint(&qwen3_4b());
    let effective = resolved(root.path());
    for patch in [
        json!({"use_sliding_window": true, "sliding_window": 4096}),
        json!({"kv_lora_rank": 512}),
        json!({"layer_types": ["full_attention", "sliding_attention"]}),
    ] {
        let mut config = qwen3_4b();
        for (key, value) in patch.as_object().unwrap() {
            config[key] = value.clone();
        }
        std::fs::write(root.path().join("config.json"), config.to_string()).unwrap();
        match plan_from_effective(&effective, 8101, "/l/i.log".into(), Path::new(LAUNCH_DIR)) {
            Err(TensorrtLlmPlanError::Geometry(reason)) => {
                assert!(reason.starts_with("kv_geometry_unsupported"), "{reason}")
            }
            other => panic!("{patch}: {other:?}"),
        }
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

// T49 T37 (ADR 0038 §3, owner decision 2026-10-27, question 3): the launch
// directory is new and private (0700), `tmp/` too, `model/<served name>`
// links the verified checkpoint, the file is CapyCTL's (0600) and holds the
// rendered text; a directory left by another launch, a served name that is
// not one path component and a reserved key are refused.
#[test]
fn the_launch_directory_link_and_file() {
    let root = checkpoint(&qwen3_4b());
    let runtime = tempfile::tempdir().unwrap();
    let mut input = plan("1.2.1");
    input.checkpoint_dir = root.path().to_string_lossy().into_owned();
    input.launch_dir = runtime
        .path()
        .join("tensorrt-llm/i-1")
        .to_string_lossy()
        .into_owned();
    prepare_launch_dir(&input).unwrap();
    let dir = Path::new(&input.launch_dir);
    assert_eq!(mode(dir), 0o700);
    assert_eq!(mode(&dir.join("tmp")), 0o700);
    assert_eq!(mode(&dir.join("model")), 0o700);
    let link = dir.join("model/qwen3-4b");
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_link(&link).unwrap(), root.path());
    assert!(link.join("config.json").is_file());
    let file = dir.join("llm_api_options.yaml");
    assert_eq!(mode(&file), 0o600);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), YAML_1_2);
    assert!(matches!(
        prepare_launch_dir(&input),
        Err(TensorrtLlmArgsError::LaunchDir(_))
    ));
    for served in ["", ".", "..", "a/b", "a\0b"] {
        let mut named = input.clone();
        named.served_model_name = served.into();
        named.launch_dir = runtime.path().join("other").to_string_lossy().into_owned();
        match prepare_launch_dir(&named).unwrap_err() {
            TensorrtLlmArgsError::ServedName(reason) => {
                assert!(reason.starts_with("served_name_unsupported"), "{reason}")
            }
            other => panic!("{served:?}: {other:?}"),
        }
        assert!(matches!(
            render_command(&named),
            Err(TensorrtLlmArgsError::ServedName(_))
        ));
        assert!(!runtime.path().join("other").exists(), "{served:?}");
    }
    let mut reserved = input.clone();
    reserved.launch_dir = runtime.path().join("keyed").to_string_lossy().into_owned();
    reserved.llm_api_options = json!({"kv_cache_config": {"max_tokens": 1}})
        .as_object()
        .unwrap()
        .clone();
    assert!(matches!(
        prepare_launch_dir(&reserved),
        Err(TensorrtLlmArgsError::Options(_))
    ));
    assert!(!runtime.path().join("keyed").exists());
}

// T49 T21 T37 (ADR 0038 §3, §9, §10, SPEC §13.3): the closed environment:
// CapyCTL's HOME and the GPU pin, the private TMPDIR, the profile's
// CUDA_HOME, the closed PATH, every PROCESS_ENV pin; the usage-reporting
// opt-outs under `off` only and the offline switches under both; no name
// the policy refuses, whatever the resolved environment or the caller holds.
#[test]
fn the_environment_is_closed() {
    let mut input = plan("1.2.1");
    input.cuda_namespaces = vec![capyctl_config::effective::CudaNamespace::Uuid(
        "GPU-1".into(),
    )];
    input.build_env = BTreeMap::from([
        ("TOKENIZERS_PARALLELISM".to_owned(), "false".to_owned()),
        ("TLLM_WORKER_USE_SINGLE_PROCESS".to_owned(), "0".to_owned()),
        ("OMPI_MCA_btl".to_owned(), "tcp".to_owned()),
        ("HF_HUB_OFFLINE".to_owned(), "0".to_owned()),
        ("TMPDIR".to_owned(), "/tmp".to_owned()),
        ("NCCL_SOCKET_IFNAME".to_owned(), "eth0".to_owned()),
    ]);
    let rendered = render_command(&input).unwrap().env;
    let inherited = |name: &str| match name {
        "HOME" => Some("/home/capyctl".to_owned()),
        "CUDA_VISIBLE_DEVICES" => Some("GPU-0".to_owned()),
        "TRTLLM_NO_USAGE_STATS" => Some("0".to_owned()),
        "OMPI_MCA_pml" => Some("ucx".to_owned()),
        _ => None,
    };
    let off = engine_environment(&rendered, &input, &inherited, EngineTelemetry::Off);
    let mut expected: Vec<&str> = vec![
        "CAPYCTL_ENGINE_LOG",
        "CUDA_HOME",
        "CUDA_VISIBLE_DEVICES",
        "DO_NOT_TRACK",
        "HOME",
        "PATH",
        "TELEMETRY_DISABLED",
        "TMPDIR",
        "TOKENIZERS_PARALLELISM",
        "TRTLLM_NO_USAGE_STATS",
    ];
    expected.extend(PROCESS_ENV.iter().map(|(name, _)| *name));
    expected.sort_unstable();
    assert_eq!(off.keys().map(String::as_str).collect::<Vec<_>>(), expected);
    for (name, value) in PROCESS_ENV {
        assert_eq!(off[*name], *value, "{name}");
    }
    assert_eq!(off["TLLM_WORKER_USE_SINGLE_PROCESS"], "1");
    assert_eq!(off["OMPI_MCA_btl"], "self");
    assert_eq!(off["OMPI_MCA_ess_singleton_isolated"], "1");
    assert_eq!(off["TRTLLM_RESPONSES_API_DISABLE_STORE"], "1");
    for name in [
        "DO_NOT_TRACK",
        "TRTLLM_NO_USAGE_STATS",
        "TELEMETRY_DISABLED",
    ] {
        assert_eq!(off[name], "1", "{name}");
    }
    assert_eq!(off["TMPDIR"], format!("{LAUNCH_DIR}/tmp"));
    assert_eq!(off["HOME"], "/home/capyctl");
    assert_eq!(off["CUDA_VISIBLE_DEVICES"], "GPU-1", "the pin wins");
    assert_eq!(off["CUDA_HOME"], "/usr/local/cuda-13.1");
    assert!(
        off["PATH"].starts_with("/home/u/tensorrt-llm-1.2.1-venv/bin:/usr/local/cuda-13.1/bin:"),
        "{}",
        off["PATH"]
    );
    assert_eq!(off["CAPYCTL_ENGINE_LOG"], "/var/lib/capyctl/logs/i-1.log");
    assert!(!off.contains_key("NCCL_SOCKET_IFNAME"));
    assert!(!off.contains_key("OMPI_MCA_pml") || off["OMPI_MCA_pml"] == "ob1");
    // Under `allow` no opt-out is set; the offline switches stay.
    let allow = engine_environment(&rendered, &input, &inherited, EngineTelemetry::Allow);
    for name in [
        "DO_NOT_TRACK",
        "TRTLLM_NO_USAGE_STATS",
        "TELEMETRY_DISABLED",
    ] {
        assert!(!allow.contains_key(name), "{name}");
    }
    assert_eq!(allow["HF_HUB_OFFLINE"], "1");
    assert_eq!(allow["TRANSFORMERS_OFFLINE"], "1");
    assert_eq!(allow.len(), off.len() - 3);
}

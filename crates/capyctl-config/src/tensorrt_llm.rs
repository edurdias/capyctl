//! ADR 0038: TensorRT-LLM's `trtllm-serve`, the sixth engine kind
//! (`tensorrt-llm`, owner decision 2026-10-27, question 8). Registration
//! constants shared by `engine add`, the roles' own installations and the
//! listings; the option, LLM API key and environment tables of ADR 0038 §3;
//! the served-name rule; the KV tokens CapyCTL renders from the grant (§4);
//! and the configuration file CapyCTL writes for every launch.
//!
//! The tables are kept per qualified version line (§2a). They were read from
//! each line's tag: 1.2.1 at `376f7e1` (the version ADR 0038 was designed
//! from), the 1.3 pre-release at `v1.3.0rc29` (`c76f4a8`, the newest 1.3
//! pre-release when this was implemented; TR1a records the build the host
//! runs) and the 1.4 pre-release at `v1.4.0rc1` (`0372799`). A name reserved
//! on any line is refused on every line: `trtllm-serve` refuses an option its
//! version lacks anyway, so the union only refuses earlier.
use crate::engine_policy::Sensitivity;
use serde_json::{Map, Value};
use std::path::Path;

/// ADR 0038 §2: the console script `engine add` finds in `<env>/bin`.
pub const EXECUTABLE: &str = "trtllm-serve";

/// ADR 0038 §2: the distribution detection and the version check read.
pub const DISTRIBUTION: &str = "tensorrt_llm";

/// ADR 0038 §2: the import packages the fingerprint digests (the wheel's
/// `top_level.txt` also names `examples`, which the server never imports).
pub const IMPORT_PACKAGES: &[&str] = &["tensorrt_llm", "triton_kernels"];

/// ADR 0038 §2: the distributions whose versions `engine add` records beside
/// TensorRT-LLM's and `engine list` shows. They are outside the digest; a
/// change of one shows in the listing.
pub const COMPANIONS: &[&str] = &[
    "torch",
    "tensorrt",
    "flashinfer-python",
    "transformers",
    "mpi4py",
];

/// ADR 0038 §2: the companion a verified-set entry names beside the version.
pub const TORCH: &str = "torch";

/// ADR 0038 §2: the profile name `engine add` writes by default.
pub const DEFAULT_PROFILE: &str = "tensorrt-llm";

/// ADR 0038 §3: the configuration file CapyCTL writes into each launch
/// directory, the only one the engine reads (mode 0600).
pub const CONFIG_FILE: &str = "llm_api_options.yaml";

/// ADR 0038 §3: the launch directory's subdirectory holding the served-name
/// link (`model/<served name>`).
pub const MODEL_DIR: &str = "model";

/// ADR 0038 §3: the backend every qualified line renders. 1.2.1 accepts
/// `pytorch`, `tensorrt`/`trt` and `_autodeploy`; the 1.3 and 1.4
/// pre-releases accept `pytorch` alone (`commands/serve.py`).
pub const BACKEND: &str = "pytorch";

/// ADR 0038 §3: rendered so no engine default can move it; on its own it is
/// not the pool's size (§4).
pub const FREE_GPU_MEMORY_FRACTION: f64 = 0.9;

/// ADR 0038 §3: the engine's log level.
pub const LOG_LEVEL: &str = "info";

/// ADR 0038 §3, §6: the iteration entries the engine keeps between reads.
pub const ITER_STATS_MAX_ITERATIONS: u64 = 1000;

/// ADR 0038 §9: the environment every launch is started with, so the model
/// runs in the `trtllm-serve` process (`llmapi/utils.py`) and the MPI
/// runtime is an isolated singleton with no network transport (Open MPI
/// 4.1.6, `ess_singleton_module.c`; the transport pins are read from Open
/// MPI's parameters, TR1 and TR9 confirm them).
pub const PROCESS_ENV: &[(&str, &str)] = &[
    ("TLLM_WORKER_USE_SINGLE_PROCESS", "1"),
    ("OMPI_MCA_ess_singleton_isolated", "1"),
    ("OMPI_MCA_btl", "self"),
    ("OMPI_MCA_pml", "ob1"),
    // ADR 0038 §3, §10: the engine keeps no conversation history.
    ("TRTLLM_RESPONSES_API_DISABLE_STORE", "1"),
    // ADR 0038 §3: the checkpoint is always local.
    ("HF_HUB_OFFLINE", "1"),
    ("TRANSFORMERS_OFFLINE", "1"),
];

/// ADR 0038 §2a: one qualified version line and what differs between lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionLine {
    /// `major.minor`.
    pub line: (u32, u32),
    /// The tag and commit the line's tables were read from.
    pub tag: &'static str,
    pub commit: &'static str,
    /// Whether `TorchLlmArgs` still has `enable_sleep`, which CapyCTL then
    /// renders `false` (§3, §8). The 1.3 and 1.4 pre-releases removed it (their
    /// sleep is `sleep_config`, reserved); their strict model would refuse it.
    pub renders_enable_sleep: bool,
}

/// ADR 0038 §2a: the lines qualified in 0.2.0, oldest first.
pub const LINES: &[VersionLine] = &[
    VersionLine {
        line: (1, 2),
        tag: "v1.2.1",
        commit: "376f7e1",
        renders_enable_sleep: true,
    },
    VersionLine {
        line: (1, 3),
        tag: "v1.3.0rc29",
        commit: "c76f4a8",
        renders_enable_sleep: false,
    },
    VersionLine {
        line: (1, 4),
        tag: "v1.4.0rc1",
        commit: "0372799",
        renders_enable_sleep: false,
    },
];

/// ADR 0038 §2a: the tables a version renders with: its own line's, else
/// 1.2.1's (an uncertified version of another line; the readiness checks of
/// §5 and the engine's strict models catch what moved).
pub fn version_line(version: &str) -> &'static VersionLine {
    let mut parts = version.trim().split('.');
    let major = parts.next().and_then(|p| p.parse::<u32>().ok());
    let minor = parts.next().and_then(|p| {
        let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    match (major, minor) {
        (Some(major), Some(minor)) => LINES
            .iter()
            .find(|line| line.line == (major, minor))
            .unwrap_or(&LINES[0]),
        _ => &LINES[0],
    }
}

/// ADR 0038 §3: options CapyCTL renders on every launch, each row one option
/// with every spelling `trtllm-serve` accepts.
pub const RESERVED_RENDERED: &[&[&str]] = &[
    &["--host"],
    &["--port"],
    &["--backend"],
    &["--max_seq_len"],
    &["--max_batch_size"],
    &["--tensor_parallel_size", "--tp_size"],
    &["--pipeline_parallel_size", "--pp_size"],
    &["--gpus_per_node"],
    &[
        "--free_gpu_memory_fraction",
        "--kv_cache_free_gpu_memory_fraction",
    ],
    &["--config", "--extra_llm_api_options"],
    &["--log_level"],
    &["--trust_remote_code"],
    &["--reasoning_parser"],
    &["--tool_parser"],
];

/// ADR 0038 §3: options CapyCTL never renders and nobody may pass. Rows
/// marked 1.3+ or 1.4 exist only on those lines (read from their tags).
pub const RESERVED_NEVER_RENDERED: &[&[&str]] = &[
    &["--context_parallel_size", "--cp_size"],
    &["--moe_expert_parallel_size", "--ep_size"],
    &["--moe_cluster_parallel_size", "--cluster_size"],
    // CapyCTL's chat contract has no beam search; beams multiply a
    // request's KV.
    &["--max_beam_width"],
    &["--tokenizer"],
    // Both import code.
    &["--custom_tokenizer"],
    &["--custom_module_dirs"],
    // 1.3+ spells it `--hf_revision` too.
    &["--revision", "--hf_revision"],
    // Disaggregated serving and tracing.
    &["--metadata_server_config_file"],
    &["--server_role"],
    &["--disagg_cluster_uri"],
    &["--otlp_traces_endpoint"],
    &["--num_postprocess_workers"],
    // 1.3+: imports a post-processing hook (code).
    &["--post_processor_hook"],
    // 1.3+: more frontend processes sharing the port.
    &["--num_serve_frontends"],
    &["--enable_attention_dp"],
    // 1.3+: the engine's own usage reporting (§10, `engine_telemetry`).
    &["--telemetry", "--no-telemetry"],
    // 1.3+: CapyCTL names the served model by its link (§3).
    &["--served_model_name"],
    // 1.3+: imports middleware (code).
    &["--middleware"],
    // 1.3+: a second, gRPC listener.
    &["--grpc"],
    &["--grpc-protocol"],
    &["--openengine-enable-load-metrics"],
    // 1.3+: out of scope (§1), and the agent scheduler reaches out to the
    // search services it names.
    &["--enable_visual_gen"],
    &["--visual_gen_args"],
    &["--agent_percentage"],
    &["--agent_types"],
    // 1.3+: writes the bound address to a file; the port is CapyCTL's.
    &["--report_addr"],
    // 1.3+: lets clients send their own chat templates.
    &["--allow_request_chat_template"],
    // 1.4: overrides any LLM API path after `--config` (§3: CapyCTL's file
    // is the only source).
    &["--set"],
    // 1.4: sleep and its authenticated control API (§2a, §8).
    &["--enable_sleep_mode"],
];

/// ADR 0038 §3, ADR 0014 §2: the reserved options a typed field renders, so
/// a refusal names the field to set instead. `--kv_cache_dtype` exists on the
/// 1.3 and 1.4 lines; CapyCTL renders the typed field into the file.
pub const TYPED_OPTIONS: &[(&str, &str)] = &[
    ("--max_seq_len", "context_length"),
    ("--max_batch_size", "max_concurrent_requests"),
    ("--trust_remote_code", "trust_remote_code"),
    ("--reasoning_parser", "tensorrt-llm.reasoning_parser"),
    ("--tool_parser", "tensorrt-llm.tool_call_parser"),
    ("--kv_cache_dtype", "kv_cache_dtype"),
];

/// ADR 0038 §3 (ADR 0014 §8): `--chat_template` takes a file path or a
/// one-line template; the host approves it by name and its file value lies
/// inside `security.approved_paths`.
pub const SENSITIVE: &[(&[&str], Sensitivity)] = &[(
    &["--chat_template"],
    Sensitivity::Path {
        checkpoint_exempt: false,
    },
)];

/// ADR 0038 §3: the ordinary options the guide lists (anything else not
/// reserved or sensitive is ordinary too; `trtllm-serve` refuses an unknown
/// one at start).
pub const ORDINARY: &[&str] = &[
    "--max_num_tokens",
    "--enable_chunked_prefill",
    "--fail_fast_on_attention_window_too_large",
    "--media_io_kwargs",
];

/// ADR 0014 open issue 5: options whose normalized name has a sensitive
/// shape but whose value is none of those things: the 1.3+
/// `--generation-config` takes `auto` or `trtllm`, not a file.
pub const ORDINARY_SHAPED: &[&str] = &["--generation-config"];

/// One option name, compared: lower case, `_` folded onto `-`, no `=value`
/// (`engine_policy::normalize_option_name`). Click takes exact names, so no
/// prefix ever stands for an option.
fn fold(name: &str) -> String {
    crate::engine_policy::normalize_option_name(name)
}

/// The table row naming `name`, in any spelling.
fn row_of<'a>(table: &'a [&'a [&'a str]], name: &str) -> Option<&'a [&'a str]> {
    let name = fold(name);
    table
        .iter()
        .copied()
        .find(|row| row.iter().any(|spelling| fold(spelling) == name))
}

/// ADR 0038 §3: whether `name` is a reserved option in any spelling.
pub fn is_reserved(name: &str) -> bool {
    row_of(RESERVED_RENDERED, name).is_some() || row_of(RESERVED_NEVER_RENDERED, name).is_some()
}

/// The typed field a reserved option renders, when one does.
pub fn typed_field(name: &str) -> Option<&'static str> {
    let name = fold(name);
    TYPED_OPTIONS
        .iter()
        .find(|(option, _)| fold(option) == name)
        .map(|(_, field)| *field)
}

/// ADR 0014 §8: the sensitivity of a listed option, and every spelling of
/// it (an approval of one spelling approves the option).
pub fn sensitive(name: &str) -> Option<(&'static [&'static str], Sensitivity)> {
    let name = fold(name);
    SENSITIVE
        .iter()
        .find(|(row, _)| row.iter().any(|spelling| fold(spelling) == name))
        .map(|(row, kind)| (*row, *kind))
}

/// One option however it is spelled (normalized): its row's first name, so
/// two spellings of one option are a duplicate; a `--no-` negation folds
/// onto its base.
pub fn option_key(name: &str) -> String {
    for table in [RESERVED_RENDERED, RESERVED_NEVER_RENDERED] {
        if let Some(row) = row_of(table, name) {
            return fold(row[0]);
        }
    }
    let name = fold(name);
    name.strip_prefix("--no-")
        .map_or_else(|| name.clone(), |base| format!("--{base}"))
}

/// ADR 0038 §3: LLM API keys CapyCTL renders into its file, as dotted paths.
/// A deployment's `llm_api_options` may not name one.
pub const RESERVED_RENDERED_KEYS: &[&str] = &[
    "model",
    "backend",
    "max_seq_len",
    "max_batch_size",
    "tensor_parallel_size",
    "pipeline_parallel_size",
    "gpus_per_node",
    "kv_cache_config.max_tokens",
    "kv_cache_config.free_gpu_memory_fraction",
    "kv_cache_config.dtype",
    // Never set: no engine-owned host cache (SPEC §12).
    "kv_cache_config.host_cache_size",
    "enable_iter_perf_stats",
    "enable_iter_req_stats",
    "iter_stats_max_iterations",
    "return_perf_metrics",
    "perf_metrics_max_requests",
    "enable_sleep",
    "dtype",
    "trust_remote_code",
    // Rendered from the typed parser fields (`--reasoning_parser`).
    "reasoning_parser",
];

/// ADR 0038 §3: LLM API keys CapyCTL never renders and nobody may set.
/// Beside the ADR's list, read from the three tags: `env_overrides` (sets the
/// engine's environment), `mpi_session` (the process model), the 1.3+
/// `sleep_config`, `telemetry_config`, `post_processor_hook` (code),
/// `num_serve_frontends`, `perf_metrics_output_dir` (writes files),
/// `prometheus_metrics_config`, `mx_config` (peer-to-peer checkpoint
/// loading), `gms_config` (a GPU memory service), `dwdp_config`,
/// `encode_only` and `model_kwargs` (overrides the model configuration
/// CapyCTL sizes from), the 1.4 `iter_perf_stats_interval` (the iteration
/// statistics §5 and §6 read), and the KV cache fields that move the pool
/// off the GPU or change the geometry CapyCTL sized it from.
pub const RESERVED_NEVER_KEYS: &[&str] = &[
    "context_parallel_size",
    "cp_config",
    "moe_expert_parallel_size",
    "moe_cluster_parallel_size",
    "moe_tensor_parallel_size",
    "enable_lm_head_tp_in_adp",
    "attention_dp_config",
    "pp_partition",
    "max_beam_width",
    "tokenizer",
    "custom_tokenizer",
    "tokenizer_revision",
    "revision",
    "skip_tokenizer_init",
    "load_format",
    "checkpoint_format",
    "checkpoint_loader",
    "orchestrator_type",
    "kv_connector_config",
    "cache_transceiver_config",
    "otlp_traces_endpoint",
    "num_postprocess_workers",
    "postprocess_tokenizer_dir",
    "mm_encoder_only",
    "enable_attention_dp",
    "speculative_config",
    "lora_config",
    "enable_lora",
    "peft_cache_config",
    "env_overrides",
    "mpi_session",
    "sleep_config",
    "telemetry_config",
    "post_processor_hook",
    "num_serve_frontends",
    "perf_metrics_output_dir",
    "prometheus_metrics_config",
    "mx_config",
    "gms_config",
    "dwdp_config",
    "encode_only",
    "model_kwargs",
    "iter_perf_stats_interval",
    "kv_cache_config.max_gpu_total_bytes",
    "kv_cache_config.use_uvm",
    "kv_cache_config.disk_cache_path",
    "kv_cache_config.disk_cache_size",
    "kv_cache_config.disk_prefetch_num_reqs",
    "kv_cache_config.cross_kv_cache_fraction",
    "kv_cache_config.iteration_stats_interval",
    "kv_cache_config.max_attention_window",
    "kv_cache_config.sink_token_length",
];

/// ADR 0038 §3: whole key families nobody may set: Ray's orchestrator and
/// every disaggregation field.
pub const RESERVED_KEY_PREFIXES: &[&str] = &["ray_", "disagg"];

/// The key a typed field renders and a mapping may then not repeat: with
/// `cuda_graphs` declared, `cuda_graph_config` is CapyCTL's.
pub const CUDA_GRAPHS_KEY: &str = "cuda_graph_config";

/// The largest `llm_api_options` mapping, as JSON text.
pub const MAX_OPTIONS_BYTES: usize = 64 * 1024;

fn reserved_key(path: &str) -> bool {
    RESERVED_RENDERED_KEYS.contains(&path)
        || RESERVED_NEVER_KEYS.contains(&path)
        || RESERVED_KEY_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

/// ADR 0038 §3 (owner decision 2026-10-27, question 6): the first key of a
/// deployment's `llm_api_options` that CapyCTL reserves, as a dotted path,
/// nested keys included (a whole reserved object counts too). With
/// `cuda_graphs` declared, `cuda_graph_config` is reserved as well.
pub fn first_reserved_key(options: &Map<String, Value>, cuda_graphs: bool) -> Option<String> {
    for (key, value) in options {
        if reserved_key(key) || (cuda_graphs && key == CUDA_GRAPHS_KEY) {
            return Some(key.clone());
        }
        if let Value::Object(nested) = value {
            for nested_key in nested.keys() {
                let path = format!("{key}.{nested_key}");
                if reserved_key(&path) {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// ADR 0038 §3: check a deployment's `llm_api_options` as written: a mapping
/// of bounded size whose keys are plain identifiers, naming no reserved key.
/// The message is the refusal's whole reason.
pub fn check_options(options: &Map<String, Value>, cuda_graphs: bool) -> Result<(), String> {
    let size = serde_json::to_string(options).map_or(usize::MAX, |text| text.len());
    if size > MAX_OPTIONS_BYTES {
        return Err(format!(
            "the mapping is {size} bytes of JSON; at most {MAX_OPTIONS_BYTES} are accepted"
        ));
    }
    if let Some(key) = first_reserved_key(options, cuda_graphs) {
        // ADR 0038 §3: rendered by CapyCTL or out of scope.
        return Err(format!(
            "key `{key}` is reserved: CapyCTL renders it, or it is out of scope for \
             TensorRT-LLM in this version; remove it"
        ));
    }
    if let Some(key) = keys(options).find(|key| !plain_key(key)) {
        return Err(format!(
            "key `{key}` is not an LLM API field name (lower-case letters, digits and `_`)"
        ));
    }
    Ok(())
}

fn keys(options: &Map<String, Value>) -> impl Iterator<Item = &String> {
    options.iter().flat_map(|(key, value)| {
        std::iter::once(key).chain(
            value
                .as_object()
                .into_iter()
                .flat_map(|nested| nested.keys()),
        )
    })
}

fn plain_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// ADR 0038 §3: the KV cache dtypes CapyCTL sizes (`kv_cache_config.dtype`):
/// `auto` (the model's dtype) and `fp8`. NVFP4's scales are not sized here.
pub const KV_CACHE_DTYPES: &[&str] = &["auto", "fp8"];

/// ADR 0038 §4: TensorRT-LLM's KV block (`KvCacheConfig.tokens_per_block`,
/// 32 by default in the three tags).
pub const DEFAULT_TOKENS_PER_BLOCK: u32 = 32;

/// The KV block a launch builds its pool in: the deployment's ordinary
/// `kv_cache_config.tokens_per_block`, else [`DEFAULT_TOKENS_PER_BLOCK`].
pub fn tokens_per_block(options: &Map<String, Value>) -> u32 {
    options
        .get("kv_cache_config")
        .and_then(|kv| kv.get("tokens_per_block"))
        .and_then(Value::as_u64)
        .and_then(|tokens| u32::try_from(tokens).ok())
        .filter(|tokens| *tokens > 0)
        .unwrap_or(DEFAULT_TOKENS_PER_BLOCK)
}

/// ADR 0038 §3: why a served name cannot be the name of the per-launch link,
/// or `None`: it must be one path component (owner decision 2026-10-27,
/// question 3). The reason leads with `served_name_unsupported`.
pub fn served_name_refusal(name: &str) -> Option<String> {
    let problem = if name.is_empty() {
        "is empty"
    } else if name == "." || name == ".." {
        "is `.` or `..`"
    } else if name.contains('/') {
        "contains `/`"
    } else if name.contains('\0') {
        "contains a NUL byte"
    } else if name.len() > 255 {
        "is longer than 255 bytes"
    } else {
        return None;
    };
    Some(format!(
        "served_name_unsupported: the served name `{}` {problem}; TensorRT-LLM serves the \
         name of the model directory it is given, so CapyCTL links the checkpoint under the \
         served name, which must be one path component",
        name.escape_debug()
    ))
}

/// ADR 0038 §3: the closed refusal of a profile's or deployment's
/// environment. The families TensorRT-LLM, its MPI runtime, its transports
/// and its libraries read, the device and directory names, and the other
/// names read from the three tags' environment lookups that move a
/// listener, a device, a directory or a reserved value.
pub const REFUSED_ENV_PREFIXES: &[&str] = &[
    "TLLM_",
    "TRTLLM_",
    "TRT_LLM_",
    "OMPI_",
    "PMIX_",
    "PRTE_",
    "UCX_",
    "NCCL_",
    "MPI4PY_",
    "HF_",
    "TRANSFORMERS_",
    "FLASHINFER_",
    "RAY_",
    "SLURM_",
];

/// ADR 0038 §3: single names refused beside the families.
pub const REFUSED_ENV_NAMES: &[&str] = &[
    "CUDA_VISIBLE_DEVICES",
    "NVIDIA_VISIBLE_DEVICES",
    "DISABLE_TORCH_DEVICE_SET",
    "CUDA_MPS_PIPE_DIRECTORY",
    "TMPDIR",
    "HOME",
    "RANK",
    "LOCAL_RANK",
    "WORLD_SIZE",
    "TLLM_MPI_SIZE",
    "MASTER_ADDR",
    "MASTER_PORT",
    "PROMETHEUS_MULTIPROC_DIR",
    "DG_JIT_CACHE_DIR",
    "MX_TRANSFER_LOG_DIR",
    "MODEL_EXPRESS_URL",
    "TELEMETRY_DISABLED",
    "DO_NOT_TRACK",
];

/// Whether a profile or deployment environment may not name `name` on a
/// TensorRT-LLM profile (compared upper-cased, as `engine_env::is_owned`
/// is).
pub fn refused_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    REFUSED_ENV_PREFIXES
        .iter()
        .any(|prefix| upper.starts_with(prefix))
        || REFUSED_ENV_NAMES.contains(&upper.as_str())
}

/// The first refused name of a TensorRT-LLM profile's or deployment's
/// `env`, as the closed reason `engine_env_reserved:<name>`.
pub fn refused_env<'a>(names: impl IntoIterator<Item = &'a String>) -> Option<String> {
    names
        .into_iter()
        .find(|name| refused_env_name(name))
        .map(|name| format!("engine_env_reserved:{name}"))
}

/// ADR 0038 §4: the closed code of a checkpoint whose KV geometry CapyCTL
/// cannot compute.
pub const KV_GEOMETRY_UNSUPPORTED: &str = "kv_geometry_unsupported";

/// ADR 0038 §4: the per-token KV geometry of a full-attention decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvGeometry {
    pub layers: u64,
    pub kv_heads: u64,
    pub head_dim: u64,
    /// Bytes of one element of the KV cache dtype.
    pub element_bytes: u64,
}

impl KvGeometry {
    /// `2 × layers × KV heads × head dimension × bytes(KV dtype)`.
    pub fn bytes_per_token(&self) -> Option<u64> {
        2u64.checked_mul(self.layers)?
            .checked_mul(self.kv_heads)?
            .checked_mul(self.head_dim)?
            .checked_mul(self.element_bytes)
    }

    /// ADR 0038 §4: `kv_cache_config.max_tokens = floor(kv_bytes /
    /// bytes_per_token)`. `None` when the pool holds no token.
    pub fn max_tokens(&self, kv_cache_bytes: i64) -> Option<u64> {
        let bytes = u64::try_from(kv_cache_bytes).ok()?;
        let tokens = bytes / self.bytes_per_token().filter(|b| *b > 0)?;
        (tokens > 0).then_some(tokens)
    }
}

fn dtype_bytes(dtype: &str) -> Option<u64> {
    match dtype {
        "bfloat16" | "float16" | "bf16" | "fp16" | "half" => Some(2),
        "float32" | "fp32" | "float" => Some(4),
        _ => None,
    }
}

fn positive(config: &Map<String, Value>, key: &str) -> Option<u64> {
    config.get(key).and_then(Value::as_u64).filter(|v| *v > 0)
}

fn names_set(config: &Map<String, Value>, key: &str) -> bool {
    config.get(key).is_some_and(|value| !value.is_null())
}

/// ADR 0038 §4: the KV geometry of the checkpoint configuration `config`
/// (its `config.json`), with the deployment's `dtype` and `kv_cache_dtype`
/// (`None` or `auto` follow the model). Refused, with the reason leading
/// with [`KV_GEOMETRY_UNSUPPORTED`], where CapyCTL cannot compute it: sliding
/// or per-layer attention windows, multi-head latent attention, recurrent or
/// hybrid layers, encoder-decoder models, or a shape it cannot read. A model
/// that names a sliding window but turns it off (`use_sliding_window:
/// false`, as Qwen3 does beside `max_window_layers`) is full attention.
pub fn kv_geometry(
    config: &Value,
    dtype: Option<&str>,
    kv_cache_dtype: Option<&str>,
) -> Result<KvGeometry, String> {
    let refuse = |what: &str| {
        Err(format!(
            "{KV_GEOMETRY_UNSUPPORTED}: {what}; CapyCTL sizes TensorRT-LLM's KV pool for \
             full-attention decoders only in this version"
        ))
    };
    let Some(top) = config.as_object() else {
        return refuse("the checkpoint's config.json is not an object");
    };
    // A multimodal checkpoint keeps its language model's shape here.
    let text = top
        .get("text_config")
        .and_then(Value::as_object)
        .filter(|_| !top.contains_key("num_hidden_layers"))
        .unwrap_or(top);
    if top.get("is_encoder_decoder").and_then(Value::as_bool) == Some(true) {
        return refuse("the model is an encoder-decoder model");
    }
    if names_set(text, "kv_lora_rank") {
        return refuse("the model uses multi-head latent attention (`kv_lora_rank`)");
    }
    let sliding_on = match text.get("use_sliding_window").and_then(Value::as_bool) {
        Some(on) => on,
        None => names_set(text, "sliding_window"),
    };
    if sliding_on {
        return refuse("the model uses sliding attention windows (`sliding_window`)");
    }
    if let Some(types) = text.get("layer_types").and_then(Value::as_array) {
        if types
            .iter()
            .any(|kind| kind.as_str() != Some("full_attention"))
        {
            return refuse("the model's layers are not all full attention (`layer_types`)");
        }
    }
    let model_type = text
        .get("model_type")
        .or_else(|| top.get("model_type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let recurrent = ["mamba", "ssm", "rwkv", "jamba", "nemotron_h", "linear"]
        .iter()
        .any(|word| model_type.contains(word))
        || text.keys().any(|key| {
            key.starts_with("mamba_")
                || key.starts_with("ssm_")
                || key.starts_with("linear_")
                || key == "hybrid_override_pattern"
        });
    if recurrent {
        return refuse("the model has recurrent or hybrid layers");
    }
    let Some(layers) = positive(text, "num_hidden_layers") else {
        return refuse("config.json names no `num_hidden_layers`");
    };
    let Some(heads) = positive(text, "num_attention_heads") else {
        return refuse("config.json names no `num_attention_heads`");
    };
    let kv_heads = positive(text, "num_key_value_heads").unwrap_or(heads);
    let head_dim = match positive(text, "head_dim") {
        Some(dim) => dim,
        None => match positive(text, "hidden_size") {
            Some(hidden) if hidden % heads == 0 => hidden / heads,
            _ => return refuse("config.json gives no `head_dim` and no whole `hidden_size`"),
        },
    };
    let element_bytes = match kv_cache_dtype.filter(|kind| *kind != "auto") {
        Some("fp8") => 1,
        Some(other) => {
            return refuse(&format!("the KV cache dtype `{other}` is not one CapyCTL sizes"))
        }
        None => {
            let model_dtype = dtype.filter(|kind| *kind != "auto").map(str::to_owned).or_else(|| {
                text.get("torch_dtype")
                    .or_else(|| text.get("dtype"))
                    .or_else(|| top.get("torch_dtype"))
                    .or_else(|| top.get("dtype"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            match model_dtype.as_deref().and_then(dtype_bytes) {
                Some(bytes) => bytes,
                None => {
                    return refuse(
                        "the model's dtype (`torch_dtype`) is not bfloat16, float16 or float32; \
                         declare engine_config.dtype",
                    )
                }
            }
        }
    };
    let geometry = KvGeometry {
        layers,
        kv_heads,
        head_dim,
        element_bytes,
    };
    if geometry.bytes_per_token().is_none() {
        return refuse("the model's KV bytes per token overflow");
    }
    Ok(geometry)
}

/// ADR 0038 §4: [`kv_geometry`] of the checkpoint at `checkpoint_root`.
pub fn checkpoint_kv_geometry(
    checkpoint_root: &Path,
    dtype: Option<&str>,
    kv_cache_dtype: Option<&str>,
) -> Result<KvGeometry, String> {
    let config = crate::context_fit::read_model_config(checkpoint_root).map_err(|reason| {
        format!("{KV_GEOMETRY_UNSUPPORTED}: the checkpoint's config.json: {reason}")
    })?;
    kv_geometry(&config, dtype, kv_cache_dtype)
}

/// What CapyCTL renders into [`CONFIG_FILE`] for one launch (ADR 0038 §3).
#[derive(Debug, Clone, PartialEq)]
pub struct LlmApiInputs<'a> {
    pub line: &'static VersionLine,
    /// §4: from the grant and the checkpoint's geometry.
    pub max_tokens: u64,
    /// The typed `kv_cache_dtype`, `auto` when undeclared.
    pub kv_cache_dtype: Option<&'a str>,
    /// The typed `dtype`, `auto` when undeclared.
    pub dtype: Option<&'a str>,
    /// The typed `cuda_graphs`: `false` renders `cuda_graph_config: null`.
    pub cuda_graphs: Option<bool>,
    /// The deployment's `llm_api_options`, merged under CapyCTL's keys.
    pub options: &'a Map<String, Value>,
}

/// ADR 0038 §3: the configuration document CapyCTL writes, which overrides
/// the command line and so is CapyCTL's whole. The deployment's mapping is
/// merged under CapyCTL's keys (one level deep for `kv_cache_config`), and
/// refused again here if it names a reserved key.
pub fn llm_api_document(inputs: &LlmApiInputs<'_>) -> Result<Value, String> {
    let cuda_graphs_declared = inputs.cuda_graphs.is_some();
    check_options(inputs.options, cuda_graphs_declared)?;
    let mut document = inputs.options.clone();
    let mut kv_cache = match document.remove("kv_cache_config") {
        Some(Value::Object(nested)) => nested,
        Some(_) => return Err("key `kv_cache_config` must be a mapping".into()),
        None => Map::new(),
    };
    kv_cache.insert("max_tokens".into(), inputs.max_tokens.into());
    kv_cache.insert(
        "free_gpu_memory_fraction".into(),
        FREE_GPU_MEMORY_FRACTION.into(),
    );
    kv_cache.insert(
        "dtype".into(),
        inputs.kv_cache_dtype.unwrap_or("auto").into(),
    );
    document.insert("kv_cache_config".into(), Value::Object(kv_cache));
    document.insert("enable_iter_perf_stats".into(), true.into());
    document.insert(
        "iter_stats_max_iterations".into(),
        ITER_STATS_MAX_ITERATIONS.into(),
    );
    document.insert("return_perf_metrics".into(), false.into());
    if inputs.line.renders_enable_sleep {
        document.insert("enable_sleep".into(), false.into());
    }
    document.insert("dtype".into(), inputs.dtype.unwrap_or("auto").into());
    if inputs.cuda_graphs == Some(false) {
        document.insert(CUDA_GRAPHS_KEY.into(), Value::Null);
    }
    Ok(Value::Object(document))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn qwen3_4b() -> Value {
        json!({
            "model_type": "qwen3", "num_hidden_layers": 36, "num_attention_heads": 32,
            "num_key_value_heads": 8, "head_dim": 128, "hidden_size": 2560,
            "torch_dtype": "bfloat16", "sliding_window": null, "use_sliding_window": false,
            "max_window_layers": 36
        })
    }

    // T49 (ADR 0038 §4): Qwen3-4B in BF16 is 147,456 bytes a token, so a
    // 4 GiB cache renders 29,127 tokens.
    #[test]
    fn qwen3_4b_max_tokens_from_a_4_gib_cache() {
        let geometry = kv_geometry(&qwen3_4b(), None, None).unwrap();
        assert_eq!(geometry.bytes_per_token(), Some(147_456));
        assert_eq!(geometry.max_tokens(4 << 30), Some(29_127));
        let fp8 = kv_geometry(&qwen3_4b(), None, Some("fp8")).unwrap();
        assert_eq!(fp8.bytes_per_token(), Some(73_728));
    }

    // T49 (ADR 0038 §4): each geometry CapyCTL cannot compute is refused.
    #[test]
    fn geometry_refusals() {
        for (patch, what) in [
            (json!({"use_sliding_window": true, "sliding_window": 4096}), "sliding"),
            (json!({"layer_types": ["full_attention", "sliding_attention"]}), "layer_types"),
            (json!({"kv_lora_rank": 512}), "kv_lora_rank"),
            (json!({"model_type": "nemotron_h"}), "recurrent"),
            (json!({"mamba_d_state": 128}), "recurrent"),
            (json!({"is_encoder_decoder": true}), "encoder-decoder"),
            (json!({"num_hidden_layers": null}), "num_hidden_layers"),
            (json!({"torch_dtype": "int8"}), "dtype"),
        ] {
            let mut config = qwen3_4b();
            for (key, value) in patch.as_object().unwrap() {
                config[key] = value.clone();
            }
            let reason = kv_geometry(&config, None, None).unwrap_err();
            assert!(reason.starts_with(KV_GEOMETRY_UNSUPPORTED), "{what}: {reason}");
            assert!(reason.contains(what), "{what}: {reason}");
        }
        let reason = kv_geometry(&qwen3_4b(), None, Some("nvfp4")).unwrap_err();
        assert!(reason.starts_with(KV_GEOMETRY_UNSUPPORTED), "{reason}");
        // A sliding window named but unused (no `use_sliding_window`) is a
        // window all the same.
        let mut named = qwen3_4b();
        named.as_object_mut().unwrap().remove("use_sliding_window");
        named["sliding_window"] = json!(4096);
        assert!(kv_geometry(&named, None, None).is_err());
    }

    // T49 (ADR 0038 §3): a served name is one path component.
    #[test]
    fn served_name_refusals() {
        assert_eq!(served_name_refusal("qwen3-4b"), None);
        let long = "x".repeat(256);
        for name in ["", ".", "..", "a/b", "a\0b", long.as_str()] {
            let reason = served_name_refusal(name).expect(name);
            assert!(reason.starts_with("served_name_unsupported"), "{reason}");
        }
        assert_eq!(served_name_refusal(&"x".repeat(255)), None);
    }

    // T49 (ADR 0038 §3): every reserved key is refused, nested ones too.
    #[test]
    fn reserved_keys_refused_nested_too() {
        for key in RESERVED_RENDERED_KEYS.iter().chain(RESERVED_NEVER_KEYS) {
            let options = match key.split_once('.') {
                Some((outer, inner)) => json!({ outer: { inner: 1 } }),
                None => json!({ *key: 1 }),
            };
            let options = options.as_object().unwrap();
            assert_eq!(first_reserved_key(options, false).as_deref(), Some(*key));
            assert!(check_options(options, false).is_err(), "{key}");
        }
        for key in ["ray_placement_config", "disaggregated_params"] {
            let options = json!({ key: {} });
            assert!(check_options(options.as_object().unwrap(), false).is_err(), "{key}");
        }
        let graphs = json!({"cuda_graph_config": {"max_batch_size": 8}});
        assert!(check_options(graphs.as_object().unwrap(), false).is_ok());
        assert!(check_options(graphs.as_object().unwrap(), true).is_err());
        let ordinary = json!({
            "enable_chunked_prefill": true, "max_num_tokens": 4096,
            "kv_cache_config": {"enable_block_reuse": false},
            "scheduler_config": {"capacity_scheduler_policy": "GUARANTEED_NO_EVICT"}
        });
        assert!(check_options(ordinary.as_object().unwrap(), false).is_ok());
        let odd = json!({"Model": 1});
        assert!(check_options(odd.as_object().unwrap(), false).is_err());
    }

    // T49 (ADR 0038 §3, §2a): the document for a minimal deployment, per line.
    #[test]
    fn document_per_line() {
        let options = Map::new();
        let document = llm_api_document(&LlmApiInputs {
            line: version_line("1.2.1"),
            max_tokens: 29_127,
            kv_cache_dtype: None,
            dtype: None,
            cuda_graphs: None,
            options: &options,
        })
        .unwrap();
        assert_eq!(
            crate::yaml_emit::to_block_yaml(&document),
            "dtype: \"auto\"\n\
             enable_iter_perf_stats: true\n\
             enable_sleep: false\n\
             iter_stats_max_iterations: 1000\n\
             kv_cache_config:\n  \
               dtype: \"auto\"\n  \
               free_gpu_memory_fraction: 0.9\n  \
               max_tokens: 29127\n\
             return_perf_metrics: false\n"
        );
        for version in ["1.3.0rc29", "1.4.0rc1", "1.4.0rc2"] {
            let document = llm_api_document(&LlmApiInputs {
                line: version_line(version),
                max_tokens: 29_127,
                kv_cache_dtype: None,
                dtype: None,
                cuda_graphs: Some(false),
                options: &options,
            })
            .unwrap();
            assert!(document.get("enable_sleep").is_none(), "{version}");
            assert_eq!(document["cuda_graph_config"], Value::Null);
        }
        // The deployment's mapping merges under CapyCTL's keys.
        let options = json!({"kv_cache_config": {"enable_block_reuse": false}, "stream_interval": 4});
        let document = llm_api_document(&LlmApiInputs {
            line: version_line("1.2.1"),
            max_tokens: 10,
            kv_cache_dtype: Some("fp8"),
            dtype: Some("bfloat16"),
            cuda_graphs: None,
            options: options.as_object().unwrap(),
        })
        .unwrap();
        assert_eq!(document["kv_cache_config"]["enable_block_reuse"], false);
        assert_eq!(document["kv_cache_config"]["max_tokens"], 10);
        assert_eq!(document["kv_cache_config"]["dtype"], "fp8");
        assert_eq!(document["stream_interval"], 4);
        assert_eq!(document["dtype"], "bfloat16");
    }

    // T49 (ADR 0038 §2a): an unlisted version renders with its line's tables,
    // else 1.2.1's.
    #[test]
    fn version_lines() {
        assert_eq!(version_line("1.2.1").line, (1, 2));
        assert_eq!(version_line("1.2.0").line, (1, 2));
        assert_eq!(version_line("1.3.0rc12").line, (1, 3));
        assert_eq!(version_line("1.4.0rc1").line, (1, 4));
        assert_eq!(version_line("1.5.0").line, (1, 2));
        assert_eq!(version_line("unknown").line, (1, 2));
    }

    // T49 (ADR 0038 §3): every alias and spelling of a reserved option, and
    // the refused environment.
    #[test]
    fn options_and_environment() {
        for row in RESERVED_RENDERED.iter().chain(RESERVED_NEVER_RENDERED) {
            for spelling in *row {
                assert!(is_reserved(&fold(spelling)), "{spelling}");
                assert_eq!(option_key(&fold(spelling)), fold(row[0]));
            }
        }
        assert!(!is_reserved(&fold("--max_num_tokens")));
        assert_eq!(typed_field(&fold("--max_batch_size")), Some("max_concurrent_requests"));
        assert!(sensitive(&fold("--chat_template")).is_some());
        for name in [
            "TLLM_WORKER_USE_SINGLE_PROCESS",
            "trtllm_responses_api_disable_store",
            "OMPI_MCA_btl",
            "PMIX_MCA_x",
            "PRTE_MCA_x",
            "UCX_TLS",
            "NCCL_SOCKET_IFNAME",
            "MPI4PY_RC_INITIALIZE",
            "HF_TOKEN",
            "TRANSFORMERS_CACHE",
            "FLASHINFER_CUBIN_DIR",
            "CUDA_VISIBLE_DEVICES",
            "TMPDIR",
            "HOME",
        ] {
            assert!(refused_env_name(name), "{name}");
        }
        assert!(!refused_env_name("TOKENIZERS_PARALLELISM"));
        assert_eq!(
            refused_env([&"A".to_owned(), &"TMPDIR".to_owned()]),
            Some("engine_env_reserved:TMPDIR".into())
        );
    }
}

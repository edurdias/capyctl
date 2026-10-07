//! Shared, closed launch argument and environment policy.
//!
//! ADR 0014 §3, §6, §8 (owner decisions E1, Q10): a deployment may pass ordinary
//! engine arguments behind `accept_extra_args: true`. Settings capyctl owns are
//! reserved and always refused, however they are spelled; security-sensitive
//! options (code loading, paths, listeners and egress) need named host approval.
//! Everything else is ordinary and passes through unchanged: capyctl checks shape,
//! not arity or meaning, which the engine's own parser owns.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Vllm,
    Sglang,
    /// ADR 0023: TensorFold, restart-only, registered with `engine add`.
    Tensorfold,
}

impl Engine {
    /// Every engine kind, in the order lists show them.
    pub const ALL: [Engine; 3] = [Engine::Vllm, Engine::Sglang, Engine::Tensorfold];

    /// The serde, CLI and profile name of the kind.
    pub fn name(self) -> &'static str {
        match self {
            Engine::Vllm => "vllm",
            Engine::Sglang => "sglang",
            Engine::Tensorfold => "tensorfold",
        }
    }

    /// The kind a name stands for; exact, lowercase.
    pub fn from_name(name: &str) -> Option<Engine> {
        Self::ALL.into_iter().find(|engine| engine.name() == name)
    }
}

/// ADR 0014 §3: vLLM options capyctl renders from the grant, placement and binding.
/// `--kv-cache-dtype` and `--block-size` are typed deployment fields now, not
/// reserved.
pub const VLLM_RESERVED_FLAGS: &[&str] = &[
    "--host",
    "--port",
    "--model",
    "--served-model-name",
    "--device",
    "--tensor-parallel-size",
    "--pipeline-parallel-size",
    "--gpu-memory-utilization",
    "--cpu-offload-gb",
    "--swap-space",
    "--kv-cache-bytes",
    "--kv-cache-memory",
    "--kv-cache-memory-bytes",
    "--enable-sleep-mode",
    "--api-key",
    // Spec §3: the guard middleware is owned by capyctl, not a profile — a
    // profile cannot pass its own `--middleware` to bypass or replace it.
    "--middleware",
    "--disable-log-requests",
    "--enable-log-requests",
    // W8 reads the engine's metrics; turning them off would blind load reports.
    "--disable-log-stats",
    "--log-config-file",
    "--uvicorn-log-level",
    "--disable-uvicorn-access-log",
    "--data-parallel-size",
    "--data-parallel-address",
    "--data-parallel-rpc-port",
    "--distributed-executor-backend",
    "--headless",
    "--api-server-count",
    "--uds",
    "--root-path",
    "--ssl-keyfile",
    "--ssl-certfile",
    "--ssl-ca-certs",
    "--revision",
    "--code-revision",
    // SPEC §8.2 / T21: vLLM 0.29 serves gRPC and re-reads SSL material on
    // these switches; capyctl owns every listener and the SSL family.
    "--grpc",
    "--enable-ssl-refresh",
    // SPEC §8.2 / T21: multi-node rendezvous. A single-rank launch is capyctl's;
    // these would open or dial a TCP rendezvous off the loopback file store.
    "--nnodes",
    "--node-rank",
    "--master-addr",
    "--master-port",
    // SPEC §8.2 / T21: vLLM 0.30 scale-out registers extra serving routes
    // (`/render`, `/derender`, `/inference/v1/generate`); capyctl never enables it.
    "--enable-scale-out",
];

/// Whole option families reserved for vLLM: any option whose name starts here.
/// `--capyctl-` is the protected entry's own family (its user-arguments marker);
/// the vLLM renderer refuses it, so resolution must too, before any effect.
const VLLM_RESERVED_FAMILIES: &[&str] = &["--ssl-", "--data-parallel-", "--capyctl-"];

/// ADR 0014 §4: reserved while sleep mode is on, where capyctl always renders
/// the loader: `eager` unless the deployment's typed
/// `vllm.safetensors_load_strategy` chose another (amended 2026-10-07).
const VLLM_SLEEP_RESERVED: &[&str] = &["--safetensors-load-strategy"];

/// ADR 0014 §3: SGLang `ServerArgs` fields capyctl owns. The option spelling is the
/// field with `_` replaced by `-`; [`SGLANG_RESERVED_ALIASES`] adds the other
/// spellings SGLang's parser accepts for the same fields.
pub const SGLANG_RESERVED_FIELDS: &[&str] = &[
    "host",
    "port",
    "api_key",
    "admin_api_key",
    "model_path",
    "tokenizer_path",
    "served_model_name",
    "revision",
    "device",
    "base_gpu_id",
    "gpu_id_step",
    "tp_size",
    "dp_size",
    "pp_size",
    "ep_size",
    "dcp_size",
    "attn_cp_size",
    "moe_dp_size",
    "nnodes",
    "node_rank",
    "dist_init_addr",
    // SPEC §8.2 / T21: rendezvous data is capyctl's; single-rank SGLang uses a
    // private file store (runtime/loopback_rendezvous.py), never a TCP port.
    "nccl_port",
    "use_ray",
    "enable_dp_attention",
    "mem_fraction_static",
    "cpu_offload_gb",
    "enable_memory_saver",
    "enable_weights_cpu_backup",
    "enable_draft_weights_cpu_backup",
    "grpc_port",
    "grpc_mode",
    "smg_grpc_mode",
    "sidecar",
    "sidecar_args",
    "smg_http_sidecar_port",
    "fastapi_root_path",
    "enable_http2",
    "enable_ssl_refresh",
    "log_level",
    "log_level_http",
    "log_requests",
    "log_requests_target",
    "crash_dump_folder",
    "enable_metrics",
    "skip_server_warmup",
    "disaggregation_mode",
    "enable_hierarchical_cache",
    "hicache_storage_backend",
    "hicache_storage_backend_extra_config",
    "enable_lmcache",
    "lmcache_config_file",
    "enable_flexkv",
    "flexkv_config_file",
    "quantize_and_serve",
];

/// Other spellings SGLang's parser accepts for reserved fields.
const SGLANG_RESERVED_ALIASES: &[&str] = &[
    "--model",
    "--tensor-parallel-size",
    "--tp",
    "--data-parallel-size",
    "--dp",
    "--pipeline-parallel-size",
    "--expert-parallel-size",
    "--ep",
    // Further aliases the installed 0.5.20 parser declares (fields/parallel.py,
    // fields/observability.py) for reserved fields.
    "--nccl-init-addr",
    "--decode-context-parallel-size",
    "--attention-context-parallel-size",
    "--moe-data-parallel-size",
    "--grpc-http-sidecar-port",
];

/// ADR 0023 §3: TensorFold 0.6.0 to 0.6.5 `serve` options capyctl renders or forbids
/// (`tensorfold/cli_args.py`). `--drafter` is not here: like other engines'
/// draft model options it is an approved path option. `--tp`, `--rank`,
/// `--master` and `--master-port` are rendered by capyctl for a two-rank group
/// member (ADR 0028 §10) and never stated by the user.
/// `--api-key`, `--api-key-file` and `--metrics-open` (0.6.5) are reserved as
/// vLLM's `--api-key` is: capyctl owns authentication, and an engine key would
/// lock capyctl out of the engine's routes and `/metrics`.
pub const TENSORFOLD_RESERVED_FLAGS: &[&str] = &[
    "--host",
    "--port",
    "--name",
    "--alias",
    "--backend",
    "--context",
    "--tp",
    "--rank",
    "--master",
    "--master-port",
    "--snapshot-dir",
    "--no-update-check",
    "--api-key",
    "--api-key-file",
    "--metrics-open",
];
const TENSORFOLD_RESERVED_FAMILIES: &[&str] = &["--capyctl-"];
/// ADR 0023 §4: the native spelling of each typed TensorFold field.
const TENSORFOLD_TYPED_OPTIONS: &[(&str, &str)] = &[
    ("--kv-dtype", "kv_cache_dtype"),
    ("--max-tokens", "tensorfold.max_tokens"),
    ("--thinking", "tensorfold.thinking"),
];
/// ADR 0023 §3: sensitive TensorFold 0.6.0 to 0.6.5 options. `--snapshot-dir` is
/// also reserved. `--drafter` is a path option exactly as SGLang's
/// `--speculative-draft-model-path` (ADR 0014 §8, ADR 0023 §5): named approval,
/// and a value inside `security.approved_paths`.
const TENSORFOLD_SENSITIVE: &[(&str, Sensitivity)] = &[
    ("--vision-urls", Sensitivity::ListenerOrEgress),
    ("--lane-kernels", Sensitivity::Code),
    (
        "--snapshot-dir",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
    (
        "--drafter",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
];
const TENSORFOLD_SHAPED: &[&str] = &[];

/// ADR 0023 §3, §5: what a TensorFold pass-through vector says about drafts:
/// whether it names a `--drafter`, and whether it turns drafts off with
/// `--no-drafts` (both in any spelling TensorFold's parser expands). capyctl
/// renders `--drafter none` only when it says neither, so TensorFold never
/// picks a drafter itself; saying both is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TensorfoldDrafts {
    pub names_drafter: bool,
    pub drafts_off: bool,
}

/// Why a TensorFold argument vector's drafter choice is refused.
pub const TENSORFOLD_DRAFTS_CONFLICT: &str =
    "`--no-drafts` turns drafts off and `--drafter` names a drafter; keep one of them";

pub fn tensorfold_drafts(args: &[String]) -> Result<TensorfoldDrafts, ProfileArgError> {
    let mut drafts = TensorfoldDrafts::default();
    for option in parse_options(args)? {
        let name = option.name.as_str();
        drafts.names_drafter |= matches_name(name, "--drafter");
        drafts.drafts_off |= name.len() > "--no-".len() && "--no-drafts".starts_with(name);
    }
    Ok(drafts)
}

/// ADR 0023 §4 (amended 2026-10-03): TensorFold's `--parallel` among `args`,
/// in any spelling its parser expands (exact, `=value`, underscores, an
/// abbreviation): `Some` with the last occurrence's value, `None` when absent.
pub fn tensorfold_parallel(args: &[String]) -> Result<Option<Option<String>>, ProfileArgError> {
    Ok(parse_options(args)?
        .into_iter()
        .rev()
        .find(|option| matches_name(&option.name, TENSORFOLD_PARALLEL))
        .map(|option| option.value))
}

/// TensorFold's option for the requests it decodes together.
pub const TENSORFOLD_PARALLEL: &str = "--parallel";

/// Whole option families reserved for SGLang: every `ssl_*` field, the
/// `modelopt_*_path` fields (quantization is out of scope, SPEC §1.2), and the
/// disaggregation, hierarchical-cache and external-cache integrations (SPEC §12).
const SGLANG_RESERVED_FAMILIES: &[&str] = &[
    "--ssl-",
    "--modelopt-",
    "--disaggregation-",
    "--hicache-storage-",
    "--lmcache-",
    "--flexkv-",
];

/// ADR 0014 §6: a configuration file would hide values from every check here
/// (SPEC §8.2), so it is refused for both engines.
const CONFIG_FILE_OPTIONS: &[&str] = &["--config"];

/// ADR 0014 §2: the native spelling of each typed field. A typed field's native
/// spelling in `extra_args` is refused; the typed field is the one way to say it.
const VLLM_TYPED_OPTIONS: &[(&str, &str)] = &[
    ("--dtype", "dtype"),
    ("--quantization", "quantization"),
    ("--kv-cache-dtype", "kv_cache_dtype"),
    ("--max-model-len", "context_length"),
    ("--max-num-seqs", "max_concurrent_requests"),
    ("--enforce-eager", "cuda_graphs"),
    ("--language-model-only", "language_model_only"),
    ("--trust-remote-code", "trust_remote_code"),
    ("--block-size", "vllm.block_size_tokens"),
    ("--max-num-batched-tokens", "vllm.max_num_batched_tokens"),
    // ADR 0014 §4 (amended 2026-10-07): typed, so the raw spelling is refused
    // with or without sleep mode; one way per setting.
    (
        "--safetensors-load-strategy",
        "vllm.safetensors_load_strategy",
    ),
];
const SGLANG_TYPED_OPTIONS: &[(&str, &str)] = &[
    ("--dtype", "dtype"),
    ("--quantization", "quantization"),
    ("--kv-cache-dtype", "kv_cache_dtype"),
    ("--context-length", "context_length"),
    ("--max-running-requests", "max_concurrent_requests"),
    ("--disable-cuda-graph", "cuda_graphs"),
    // SGLang 0.5.20 (arg_groups/fields/exec_.py): the per-phase switches and
    // backends the typed field renders are the same setting.
    ("--disable-prefill-cuda-graph", "cuda_graphs"),
    ("--disable-decode-cuda-graph", "cuda_graphs"),
    ("--cuda-graph-backend-prefill", "cuda_graphs"),
    ("--cuda-graph-backend-decode", "cuda_graphs"),
    // SGLang 0.5.20 `language_model_only` (fields/disagg.py). `--language-only`
    // is a different switch (encoder/language disaggregation), not text-only.
    ("--language-model-only", "language_model_only"),
    ("--trust-remote-code", "trust_remote_code"),
    ("--max-total-tokens", "sglang.max_total_tokens"),
    ("--chunked-prefill-size", "sglang.chunked_prefill_size"),
    ("--tokenizer-worker-num", "sglang.tokenizer_workers"),
];

/// ADR 0014 §8 (owner decision Q10): why an option needs named host approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    /// Loads remote or custom code.
    Code,
    /// Takes a filesystem path; the value must lie inside `approved_paths`.
    /// `checkpoint_exempt` paths may instead lie inside the checkpoint.
    Path { checkpoint_exempt: bool },
    /// Opens a listener or reaches off the host.
    ListenerOrEgress,
    /// A JSON configuration value, which can carry paths or endpoints no
    /// check here reads (SPEC §8.2).
    Config,
    /// vLLM `--speculative-config`: a JSON object whose keys are all in
    /// [`SPECULATIVE_CONFIG_KEYS`] and whose draft `model`, when named, lies
    /// inside `approved_paths`. Anything else is refused, so every path it
    /// can carry is checked (found live 2026-09-25: as a plain path option a
    /// JSON value could never be approved, so vLLM speculation never deployed).
    SpeculativeConfig,
}

/// The `--speculative-config` keys a deployment may set. Only `model` names a
/// path; every other key is a number or a closed word (`moe_backend` picks the
/// draft's MoE kernels, e.g. `triton` where FlashInfer would JIT-compile). A key outside this list
/// (a tokenizer, a revision, a quantization config) is refused.
pub const SPECULATIVE_CONFIG_KEYS: &[&str] = &[
    "method",
    "model",
    "num_speculative_tokens",
    "draft_tensor_parallel_size",
    "prompt_lookup_max",
    "prompt_lookup_min",
    "draft_sample_method",
    "moe_backend",
];

/// ADR 0014 §5 amendment A6: the draft model directory a launch's arguments
/// (host-fixed then the deployment's) name for `engine`: vLLM's
/// `--speculative-config` `model`, SGLang's `--speculative-draft-model-path`,
/// TensorFold's `--drafter`. The last one named wins, as in the engines'
/// parsers; only an absolute path is a directory. `None` when the arguments
/// name none (MTP heads live in the checkpoint) or do not parse.
pub fn draft_model_path(engine: Engine, args: &[String]) -> Option<String> {
    let option = match engine {
        Engine::Vllm => "--speculative-config",
        Engine::Sglang => "--speculative-draft-model-path",
        Engine::Tensorfold => "--drafter",
    };
    let mut named = None;
    for parsed in parse_options(args).ok()? {
        if !matches_name(&parsed.name, option) {
            continue;
        }
        let Some(value) = parsed.value else {
            continue;
        };
        named = match engine {
            Engine::Vllm => serde_json::from_str::<serde_json::Value>(&value)
                .ok()
                .and_then(|config| config["model"].as_str().map(str::to_owned)),
            Engine::Sglang | Engine::Tensorfold => Some(value),
        };
    }
    named.filter(|path| Path::new(path).is_absolute())
}

/// ADR 0014 amendment A15: whether SGLang arguments (host-fixed then the
/// deployment's) turn on speculative decoding, which SGLang does with
/// `--speculative-algorithm` (abbreviations and the `=` spelling included).
/// Arguments that do not parse are refused by their own validation.
pub fn sglang_speculative(args: &[String]) -> bool {
    parse_options(args).is_ok_and(|options| {
        options
            .iter()
            .any(|parsed| matches_name(&parsed.name, "--speculative-algorithm"))
    })
}

/// Whether a `--speculative-config` value is admissible under the host's
/// approved directories (ADR 0014 §8).
fn speculative_config_admitted(value: &str, approved_paths: &[PathBuf]) -> bool {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(value)
    else {
        return false;
    };
    map.iter().all(|(key, field)| {
        if !SPECULATIVE_CONFIG_KEYS.contains(&key.as_str()) {
            return false;
        }
        match (key.as_str(), field) {
            ("model", serde_json::Value::String(path)) => {
                approved_paths.iter().any(|root| path_within(path, root))
            }
            ("model", _) => false,
            (_, serde_json::Value::String(_) | serde_json::Value::Number(_)) => true,
            (_, serde_json::Value::Bool(_)) => true,
            _ => false,
        }
    })
}

const VLLM_SENSITIVE: &[(&str, Sensitivity)] = &[
    ("--worker-cls", Sensitivity::Code),
    ("--worker-extension-cls", Sensitivity::Code),
    ("--logits-processors", Sensitivity::Code),
    ("--logits-processor-pattern", Sensitivity::Code),
    ("--tool-parser-plugin", Sensitivity::Code),
    ("--reasoning-parser-plugin", Sensitivity::Code),
    (
        "--download-dir",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
    (
        "--tokenizer",
        Sensitivity::Path {
            checkpoint_exempt: true,
        },
    ),
    (
        "--chat-template",
        Sensitivity::Path {
            checkpoint_exempt: true,
        },
    ),
    (
        "--lora-modules",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
    ("--speculative-config", Sensitivity::SpeculativeConfig),
    (
        "--generation-config",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
    (
        "--allowed-local-media-path",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
    (
        "--hf-config-path",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
    ("--otlp-traces-endpoint", Sensitivity::ListenerOrEgress),
    ("--kv-transfer-config", Sensitivity::ListenerOrEgress),
    ("--kv-events-config", Sensitivity::ListenerOrEgress),
    ("--load-format", Sensitivity::ListenerOrEgress),
    ("--allowed-media-domains", Sensitivity::ListenerOrEgress),
    ("--hf-token", Sensitivity::ListenerOrEgress),
];
const SGLANG_SENSITIVE: &[(&str, Sensitivity)] = &[
    ("--enable-custom-logit-processor", Sensitivity::Code),
    (
        "--download-dir",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
    (
        "--chat-template",
        Sensitivity::Path {
            checkpoint_exempt: true,
        },
    ),
    (
        "--completion-template",
        Sensitivity::Path {
            checkpoint_exempt: true,
        },
    ),
    (
        "--lora-paths",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
    (
        "--speculative-draft-model-path",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
    (
        "--file-storage-path",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
    ("--otlp-traces-endpoint", Sensitivity::ListenerOrEgress),
    ("--tool-server", Sensitivity::ListenerOrEgress),
    ("--kv-events-config", Sensitivity::ListenerOrEgress),
    ("--load-format", Sensitivity::ListenerOrEgress),
    (
        "--remote-instance-weight-loader-seed-instance-ip",
        Sensitivity::ListenerOrEgress,
    ),
];

/// ADR 0014 open issue 5: the explicit lists above are initial. These name
/// shapes catch the obvious remainder (an extra port, a socket, an endpoint, a
/// bind address, a path or a hub token) so a new engine option of that kind
/// needs approval rather than passing as ordinary. `runtime/extra_args_policy.py`
/// applies the same shapes to the destination the engine's own parser resolves,
/// at launch, which is where an abbreviation is finally known.
const LISTENER_SUFFIXES: &[&str] = &[
    "-port",
    "-ports",
    "-host",
    "-address",
    "-addr",
    "-ip",
    "-socket",
    "-endpoint",
    "-endpoints",
    "-url",
    "-urls",
    "-token",
    "-bind",
];
const PATH_SUFFIXES: &[&str] = &["-path", "-paths", "-dir", "-directory", "-folder", "-file"];
/// SPEC §8.2: a JSON configuration value can name paths and endpoints that no
/// check here reads, so a `*-config` option needs named approval.
const CONFIG_SUFFIXES: &[&str] = &["-config"];
/// ADR 0014 §8: an option naming a class, a loader or a plugin loads code
/// (vLLM `--scheduler-cls`, `--io-processor-plugin`; SGLang
/// `--custom-weight-loader`).
const CODE_SUFFIXES: &[&str] = &["-cls", "-class", "-loader"];

/// Sensitive-shaped option names the installed parsers declare (SGLang 0.5.20
/// `arg_groups/field_order.py`, no new ones in 0.5.21; vLLM `serve`), listed so an abbreviation of
/// one is refused at deploy time. The launch-time check on the parsed
/// destination remains the authority for names not listed here.
const SGLANG_SHAPED: &[&str] = &[
    "--quantization-param-path",
    "--radix-eviction-policy-config",
    "--gated-launch-port",
    "--file-storage-path",
    "--load-publish-endpoint",
    "--otlp-traces-endpoint",
    "--export-metrics-to-file",
    "--export-metrics-to-file-dir",
    "--cuda-graph-config",
    "--uno-lora-path",
    "--speculative-draft-model-path",
    "--speculative-dspark-sps-table-path",
    "--speculative-dspark-confidence-sts-path",
    "--speculative-adaptive-config",
    "--decoupled-spec-bind-endpoint",
    "--decoupled-spec-connect-endpoints",
    "--spec-trace-dir",
    "--speculative-ngram-external-corpus-path",
    "--deepep-config",
    "--hisparse-config",
    "--mm-process-config",
    "--lora-paths",
    "--kt-weight-path",
    "--dllm-algorithm-config",
    "--encoder-urls",
    "--encoder-bootstrap-port",
    "--encoder-register-urls",
    "--pdmux-config-path",
    "--remote-instance-weight-loader-seed-instance-ip",
    "--remote-instance-weight-loader-seed-instance-service-port",
    "--remote-instance-weight-loader-send-weights-group-ports",
    "--engine-info-bootstrap-port",
    "--modelexpress-config",
    "--download-dir",
    "--decrypted-config-file",
    "--decrypted-draft-config-file",
    "--debug-tensor-dump-output-folder",
    "--debug-tensor-dump-input-file",
    "--weight-cache-socket",
    "--msprobe-dump-config",
    "--model-loader-extra-config",
    "--kv-events-config",
    "--custom-weight-loader",
];
const VLLM_SHAPED: &[&str] = &[
    "--scheduler-cls",
    "--io-processor-plugin",
    "--additional-config",
    "--allowed-local-media-path",
    "--attention-config",
    "--compilation-config",
    "--diffusion-config",
    "--download-dir",
    "--ec-transfer-config",
    "--engram-config",
    "--eplb-config",
    "--generation-config",
    "--hf-config-path",
    "--kernel-config",
    "--kv-events-config",
    "--kv-transfer-config",
    "--mm-encoder-fp8-scale-path",
    "--mm-encoder-fp8-scale-save-path",
    "--model-loader-extra-config",
    "--numa-bind",
    "--numa-bind-cpus",
    "--numa-bind-nodes",
    "--otlp-traces-endpoint",
    "--override-generation-config",
    "--pooler-config",
    "--profiler-config",
    "--quantization-config",
    "--reasoning-config",
    "--speculative-config",
    "--structured-outputs-config",
    "--watermark-config",
    "--weight-transfer-config",
];

/// Ordinary options whose full name happens to be a prefix of a sensitive one
/// (`--reasoning-parser` of `--reasoning-parser-plugin`). The parser resolves an
/// exact name before any abbreviation, so these are what they say they are.
// ADR 0023 §3: TensorFold's --vision is a prefix of --vision-urls.
const ORDINARY_EXACT: &[&str] = &["--reasoning-parser", "--vision"];

/// ADR 0014 §8, SPEC §8.2: the environment variable carrying the host's
/// approvals for sensitive extra arguments to the protected entries, which
/// gate the destinations the engine's own parser resolved
/// (`runtime/extra_args_policy.py`).
pub const EXTRA_APPROVALS_ENV: &str = "CAPYCTL_EXTRA_APPROVALS";

/// The approvals document the entries read: the approved option names, the
/// approved directories, and whether the host allows `trust_remote_code`.
pub fn extra_approvals_document(
    approved_options: &[String],
    approved_paths: &[String],
    trust_remote_code: bool,
) -> String {
    serde_json::json!({
        "options": approved_options,
        "paths": approved_paths,
        "trust_remote_code": trust_remote_code,
    })
    .to_string()
}

/// Host policy over deployment extra arguments (`security.extra_args`).
/// Owner decision Q10: allowed unless the host denies them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtraArgsPolicy {
    #[default]
    Allowed,
    Denied,
}

impl ExtraArgsPolicy {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// Why an argument list was refused. Messages name options, never values:
/// a value may be a token or a path an operator did not mean to publish.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileArgError {
    #[error("reserved option `{0}` is rendered by capyctl and cannot be passed")]
    Reserved(String),
    #[error("duplicate option `{0}`")]
    Duplicate(String),
    #[error("option `{0}` is not accepted here")]
    Unsupported(String),
    #[error("option `{0}` requires a value")]
    MissingValue(String),
    #[error("unexpected positional argument at position {0}")]
    UnexpectedArgument(usize),
    #[error("short option `{0}` is refused; spell it as a long option")]
    ShortOption(String),
    #[error("configuration-file option `{0}` would hide values from validation")]
    ConfigFile(String),
    #[error(
        "option `{option}` duplicates typed field `engine_config.{field}`; set the typed field"
    )]
    TypedField { option: String, field: String },
    #[error(
        "option `{0}` is security-sensitive; it needs the host installation to list it \
         in security.approved_options"
    )]
    Sensitive(String),
    #[error("option `{0}` names a path outside the host's security.approved_paths")]
    PathNotApproved(String),
}

/// Lowercase long-option name with `_` spelled `-`, without any `=value`.
pub fn normalize_option_name(argument: &str) -> String {
    argument
        .split_once('=')
        .map_or(argument, |(name, _)| name)
        .to_ascii_lowercase()
        .replace('_', "-")
}

fn sglang_field_option(field: &str) -> String {
    format!("--{}", field.replace('_', "-"))
}

/// Every option name capyctl reserves for `engine`, as exact names. Families are
/// matched separately by [`reserved_option`].
pub fn reserved_options(engine: Engine, sleep_mode: bool) -> Vec<String> {
    match engine {
        Engine::Vllm => VLLM_RESERVED_FLAGS
            .iter()
            .chain(if sleep_mode { VLLM_SLEEP_RESERVED } else { &[] })
            .map(|name| (*name).to_owned())
            .collect(),
        Engine::Sglang => SGLANG_RESERVED_FIELDS
            .iter()
            .map(|field| sglang_field_option(field))
            .chain(
                SGLANG_RESERVED_ALIASES
                    .iter()
                    .map(|name| (*name).to_owned()),
            )
            .collect(),
        Engine::Tensorfold => TENSORFOLD_RESERVED_FLAGS
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
    }
}

/// Whole reserved option families (prefixes) for `engine`.
pub fn reserved_families(engine: Engine) -> &'static [&'static str] {
    match engine {
        Engine::Vllm => VLLM_RESERVED_FAMILIES,
        Engine::Sglang => SGLANG_RESERVED_FAMILIES,
        Engine::Tensorfold => TENSORFOLD_RESERVED_FAMILIES,
    }
}

fn typed_options(engine: Engine) -> &'static [(&'static str, &'static str)] {
    match engine {
        Engine::Vllm => VLLM_TYPED_OPTIONS,
        Engine::Sglang => SGLANG_TYPED_OPTIONS,
        Engine::Tensorfold => TENSORFOLD_TYPED_OPTIONS,
    }
}

/// The native option a typed field renders to, for duplicate detection against
/// host-fixed profile arguments.
pub fn typed_field_option(engine: Engine, field: &str) -> Option<&'static str> {
    typed_options(engine)
        .iter()
        .find(|(_, typed)| *typed == field)
        .map(|(option, _)| *option)
}

/// Both engines' parsers accept any unambiguous prefix of a long option, so a
/// name that is a prefix of a protected name could reach it (ADR 0014 §3). An
/// abbreviation is refused whether or not it is ambiguous in the full parser.
pub(crate) fn matches_name(given: &str, protected: &str) -> bool {
    given.len() > 2 && protected.starts_with(given)
}

/// The names this option could stand for: itself, its `--no-` negation's base
/// (vLLM's boolean options accept `--no-<name>`), and a dotted JSON sub-key's
/// base (`--compilation-config.level`).
fn candidate_names(name: &str) -> Vec<String> {
    let base = name
        .split_once('.')
        .map_or(name, |(base, _)| base)
        .to_owned();
    let mut names = vec![base.clone()];
    if let Some(stripped) = base.strip_prefix("--no-") {
        names.push(format!("--{stripped}"));
    }
    names
}

fn reserved_option(engine: Engine, name: &str, sleep_mode: bool) -> bool {
    let reserved = reserved_options(engine, sleep_mode);
    candidate_names(name).iter().any(|candidate| {
        reserved
            .iter()
            .any(|protected| matches_name(candidate, protected))
            || reserved_families(engine)
                .iter()
                .any(|family| candidate.starts_with(family))
    })
}

fn config_file_option(name: &str) -> bool {
    candidate_names(name).iter().any(|candidate| {
        CONFIG_FILE_OPTIONS
            .iter()
            .any(|protected| matches_name(candidate, protected))
    })
}

fn typed_option(engine: Engine, name: &str) -> Option<(&'static str, &'static str)> {
    candidate_names(name).iter().find_map(|candidate| {
        typed_options(engine)
            .iter()
            .find(|(option, _)| matches_name(candidate, option))
            .copied()
    })
}

/// The typed option (native spelling, field) a pass-through name stands for, in
/// any spelling: abbreviated, `_` for `-`, `=value`, or `--no-` negated.
pub fn typed_option_of(engine: Engine, name: &str) -> Option<(&'static str, &'static str)> {
    typed_option(engine, &normalize_option_name(name))
}

/// The sensitivity a full option name's shape implies (ADR 0014 open issue 5).
fn shape(name: &str) -> Option<Sensitivity> {
    let parts: Vec<&str> = name.trim_start_matches('-').split('-').collect();
    if CODE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        || parts
            .iter()
            .any(|part| matches!(*part, "plugin" | "plugins"))
    {
        return Some(Sensitivity::Code);
    }
    let bind_like = parts.contains(&"bind");
    if LISTENER_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
        || bind_like
        || name.contains("remote-instance")
    {
        return Some(Sensitivity::ListenerOrEgress);
    }
    if PATH_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        return Some(Sensitivity::Path {
            checkpoint_exempt: false,
        });
    }
    if CONFIG_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        return Some(Sensitivity::Config);
    }
    None
}

/// ADR 0014 §8: whether an option needs named host approval, and why.
pub fn sensitivity(engine: Engine, name: &str) -> Option<Sensitivity> {
    let (explicit, shaped) = match engine {
        Engine::Vllm => (VLLM_SENSITIVE, VLLM_SHAPED),
        Engine::Sglang => (SGLANG_SENSITIVE, SGLANG_SHAPED),
        Engine::Tensorfold => (TENSORFOLD_SENSITIVE, TENSORFOLD_SHAPED),
    };
    for candidate in candidate_names(name) {
        let abbreviation_ok = ORDINARY_EXACT.contains(&candidate.as_str());
        if let Some((_, kind)) = explicit.iter().find(|(option, _)| {
            candidate == *option || (!abbreviation_ok && matches_name(&candidate, option))
        }) {
            return Some(*kind);
        }
        if let Some(kind) = shape(&candidate) {
            return Some(kind);
        }
        // SPEC §8.2: the parsers expand any unambiguous prefix, so an
        // abbreviation of a known sensitive name is that name.
        if let Some(kind) = shaped
            .iter()
            .find(|option| !abbreviation_ok && matches_name(&candidate, option))
            .and_then(|option| shape(option))
        {
            return Some(kind);
        }
    }
    None
}

/// One long option in an argument list, with its value when one follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedOption {
    pub name: String,
    pub value: Option<String>,
}

/// Split an ordered token list into long options and their values.
///
/// ADR 0014 §6: only long options; a value is either `--name=value` or the one
/// token that follows an option. Short options and positional tokens that do
/// not follow an option are refused. A single-dash token that parses as a
/// number is a (negative) value, e.g. SGLang's `--chunked-prefill-size -1`.
pub fn parse_options(args: &[String]) -> Result<Vec<ParsedOption>, ProfileArgError> {
    let mut options: Vec<ParsedOption> = Vec::new();
    let mut awaiting_value = false;
    for (index, token) in args.iter().enumerate() {
        if let Some(rest) = token.strip_prefix("--") {
            if rest.is_empty() || rest.starts_with('=') {
                return Err(ProfileArgError::UnexpectedArgument(index));
            }
            let name = normalize_option_name(token);
            let value = match token.split_once('=') {
                Some((_, "")) => return Err(ProfileArgError::MissingValue(name)),
                Some((_, value)) => Some(value.to_owned()),
                None => None,
            };
            awaiting_value = value.is_none();
            options.push(ParsedOption { name, value });
        } else if token.starts_with('-') && token.len() > 1 && token.parse::<f64>().is_err() {
            return Err(ProfileArgError::ShortOption(
                token.chars().take(2).collect(),
            ));
        } else if awaiting_value {
            awaiting_value = false;
            if let Some(last) = options.last_mut() {
                last.value = Some(token.clone());
            }
        } else {
            return Err(ProfileArgError::UnexpectedArgument(index));
        }
    }
    Ok(options)
}

/// A path value inside `root`, compared lexically. Relative paths and any `.`
/// or `..` component are refused so a value cannot climb out of an approval.
pub(crate) fn path_within(value: &str, root: &Path) -> bool {
    let path = Path::new(value);
    path.is_absolute()
        && root.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
        && path.starts_with(root)
}

/// What a deployment's extra arguments are checked against.
#[derive(Debug, Clone)]
pub struct ExtraArgsContext<'a> {
    pub engine: Engine,
    /// Whether capyctl renders sleep mode for this launch (ADR 0014 §4).
    pub sleep_mode: bool,
    /// The installation's `security.approved_options`, normalized.
    pub approved_options: &'a BTreeSet<String>,
    /// The installation's `security.approved_paths`.
    pub approved_paths: &'a [PathBuf],
    /// The resolved checkpoint directory, where known.
    pub checkpoint_root: Option<&'a Path>,
    /// Option names the host-fixed profile arguments already set.
    pub host_fixed: &'a BTreeSet<String>,
}

/// ADR 0014 §6: deploy-time checks over a deployment's `extra_args`: short
/// options, positional tokens, reserved names and their prefixes, duplicates,
/// typed-field spellings, configuration files, and sensitive options the host
/// has not approved. Arity and meaning are left to the engine's parser.
pub fn validate_extra_args(
    args: &[String],
    context: &ExtraArgsContext<'_>,
) -> Result<(), ProfileArgError> {
    let mut seen = BTreeSet::new();
    for option in parse_options(args)? {
        let name = option.name.clone();
        if config_file_option(&name) {
            return Err(ProfileArgError::ConfigFile(name));
        }
        if reserved_option(context.engine, &name, context.sleep_mode) {
            return Err(ProfileArgError::Reserved(name));
        }
        if let Some((native, field)) = typed_option(context.engine, &name) {
            return Err(ProfileArgError::TypedField {
                option: native.to_owned(),
                field: field.to_owned(),
            });
        }
        let base = candidate_names(&name).pop().unwrap_or_else(|| name.clone());
        if !seen.insert(base.clone()) || context.host_fixed.contains(&base) {
            return Err(ProfileArgError::Duplicate(name));
        }
        match sensitivity(context.engine, &name) {
            None => {}
            Some(Sensitivity::Path { checkpoint_exempt }) => {
                let value = option.value.as_deref().unwrap_or_default();
                let in_checkpoint = checkpoint_exempt
                    && context
                        .checkpoint_root
                        .is_some_and(|root| path_within(value, root));
                if in_checkpoint {
                    continue;
                }
                if !approved(context.approved_options, &name) {
                    return Err(ProfileArgError::Sensitive(name));
                }
                if !context
                    .approved_paths
                    .iter()
                    .any(|root| path_within(value, root))
                {
                    return Err(ProfileArgError::PathNotApproved(name));
                }
            }
            Some(Sensitivity::SpeculativeConfig) => {
                if !approved(context.approved_options, &name) {
                    return Err(ProfileArgError::Sensitive(name));
                }
                let value = option.value.as_deref().unwrap_or_default();
                if !speculative_config_admitted(value, context.approved_paths) {
                    return Err(ProfileArgError::PathNotApproved(name));
                }
            }
            Some(Sensitivity::Code | Sensitivity::ListenerOrEgress | Sensitivity::Config)
                if !approved(context.approved_options, &name) =>
            {
                return Err(ProfileArgError::Sensitive(name));
            }
            Some(Sensitivity::Code | Sensitivity::ListenerOrEgress | Sensitivity::Config) => {}
        }
    }
    Ok(())
}

fn approved(approved_options: &BTreeSet<String>, name: &str) -> bool {
    candidate_names(name)
        .iter()
        .any(|candidate| approved_options.contains(candidate))
}

/// The option names an argument list sets (base names, `--no-` folded).
pub fn option_names(args: &[String]) -> Result<BTreeSet<String>, ProfileArgError> {
    Ok(parse_options(args)?
        .into_iter()
        .filter_map(|option| candidate_names(&option.name).pop())
        .collect())
}

/// Host-fixed profile arguments (ADR 0014 §1). The host operator writes these,
/// so sensitive options are theirs to pass; reserved names, configuration files
/// and malformed lists are still refused. SGLang's protected entry takes no
/// argument vector, so an SGLang profile carries none.
pub fn validate_profile_args(
    engine: Engine,
    args: &[String],
    sleep_mode: bool,
) -> Result<(), ProfileArgError> {
    if engine == Engine::Sglang {
        return args.first().map_or(Ok(()), |arg| {
            Err(ProfileArgError::Unsupported(normalize_option_name(arg)))
        });
    }
    validate_rendered_args(engine, args, sleep_mode)
}

/// The launch-time subset: shape, reserved names, configuration files and
/// duplicates over the complete pass-through vector (host-fixed plus extra
/// arguments). Used again by the adapter right before rendering.
pub fn validate_rendered_args(
    engine: Engine,
    args: &[String],
    sleep_mode: bool,
) -> Result<(), ProfileArgError> {
    let mut seen = BTreeSet::new();
    for option in parse_options(args)? {
        let name = option.name;
        if config_file_option(&name) {
            return Err(ProfileArgError::ConfigFile(name));
        }
        if reserved_option(engine, &name, sleep_mode) {
            return Err(ProfileArgError::Reserved(name));
        }
        let base = candidate_names(&name).pop().unwrap_or_else(|| name.clone());
        if !seen.insert(base) {
            return Err(ProfileArgError::Duplicate(name));
        }
    }
    Ok(())
}

/// ADR 0028 §2.1: the profile half of engine environment resolution. The
/// error is the closed reason code or the detail naming the entry.
pub fn validate_profile_env(env: &BTreeMap<String, String>) -> Result<(), String> {
    crate::engine_env::resolve_engine_env(
        env,
        &crate::engine_env::ApprovedEnv::default(),
        &BTreeMap::new(),
    )
    .map(|_| ())
    .map_err(|refusal| refusal.code())
}

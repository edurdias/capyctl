//! ADR 0038 §3, §4: one `trtllm-serve` command, the configuration file
//! CapyCTL writes beside it and the per-launch directory both live in. No
//! shell and no key: the engine listens on loopback without one, CapyCTL's
//! routed path is the only way in, and every option and LLM API key CapyCTL
//! owns is rendered here once.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use capyctl_config::engine_policy::{
    normalize_option_name, parse_options, validate_rendered_args, Engine, ProfileArgError,
};
use capyctl_config::tensorrt_llm::{
    is_reserved, llm_api_document, option_key, refused_env_name, served_name_refusal, version_line,
    LlmApiInputs, BACKEND, CONFIG_FILE, FREE_GPU_MEMORY_FRACTION, LOG_LEVEL, MODEL_DIR,
    PROCESS_ENV,
};

use crate::traits::RenderedCommand;

/// Every variable a TensorRT-LLM engine may start with (SPEC §13.3 / T21,
/// ADR 0038 §3), besides [`PROCESS_ENV`] and the resolved engine
/// environment's own names.
pub const ENGINE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "CUDA_VISIBLE_DEVICES",
    "CUDA_DEVICE_ORDER",
    "CUDA_HOME",
    "TMPDIR",
    "CAPYCTL_ENGINE_LOG",
    // ADR 0038 §10: the opt-outs, under the host's `engine_telemetry: off`
    // only (`engine_telemetry::TENSORRT_LLM`).
    "DO_NOT_TRACK",
    "TRTLLM_NO_USAGE_STATS",
    "TELEMETRY_DISABLED",
];

/// Variables taken from the agent's own environment: CapyCTL's `HOME`
/// (where the engine's kernel caches live) and the device choice.
const PASS_THROUGH: &[&str] = &["HOME", "CUDA_VISIBLE_DEVICES"];

/// ADR 0038 §3: the listener is loopback only.
const LOOPBACK: &str = "127.0.0.1";
/// ADR 0038 §13: one GPU, one rank, so tensor and pipeline parallelism and
/// the GPUs per node are each 1.
const ONE: &str = "1";
/// ADR 0038 §3: the launch directory's private temporary directory
/// (`TMPDIR`), where the stats socket and every temporary file go.
pub const TMP_DIR: &str = "tmp";

#[derive(Clone, PartialEq, Eq, Default)]
pub struct PlanInputTensorrtLlm {
    /// `<env>/bin/trtllm-serve`.
    pub engine_bin: String,
    pub engine_path_extra: Option<String>,
    /// ADR 0038 §3: the toolkit the version's TR1 recorded (`CUDA_HOME`).
    pub cuda_home: Option<String>,
    /// ADR 0028 §2.1: the resolved engine environment (profile and deployment).
    pub build_env: BTreeMap<String, String>,
    /// ADR 0038 §2, §5: the profile's checked version, which `/version` must
    /// report and whose line's tables render the file (§2a).
    pub version: String,
    /// ADR 0038 §3: the name the per-launch link carries, which the engine
    /// serves.
    pub served_model_name: String,
    /// ADR 0038 §3: the verified checkpoint directory the link points at.
    pub checkpoint_dir: String,
    pub port: u16,
    /// ADR 0038 §3: `--max_seq_len`, each request's window.
    pub max_seq_len: u32,
    /// ADR 0038 §3: `--max_batch_size`.
    pub max_batch_size: u32,
    /// ADR 0038 §4: `kv_cache_config.max_tokens`, from the grant.
    pub max_tokens: u64,
    /// ADR 0038 §4: the KV block the pool is built in, the slack the built
    /// pool may exceed `max_tokens` by.
    pub tokens_per_block: u32,
    pub dtype: Option<String>,
    pub kv_cache_dtype: Option<String>,
    pub cuda_graphs: Option<bool>,
    /// ADR 0038 §3: the deployment's `llm_api_options`, merged under
    /// CapyCTL's keys.
    pub llm_api_options: serde_json::Map<String, serde_json::Value>,
    /// ADR 0038 §14: `--reasoning_parser` and `--tool_parser`.
    pub reasoning_parser: Option<String>,
    pub tool_parser: Option<String>,
    pub trust_remote_code: bool,
    /// Host-fixed arguments.
    pub engine_args: Vec<String>,
    /// The deployment's ordinary extra arguments.
    pub extra_args: Vec<String>,
    /// ADR 0038 §3: the CapyCTL-owned directory of this launch (0700),
    /// under the role's runtime directory.
    pub launch_dir: String,
    pub engine_log: Option<String>,
    /// ADR 0019 §6: the GPU the engine child's namespace is narrowed to;
    /// empty keeps the agent's own.
    pub cuda_namespaces: Vec<capyctl_config::effective::CudaNamespace>,
}

/// ADR 0028 §2.1: `build_env` values may hold tokens, so Debug prints its
/// names only.
impl std::fmt::Debug for PlanInputTensorrtLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanInputTensorrtLlm")
            .field("engine_bin", &self.engine_bin)
            .field("engine_path_extra", &self.engine_path_extra)
            .field("cuda_home", &self.cuda_home)
            .field("build_env", &self.build_env.keys().collect::<Vec<_>>())
            .field("version", &self.version)
            .field("served_model_name", &self.served_model_name)
            .field("checkpoint_dir", &self.checkpoint_dir)
            .field("port", &self.port)
            .field("max_seq_len", &self.max_seq_len)
            .field("max_batch_size", &self.max_batch_size)
            .field("max_tokens", &self.max_tokens)
            .field("tokens_per_block", &self.tokens_per_block)
            .field("dtype", &self.dtype)
            .field("kv_cache_dtype", &self.kv_cache_dtype)
            .field("cuda_graphs", &self.cuda_graphs)
            .field("llm_api_options", &self.llm_api_options)
            .field("reasoning_parser", &self.reasoning_parser)
            .field("tool_parser", &self.tool_parser)
            .field("trust_remote_code", &self.trust_remote_code)
            .field("engine_args", &self.engine_args)
            .field("extra_args", &self.extra_args)
            .field("launch_dir", &self.launch_dir)
            .field("engine_log", &self.engine_log)
            .field("cuda_namespaces", &self.cuda_namespaces)
            .finish()
    }
}

impl PlanInputTensorrtLlm {
    /// ADR 0038 §3: `<launch dir>/model/<served name>`, the model argument.
    pub fn model_link(&self) -> PathBuf {
        Path::new(&self.launch_dir)
            .join(MODEL_DIR)
            .join(&self.served_model_name)
    }

    /// ADR 0038 §3: `<launch dir>/llm_api_options.yaml`, the `--config` file.
    pub fn config_file(&self) -> PathBuf {
        Path::new(&self.launch_dir).join(CONFIG_FILE)
    }

    /// ADR 0038 §3: `<launch dir>/tmp`, the engine's `TMPDIR`.
    pub fn tmp_dir(&self) -> PathBuf {
        Path::new(&self.launch_dir).join(TMP_DIR)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TensorrtLlmArgsError {
    #[error("reserved option {0} conflicts with a capyctl-owned setting")]
    Reserved(String),
    #[error(
        "option {option} is rendered by capyctl from engine_config.{field}; set that field instead"
    )]
    ReservedField { option: String, field: String },
    #[error("duplicate option {0}")]
    Duplicate(String),
    #[error("malformed engine arguments: {0}")]
    Malformed(String),
    #[error("a TensorRT-LLM launch needs a context length, a batch size and a KV pool of at least one token")]
    NoContext,
    /// ADR 0038 §3: the reason leads with `served_name_unsupported`.
    #[error("{0}")]
    ServedName(String),
    /// ADR 0038 §3: the deployment's `llm_api_options` names a reserved key.
    #[error("engine_config.tensorrt-llm.llm_api_options: {0}")]
    Options(String),
    #[error(
        "the rendered command holds reserved option {0} other than once, where capyctl renders it"
    )]
    RenderedReserved(String),
    #[error("the launch directory {0}")]
    LaunchDir(String),
}

fn pass_through_error(error: ProfileArgError) -> TensorrtLlmArgsError {
    match error {
        ProfileArgError::Reserved(name) => TensorrtLlmArgsError::Reserved(name),
        ProfileArgError::ReservedField { option, field } => {
            TensorrtLlmArgsError::ReservedField { option, field }
        }
        ProfileArgError::Duplicate(name) => TensorrtLlmArgsError::Duplicate(name),
        other => TensorrtLlmArgsError::Malformed(other.to_string()),
    }
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// ADR 0038 §3: the options CapyCTL renders, in this order, after `serve`
/// and the model link, before the host-fixed and the deployment's arguments.
fn reserved_block(input: &PlanInputTensorrtLlm) -> Vec<String> {
    let mut argv: Vec<String> = [
        "--host",
        LOOPBACK,
        "--port",
        &input.port.to_string(),
        "--backend",
        BACKEND,
        "--max_seq_len",
        &input.max_seq_len.to_string(),
        "--max_batch_size",
        &input.max_batch_size.to_string(),
        "--tp_size",
        ONE,
        "--pp_size",
        ONE,
        "--gpus_per_node",
        ONE,
        // ADR 0038 §4: only a ceiling; `max_tokens` in the file sizes the pool.
        "--kv_cache_free_gpu_memory_fraction",
        &FREE_GPU_MEMORY_FRACTION.to_string(),
        "--log_level",
        LOG_LEVEL,
        "--config",
        &display(&input.config_file()),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(parser) = &input.reasoning_parser {
        argv.extend(["--reasoning_parser".to_owned(), parser.clone()]);
    }
    if let Some(parser) = &input.tool_parser {
        argv.extend(["--tool_parser".to_owned(), parser.clone()]);
    }
    if input.trust_remote_code {
        argv.push("--trust_remote_code".to_owned());
    }
    argv
}

/// ADR 0038 §3: the command for one launch. The rendered environment holds
/// `TMPDIR` and the GPU pin; [`engine_environment`] closes it.
pub fn render_command(
    input: &PlanInputTensorrtLlm,
) -> Result<RenderedCommand, TensorrtLlmArgsError> {
    // ADR 0038 §3 (owner decision 2026-10-27, question 3): the served name is
    // the link's name, one path component; resolution refused any other.
    if let Some(reason) = served_name_refusal(&input.served_model_name) {
        return Err(TensorrtLlmArgsError::ServedName(reason));
    }
    if input.max_seq_len == 0 || input.max_batch_size == 0 || input.max_tokens == 0 {
        return Err(TensorrtLlmArgsError::NoContext);
    }
    // ADR 0038 §3: no protected entry parses these, so the exact-name policy
    // is the check of the complete pass-through vector, again at render.
    let pass_through: Vec<String> = input
        .engine_args
        .iter()
        .chain(&input.extra_args)
        .cloned()
        .collect();
    validate_rendered_args(Engine::TensorrtLlm, &pass_through, false)
        .map_err(pass_through_error)?;
    // `serve` is always named, so no model path is read as a subcommand.
    let mut argv = vec![
        input.engine_bin.clone(),
        "serve".to_owned(),
        display(&input.model_link()),
    ];
    argv.extend(reserved_block(input));
    argv.extend(pass_through);
    recheck_rendered(&argv)?;
    let mut env = BTreeMap::from([("TMPDIR".to_owned(), display(&input.tmp_dir()))]);
    // ADR 0019 §6: an empty namespace list keeps the agent's own devices.
    if !input.cuda_namespaces.is_empty() {
        for (name, value) in
            capyctl_config::effective::CudaNamespace::environment_of(&input.cuda_namespaces)
        {
            env.insert(name.into(), value);
        }
    }
    Ok(RenderedCommand { argv, env })
}

/// The reserved options [`reserved_block`] renders on every launch, by the
/// first spelling of their row in `RESERVED_RENDERED`.
const RENDERED_ONCE: &[&str] = &[
    "--host",
    "--port",
    "--backend",
    "--max_seq_len",
    "--max_batch_size",
    "--tensor_parallel_size",
    "--pipeline_parallel_size",
    "--gpus_per_node",
    "--free_gpu_memory_fraction",
    "--config",
    "--log_level",
];
/// Rendered when the plan names them.
const RENDERED_AT_MOST_ONCE: &[&str] =
    &["--reasoning_parser", "--tool_parser", "--trust_remote_code"];

/// ADR 0038 §3, SPEC §8.2: the rendered-argument recheck. There is no
/// in-process parser to ask, so the final vector (`argv[0]` the binary) is
/// read as Click reads it: `serve` and the model link are the only
/// positional tokens; every option CapyCTL renders appears exactly once in
/// any spelling (the parsers and `--trust_remote_code` at most once); the
/// listener is loopback and every parallel size 1; no other reserved option
/// and no reserved `--name=value` spelling appears at all.
pub fn recheck_rendered(argv: &[String]) -> Result<(), TensorrtLlmArgsError> {
    if argv.get(1).map(String::as_str) != Some("serve") {
        return Err(TensorrtLlmArgsError::RenderedReserved("serve".into()));
    }
    if argv.get(2).is_none_or(|model| model.starts_with('-')) {
        return Err(TensorrtLlmArgsError::RenderedReserved(
            "the model argument".into(),
        ));
    }
    let args = argv.get(3..).unwrap_or_default();
    if let Some(token) = args.iter().find(|token| {
        token.starts_with("--") && token.contains('=') && is_reserved(&normalize_option_name(token))
    }) {
        return Err(TensorrtLlmArgsError::RenderedReserved(token.clone()));
    }
    let options =
        parse_options(args).map_err(|error| TensorrtLlmArgsError::Malformed(error.to_string()))?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for option in &options {
        if !is_reserved(&option.name) {
            continue;
        }
        let key = option_key(&option.name);
        if !RENDERED_ONCE
            .iter()
            .chain(RENDERED_AT_MOST_ONCE)
            .any(|head| option_key(head) == key)
        {
            return Err(TensorrtLlmArgsError::RenderedReserved(option.name.clone()));
        }
        let expected = if key == option_key("--host") {
            Some(LOOPBACK)
        } else if [
            "--tensor_parallel_size",
            "--pipeline_parallel_size",
            "--gpus_per_node",
        ]
        .iter()
        .any(|head| option_key(head) == key)
        {
            Some(ONE)
        } else {
            None
        };
        if expected.is_some_and(|value| option.value.as_deref() != Some(value)) {
            return Err(TensorrtLlmArgsError::RenderedReserved(option.name.clone()));
        }
        *counts.entry(key).or_default() += 1;
    }
    let count = |head: &str| counts.get(&option_key(head)).copied().unwrap_or(0);
    if let Some(head) = RENDERED_ONCE.iter().find(|head| count(head) != 1) {
        return Err(TensorrtLlmArgsError::RenderedReserved((*head).to_owned()));
    }
    if let Some(head) = RENDERED_AT_MOST_ONCE.iter().find(|head| count(head) > 1) {
        return Err(TensorrtLlmArgsError::RenderedReserved((*head).to_owned()));
    }
    Ok(())
}

/// ADR 0038 §3: the configuration file's text for one launch, rendered with
/// the tables of the plan's version line (§2a). A deployment mapping that
/// names a reserved key is refused here again.
pub fn render_llm_api_yaml(input: &PlanInputTensorrtLlm) -> Result<String, TensorrtLlmArgsError> {
    let document = llm_api_document(&LlmApiInputs {
        line: version_line(&input.version),
        max_tokens: input.max_tokens,
        kv_cache_dtype: input.kv_cache_dtype.as_deref(),
        dtype: input.dtype.as_deref(),
        cuda_graphs: input.cuda_graphs,
        options: &input.llm_api_options,
    })
    .map_err(TensorrtLlmArgsError::Options)?;
    Ok(capyctl_config::yaml_emit::to_block_yaml(&document))
}

/// A directory created by this launch alone, mode 0700 whatever the umask.
fn private_dir(path: &Path) -> Result<(), TensorrtLlmArgsError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let failed = |error: std::io::Error| {
        TensorrtLlmArgsError::LaunchDir(format!("{}: {error}", path.display()))
    };
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(failed)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(failed)
}

/// ADR 0038 §3: create this launch's directory before the engine starts:
/// the directory itself (0700, new: a directory left by another launch is
/// refused, never reused), `tmp/` (0700), `model/<served name>` linking the
/// verified checkpoint directory, and the configuration file (0600, written
/// once). Nothing in it exists before this call.
pub fn prepare_launch_dir(input: &PlanInputTensorrtLlm) -> Result<(), TensorrtLlmArgsError> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if let Some(reason) = served_name_refusal(&input.served_model_name) {
        return Err(TensorrtLlmArgsError::ServedName(reason));
    }
    let yaml = render_llm_api_yaml(input)?;
    let dir = Path::new(&input.launch_dir);
    if !dir.is_absolute() {
        return Err(TensorrtLlmArgsError::LaunchDir(format!(
            "{} is not an absolute path",
            dir.display()
        )));
    }
    let checkpoint = Path::new(&input.checkpoint_dir);
    if !checkpoint.is_absolute() || !checkpoint.is_dir() {
        return Err(TensorrtLlmArgsError::LaunchDir(format!(
            "cannot link the checkpoint {}: it is not an absolute directory",
            checkpoint.display()
        )));
    }
    if let Some(parent) = dir.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| {
                TensorrtLlmArgsError::LaunchDir(format!("{}: {error}", parent.display()))
            })?;
    }
    private_dir(dir)?;
    private_dir(&input.tmp_dir())?;
    let model_dir = dir.join(MODEL_DIR);
    private_dir(&model_dir)?;
    let link = input.model_link();
    std::os::unix::fs::symlink(checkpoint, &link)
        .map_err(|error| TensorrtLlmArgsError::LaunchDir(format!("{}: {error}", link.display())))?;
    let path = input.config_file();
    let failed = |error: std::io::Error| {
        TensorrtLlmArgsError::LaunchDir(format!("{}: {error}", path.display()))
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(failed)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(failed)?;
    file.write_all(yaml.as_bytes()).map_err(failed)?;
    file.sync_all().map_err(failed)
}

/// Whether a resolved engine-environment name may reach the engine: never a
/// name TensorRT-LLM's policy refuses (ADR 0038 §3), nor one CapyCTL owns.
fn passed_from_build_env(name: &str) -> bool {
    !refused_env_name(name) && !capyctl_config::engine_env::is_owned(name)
}

/// The closed environment one launch starts with (SPEC §13.3, ADR 0038 §3):
/// CapyCTL's `HOME` and the device choice from the agent, the resolved
/// engine environment without the names the policy refuses, the rendered
/// `TMPDIR` and GPU pin, [`PROCESS_ENV`] (one process, MPI pinned, no
/// conversation store, offline under every telemetry setting), the
/// profile's `CUDA_HOME`, the closed PATH, the log metadata and, under the
/// host's `engine_telemetry: off`, the usage-reporting opt-outs.
pub fn engine_environment(
    rendered: &BTreeMap<String, String>,
    plan: &PlanInputTensorrtLlm,
    inherited: &dyn Fn(&str) -> Option<String>,
    telemetry: crate::engine_telemetry::EngineTelemetry,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for name in PASS_THROUGH {
        if let Some(value) = inherited(name) {
            env.insert((*name).to_owned(), value);
        }
    }
    // ADR 0028 §2.1: the resolved engine env comes before everything CapyCTL
    // renders, so a rendered value is never replaced.
    env.extend(
        plan.build_env
            .iter()
            .filter(|(name, _)| passed_from_build_env(name))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    env.extend(rendered.iter().map(|(k, v)| (k.clone(), v.clone())));
    // ADR 0038 §9, §10: on every launch, whatever the telemetry setting.
    for (name, value) in PROCESS_ENV {
        env.insert((*name).to_owned(), (*value).to_owned());
    }
    if let Some(cuda_home) = &plan.cuda_home {
        env.insert("CUDA_HOME".into(), cuda_home.clone());
    }
    env.insert(
        "PATH".into(),
        crate::engine_env::tool_path(
            plan.engine_path_extra.as_deref(),
            plan.cuda_home.as_deref(),
            crate::engine_env::SYSTEM_PATH,
        ),
    );
    if let Some(log) = &plan.engine_log {
        env.insert("CAPYCTL_ENGINE_LOG".into(), log.clone());
    }
    crate::engine_telemetry::apply(&mut env, crate::engine_telemetry::TENSORRT_LLM, telemetry);
    env.retain(|name, _| {
        ENGINE_ENV_ALLOWLIST.contains(&name.as_str())
            || PROCESS_ENV.iter().any(|(owned, _)| owned == name)
            || (plan.build_env.contains_key(name) && passed_from_build_env(name))
    });
    env
}

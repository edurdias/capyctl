//! ADR 0029 §4–§6: one `llama-server` command from a resolved plan. No shell,
//! no key: the engine listens on loopback without one, CapyCTL's routed path
//! is the only way in, and every option CapyCTL owns is rendered here once.
use std::collections::BTreeMap;

use capyctl_config::engine_policy::{
    parse_options, validate_rendered_args, Engine, ProfileArgError,
};
use capyctl_config::llamacpp::{
    is_reserved, refused_env_name, slot_pool_tokens, CACHE_TYPES, RESERVED_RENDERED,
};
use capyctl_domain::launch::LlamacppGpuLayers;

use crate::traits::RenderedCommand;

/// Every variable a llama.cpp engine may start with (SPEC §13.3 / T21), besides
/// the resolved engine environment's own names.
pub const ENGINE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "CUDA_VISIBLE_DEVICES",
    "CUDA_DEVICE_ORDER",
    "XDG_CONFIG_HOME",
    "LLAMA_CACHE",
    "CAPYCTL_ENGINE_LOG",
];
/// Variables taken from the agent's own environment. `HOME` is not one:
/// llama-server would find its user configuration and cache through it
/// (ADR 0029 §6), and both are pointed at CapyCTL's own directories.
const PASS_THROUGH: &[&str] = &["CUDA_VISIBLE_DEVICES"];

/// ADR 0029 §4: the listener is loopback only.
const LOOPBACK: &str = "127.0.0.1";
/// ADR 0029 §4: a web page from another origin cannot read the engine's answers.
const CORS_ORIGINS: &str = "localhost";

#[derive(Clone, PartialEq, Eq, Default)]
pub struct PlanInputLlamacpp {
    pub engine_bin: String,
    pub engine_path_extra: Option<String>,
    pub cuda_home: Option<String>,
    /// ADR 0028 §2.1: the resolved engine environment (profile and deployment).
    pub build_env: BTreeMap<String, String>,
    /// ADR 0029 §9: the GGUF rendered as `--model` (absolute; the first
    /// shard of a split model).
    pub model_file: String,
    /// ADR 0029 §9: the multimodal projector (`--mmproj`), absolute.
    pub mmproj_file: Option<String>,
    pub served_model_name: String,
    pub port: u16,
    /// ADR 0029 §5: each request's window.
    pub context_length: u32,
    /// ADR 0029 §5: the slot count (`--parallel`).
    pub slots: u32,
    pub n_gpu_layers: LlamacppGpuLayers,
    /// ADR 0029 §5: one cache type for keys and values.
    pub cache_type: String,
    pub engine_args: Vec<String>,
    pub extra_args: Vec<String>,
    /// ADR 0029 §6: CapyCTL's private, empty `XDG_CONFIG_HOME`.
    pub config_dir: String,
    /// ADR 0029 §6: CapyCTL's private `LLAMA_CACHE`.
    pub cache_dir: String,
    pub engine_log: Option<String>,
    pub cuda_namespace: Option<capyctl_config::effective::CudaNamespace>,
}

/// ADR 0028 §2.1: `build_env` values may hold tokens, so Debug prints its names only.
impl std::fmt::Debug for PlanInputLlamacpp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanInputLlamacpp")
            .field("engine_bin", &self.engine_bin)
            .field("engine_path_extra", &self.engine_path_extra)
            .field("cuda_home", &self.cuda_home)
            .field("build_env", &self.build_env.keys().collect::<Vec<_>>())
            .field("model_file", &self.model_file)
            .field("mmproj_file", &self.mmproj_file)
            .field("served_model_name", &self.served_model_name)
            .field("port", &self.port)
            .field("context_length", &self.context_length)
            .field("slots", &self.slots)
            .field("n_gpu_layers", &self.n_gpu_layers)
            .field("cache_type", &self.cache_type)
            .field("engine_args", &self.engine_args)
            .field("extra_args", &self.extra_args)
            .field("config_dir", &self.config_dir)
            .field("cache_dir", &self.cache_dir)
            .field("engine_log", &self.engine_log)
            .field("cuda_namespace", &self.cuda_namespace)
            .finish()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LlamacppArgsError {
    #[error("reserved option {0} conflicts with a capyctl-owned setting")]
    Reserved(String),
    #[error("duplicate option {0}")]
    Duplicate(String),
    #[error("malformed engine arguments: {0}")]
    Malformed(String),
    #[error("a llama.cpp launch needs a context length and at least one slot that fit llama.cpp's context")]
    NoContext,
    #[error("{0} is not one of llama.cpp's cache types")]
    CacheType(String),
    #[error(
        "the rendered command holds reserved option {0} other than once, where capyctl renders it"
    )]
    RenderedReserved(String),
    #[error("llama-server splits --alias at commas and trims spaces, so it cannot serve the route {0:?}; name the route without them")]
    ServedName(String),
}

/// ADR 0029 §4–§6: the options CapyCTL renders, in this order, before the
/// host-fixed and the deployment's arguments.
fn reserved_block(input: &PlanInputLlamacpp, ctx_size: u32) -> Vec<String> {
    let mut argv: Vec<String> = [
        "--host",
        LOOPBACK,
        "--port",
        &input.port.to_string(),
        "--model",
        &input.model_file,
        "--alias",
        &input.served_model_name,
        // ADR 0029 §5: every slot holds the declared window, allocated at start.
        "--ctx-size",
        &ctx_size.to_string(),
        "--parallel",
        &input.slots.to_string(),
        "--no-kv-unified",
        "--gpu-layers",
        &input.n_gpu_layers.argument(),
        "--cache-type-k",
        &input.cache_type,
        "--cache-type-v",
        &input.cache_type,
        // ADR 0029 §6: nothing adjusted to free memory after the grant.
        "--fit",
        "off",
        // SPEC §12: no engine-owned host cache until designed.
        "--cache-ram",
        "0",
        "--no-context-shift",
        // ADR 0029 §10, §11: the idle gate, load reports and readiness read these.
        "--metrics",
        "--slots",
        // ADR 0008: CapyCTL materializes checkpoints; the engine fetches nothing.
        "--offline",
        // ADR 0029 §4.
        "--no-webui",
        "--cors-origins",
        CORS_ORIGINS,
        "--no-cors-credentials",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(projector) = &input.mmproj_file {
        argv.extend(["--mmproj".to_owned(), projector.clone()]);
    }
    argv
}

fn pass_through_error(error: ProfileArgError) -> LlamacppArgsError {
    match error {
        ProfileArgError::Reserved(name) | ProfileArgError::ReservedField { option: name, .. } => {
            LlamacppArgsError::Reserved(name)
        }
        ProfileArgError::Duplicate(name) => LlamacppArgsError::Duplicate(name),
        other => LlamacppArgsError::Malformed(other.to_string()),
    }
}

pub fn render_command(input: &PlanInputLlamacpp) -> Result<RenderedCommand, LlamacppArgsError> {
    let ctx_size =
        slot_pool_tokens(input.context_length, input.slots).ok_or(LlamacppArgsError::NoContext)?;
    if !CACHE_TYPES.contains(&input.cache_type.as_str()) {
        return Err(LlamacppArgsError::CacheType(input.cache_type.clone()));
    }
    // ADR 0029 §10: readiness waits for `/v1/models` to list the served name
    // as rendered; llama-server would split it at commas and trim it
    // (`common/arg.cpp`, `--alias`), so such a name could never be listed.
    let served = &input.served_model_name;
    if served.is_empty() || served.contains(',') || served.trim() != served {
        return Err(LlamacppArgsError::ServedName(served.clone()));
    }
    // ADR 0029 §6: no protected entry parses these, so the exact-name policy
    // is the check of the complete pass-through vector, again at render.
    let pass_through: Vec<String> = input
        .engine_args
        .iter()
        .chain(&input.extra_args)
        .cloned()
        .collect();
    validate_rendered_args(Engine::Llamacpp, &pass_through, false).map_err(pass_through_error)?;
    let mut argv = vec![input.engine_bin.clone()];
    argv.extend(reserved_block(input, ctx_size));
    argv.extend(pass_through);
    recheck_rendered(&argv)?;
    let mut env = BTreeMap::from([
        ("XDG_CONFIG_HOME".to_owned(), input.config_dir.clone()),
        ("LLAMA_CACHE".to_owned(), input.cache_dir.clone()),
    ]);
    if let Some(namespace) = &input.cuda_namespace {
        for (name, value) in namespace.environment() {
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
    "--model",
    "--alias",
    "--ctx-size",
    "--parallel",
    "--kv-unified",
    "--gpu-layers",
    "--cache-type-k",
    "--cache-type-v",
    "--fit",
    "--cache-ram",
    "--context-shift",
    "--metrics",
    "--slots",
    "--offline",
    "--ui",
    "--cors-origins",
    "--cors-credentials",
];
/// Rendered when the deployment names a projector.
const RENDERED_AT_MOST_ONCE: &str = "--mmproj";

/// ADR 0029 §6, SPEC §8.2: the rendered-argument recheck. There is no
/// in-process parser to ask, so the final vector (`argv[0]` the binary) is
/// read as llama-server reads it: every option CapyCTL renders appears
/// exactly once (`--mmproj` at most once), no other reserved option and no
/// `--name=value` spelling appears at all.
pub fn recheck_rendered(argv: &[String]) -> Result<(), LlamacppArgsError> {
    let args = argv.get(1..).unwrap_or_default();
    if let Some(token) = args
        .iter()
        .find(|token| token.starts_with("--") && token.contains('='))
    {
        return Err(LlamacppArgsError::RenderedReserved(token.clone()));
    }
    let options =
        parse_options(args).map_err(|error| LlamacppArgsError::Malformed(error.to_string()))?;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for option in &options {
        if !is_reserved(&option.name) {
            continue;
        }
        let rendered = RESERVED_RENDERED
            .iter()
            .find(|row| row.contains(&option.name.as_str()))
            .map(|row| row[0])
            .filter(|head| RENDERED_ONCE.contains(head) || *head == RENDERED_AT_MOST_ONCE);
        let Some(head) = rendered else {
            return Err(LlamacppArgsError::RenderedReserved(option.name.clone()));
        };
        *counts.entry(head).or_default() += 1;
    }
    let count = |head: &str| counts.get(head).copied().unwrap_or(0);
    if let Some(head) = RENDERED_ONCE.iter().find(|head| count(head) != 1) {
        return Err(LlamacppArgsError::RenderedReserved((*head).to_owned()));
    }
    if count(RENDERED_AT_MOST_ONCE) > 1 {
        return Err(LlamacppArgsError::RenderedReserved(
            RENDERED_AT_MOST_ONCE.to_owned(),
        ));
    }
    Ok(())
}

/// The closed environment one launch starts with (SPEC §13.3, ADR 0029 §6):
/// the rendered variables, the closed PATH, the resolved engine environment
/// and `CUDA_VISIBLE_DEVICES` from the agent. No `LLAMA_*` variable reaches the
/// engine but the rendered `LLAMA_CACHE`, and `XDG_CONFIG_HOME` is always
/// CapyCTL's.
pub fn engine_environment(
    rendered: &BTreeMap<String, String>,
    plan: &PlanInputLlamacpp,
    inherited: &dyn Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for name in PASS_THROUGH {
        if let Some(value) = inherited(name) {
            env.insert((*name).to_owned(), value);
        }
    }
    // ADR 0028 §2.1: the resolved engine env comes before everything CapyCTL
    // renders, so a rendered value is never replaced. Resolution refused the
    // names llama-server reads in place of a reserved option, the listener,
    // the device choice or the directories (ADR 0029 §6); they are dropped
    // here again.
    env.extend(
        plan.build_env
            .iter()
            .filter(|(name, _)| !refused_env_name(name))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    env.extend(rendered.iter().map(|(k, v)| (k.clone(), v.clone())));
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
    env.retain(|name, _| {
        ENGINE_ENV_ALLOWLIST.contains(&name.as_str())
            || (plan.build_env.contains_key(name) && !refused_env_name(name))
    });
    env
}

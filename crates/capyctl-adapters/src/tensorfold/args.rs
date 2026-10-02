//! ADR 0023 §3: one TensorFold `serve` command from a resolved plan. No shell,
//! no key: TensorFold has none, and capyctl's routed path is the only way in.
use std::collections::BTreeMap;

use capyctl_config::engine_policy::{
    parse_options, tensorfold_drafts, typed_option_of, validate_rendered_args, Engine,
    ProfileArgError, TENSORFOLD_DRAFTS_CONFLICT,
};

use crate::traits::RenderedCommand;

/// Every variable a TensorFold engine may start with (SPEC §13.3 / T21).
pub const ENGINE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "CUDA_VISIBLE_DEVICES",
    "CUDA_DEVICE_ORDER",
    "HF_HUB_OFFLINE",
    "TRANSFORMERS_OFFLINE",
    "TENSORFOLD_NO_UPDATE_CHECK",
    "TORCH_EXTENSIONS_DIR",
    "CAPYCTL_ENGINE_LOG",
    "CUDA_HOME",
    "MAX_JOBS",
    "FLASHINFER_NVCC_THREADS",
];
/// Variables taken from the agent's own environment.
const PASS_THROUGH: &[&str] = &["HOME", "CUDA_VISIBLE_DEVICES"];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanInputTensorfold {
    pub engine_bin: String,
    pub engine_path_extra: Option<String>,
    pub cuda_home: Option<String>,
    pub build_env: BTreeMap<String, String>,
    pub model_path: String,
    pub served_model_name: String,
    pub port: u16,
    /// ADR 0023 §4: required; fixes TensorFold's KV allocation.
    pub context_length: u32,
    pub kv_dtype: Option<String>,
    pub max_tokens: Option<u32>,
    pub thinking: Option<bool>,
    pub engine_args: Vec<String>,
    pub extra_args: Vec<String>,
    pub extensions_dir: Option<String>,
    pub engine_log: Option<String>,
    pub cuda_namespace: Option<capyctl_config::effective::CudaNamespace>,
    /// ADR 0023 §4: the bound a launch with an existing build
    /// gives up at, in milliseconds.
    pub warm_startup_ms: i64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TensorfoldArgsError {
    #[error("reserved option {0} conflicts with a capyctl-owned setting")]
    Reserved(String),
    #[error("duplicate option {0}")]
    Duplicate(String),
    #[error("malformed engine arguments: {0}")]
    Malformed(String),
    #[error("a TensorFold launch needs a context length")]
    NoContext,
    #[error("{}", TENSORFOLD_DRAFTS_CONFLICT)]
    DraftsConflict,
}

/// The typed flags, in TensorFold's own spelling (`tensorfold/cli_args.py`).
fn typed_args(input: &PlanInputTensorfold) -> Vec<String> {
    let mut argv = vec!["--context".to_owned(), input.context_length.to_string()];
    if let Some(dtype) = &input.kv_dtype {
        argv.extend(["--kv-dtype".to_owned(), dtype.clone()]);
    }
    if let Some(tokens) = input.max_tokens {
        argv.extend(["--max-tokens".to_owned(), tokens.to_string()]);
    }
    match input.thinking {
        Some(true) => argv.push("--thinking".to_owned()),
        Some(false) => argv.push("--no-thinking".to_owned()),
        None => {}
    }
    argv
}

fn option_names(args: &[String]) -> Result<Vec<String>, TensorfoldArgsError> {
    parse_options(args)
        .map(|options| options.into_iter().map(|option| option.name).collect())
        .map_err(|error| TensorfoldArgsError::Malformed(error.to_string()))
}

/// ADR 0023 §4: a typed option in any spelling is refused in the extra
/// arguments, and in the host-fixed arguments when the typed field is set.
fn check_typed(input: &PlanInputTensorfold, typed: &[String]) -> Result<(), TensorfoldArgsError> {
    let rendered: Vec<&str> = typed
        .iter()
        .filter(|token| token.starts_with("--"))
        .filter_map(|token| typed_option_of(Engine::Tensorfold, token))
        .map(|(native, _)| native)
        .collect();
    for name in option_names(&input.engine_args)? {
        if typed_option_of(Engine::Tensorfold, &name)
            .is_some_and(|(native, _)| rendered.contains(&native))
        {
            return Err(TensorfoldArgsError::Duplicate(name));
        }
    }
    for name in option_names(&input.extra_args)? {
        if typed_option_of(Engine::Tensorfold, &name).is_some() {
            return Err(TensorfoldArgsError::Duplicate(name));
        }
    }
    Ok(())
}

pub fn render_command(input: &PlanInputTensorfold) -> Result<RenderedCommand, TensorfoldArgsError> {
    if input.context_length == 0 {
        return Err(TensorfoldArgsError::NoContext);
    }
    // ADR 0023 §3: no protected entry, so the shared policy's
    // prefix rule is the second check of the complete pass-through vector.
    let pass_through: Vec<String> = input
        .engine_args
        .iter()
        .chain(&input.extra_args)
        .cloned()
        .collect();
    validate_rendered_args(Engine::Tensorfold, &pass_through, false).map_err(
        |error| match error {
            ProfileArgError::Reserved(name) | ProfileArgError::ConfigFile(name) => {
                TensorfoldArgsError::Reserved(name)
            }
            ProfileArgError::Duplicate(name) => TensorfoldArgsError::Duplicate(name),
            other => TensorfoldArgsError::Malformed(other.to_string()),
        },
    )?;
    let typed = typed_args(input);
    check_typed(input, &typed)?;
    let mut argv = vec![
        input.engine_bin.clone(),
        "serve".into(),
        input.model_path.clone(),
        // ADR 0023 §3: capyctl owns the served name and the loopback listener.
        "--name".into(),
        input.served_model_name.clone(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        input.port.to_string(),
        "--no-update-check".into(),
        // ADR 0023 §3: NVIDIA only; prefix snapshots stay in memory.
        "--backend".into(),
        "cuda".into(),
        "--snapshot-dir".into(),
        "none".into(),
    ];
    argv.extend(typed);
    // ADR 0023 §3, §5: TensorFold's default `--drafter auto` would read the
    // Hugging Face cache; without an approved `--drafter` or `--no-drafts` it
    // is `none`. Some families refuse `none` on CUDA (Qwen3.8 dense), so drafts
    // turned off render `--no-drafts` alone.
    let drafts = tensorfold_drafts(&pass_through)
        .map_err(|error| TensorfoldArgsError::Malformed(error.to_string()))?;
    if drafts.names_drafter && drafts.drafts_off {
        return Err(TensorfoldArgsError::DraftsConflict);
    }
    if !drafts.names_drafter && !drafts.drafts_off {
        argv.extend(["--drafter".to_owned(), "none".to_owned()]);
    }
    argv.extend(pass_through);
    let mut env = BTreeMap::new();
    env.insert("TENSORFOLD_NO_UPDATE_CHECK".into(), "1".into());
    // ADR 0023 §3: the engine fetches nothing; capyctl's model store does.
    env.insert("HF_HUB_OFFLINE".into(), "1".into());
    env.insert("TRANSFORMERS_OFFLINE".into(), "1".into());
    if let Some(dir) = &input.extensions_dir {
        env.insert("TORCH_EXTENSIONS_DIR".into(), dir.clone());
    }
    if let Some(namespace) = &input.cuda_namespace {
        for (name, value) in namespace.environment() {
            env.insert(name.into(), value);
        }
    }
    Ok(RenderedCommand { argv, env })
}

/// The closed environment one launch starts with (SPEC §13.3): the rendered
/// variables, the closed PATH, the toolchain limits, a few pass-throughs.
pub fn engine_environment(
    rendered: &BTreeMap<String, String>,
    plan: &PlanInputTensorfold,
    inherited: &dyn Fn(&str) -> Option<String>,
    toolchain: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for name in PASS_THROUGH {
        if let Some(value) = inherited(name) {
            env.insert((*name).to_owned(), value);
        }
    }
    env.extend(rendered.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.insert(
        "PATH".into(),
        crate::engine_env::tool_path(
            plan.engine_path_extra.as_deref(),
            plan.cuda_home.as_deref(),
            crate::engine_env::SYSTEM_PATH,
        ),
    );
    env.extend(toolchain.iter().map(|(k, v)| (k.clone(), v.clone())));
    if let Some(log) = &plan.engine_log {
        env.insert("CAPYCTL_ENGINE_LOG".into(), log.clone());
    }
    env.retain(|name, _| ENGINE_ENV_ALLOWLIST.contains(&name.as_str()));
    env
}

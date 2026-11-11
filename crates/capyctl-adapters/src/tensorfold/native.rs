//! ADR 0039 §4, §5: TensorFold's native line (1.x, `bin/tensorfold-native`):
//! the `serve` command and its closed environment. The binary matches option
//! names exactly and expands no abbreviation, so every check here compares
//! exact names (`--name=value` split at `=`); a flag the binary does not
//! serve is refused by the binary at start (`crate::launch_failure`).
use std::collections::BTreeMap;

use capyctl_config::engine_policy::{parse_options, ProfileArgError};
use capyctl_config::tensorfold_native::{
    self, DRAFTER_REFUSAL, KV_CACHE_DTYPE_REFUSAL, MEMORY_RESERVE_GIB, PARALLEL,
};

use super::args::{gib_text, PlanInputTensorfold, TensorfoldArgsError, PASS_THROUGH};
use crate::traits::RenderedCommand;

/// ADR 0039 §5: every variable a native TensorFold engine may start with,
/// besides the resolved engine environment's approved names (SPEC §13.3 /
/// T21). The 0.6 list's `TORCH_EXTENSIONS_DIR`, `CUDA_HOME`, `MAX_JOBS`,
/// `FLASHINFER_NVCC_THREADS`, `TRANSFORMERS_OFFLINE` and `TF_NCCL_LIB` are
/// not here: nothing compiles, nothing reads the Transformers cache and there
/// is no group.
pub const ENGINE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "CUDA_VISIBLE_DEVICES",
    "CUDA_DEVICE_ORDER",
    "HF_HUB_OFFLINE",
    "TENSORFOLD_NO_UPDATE_CHECK",
    "TENSORFOLD_CUDA_MEMORY_LIMIT_GB",
    "TENSORFOLD_MEMORY_RESERVE_GIB",
    "CAPYCTL_ENGINE_LOG",
    "DO_NOT_TRACK",
];

/// ADR 0039 §5: the 0.6 line's build and group variables, which the native
/// line drops even when the resolved engine environment names them (the
/// binary builds nothing and serves no group, so they would only widen the
/// closed environment).
const DROPPED_ENV: &[&str] = &[
    "TORCH_EXTENSIONS_DIR",
    "CUDA_HOME",
    "MAX_JOBS",
    "FLASHINFER_NVCC_THREADS",
    "TRANSFORMERS_OFFLINE",
    "TF_NCCL_LIB",
];

/// ADR 0039 §13: the native line serves one GPU and no group.
pub const GROUP_UNSUPPORTED: &str = "group_unsupported";

fn args_error(error: ProfileArgError) -> TensorfoldArgsError {
    match error {
        ProfileArgError::Reserved(name) | ProfileArgError::ConfigFile(name) => {
            TensorfoldArgsError::Reserved(name)
        }
        ProfileArgError::Duplicate(name) => TensorfoldArgsError::Duplicate(name),
        // ADR 0039 §6: `--drafter` in any arguments.
        ProfileArgError::NativeDrafter(_) => {
            TensorfoldArgsError::NativeRefused(DRAFTER_REFUSAL.to_owned())
        }
        other => TensorfoldArgsError::Malformed(other.to_string()),
    }
}

fn option_names(args: &[String]) -> Result<Vec<String>, TensorfoldArgsError> {
    parse_options(args)
        .map(|options| options.into_iter().map(|option| option.name).collect())
        .map_err(args_error)
}

/// The typed flags (ADR 0039 §4): `--context`, `--parallel` unless the
/// arguments pass their own, `--max-tokens`, `--thinking` / `--no-thinking`.
/// Nothing for `kv_cache_dtype` (no native flag), drafts or compaction.
fn typed_args(input: &PlanInputTensorfold) -> Vec<String> {
    let mut argv = vec!["--context".to_owned(), input.context_length.to_string()];
    if let Some(parallel) = input.parallel {
        argv.extend([PARALLEL.to_owned(), parallel.to_string()]);
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

/// ADR 0039 §4: a typed field's spelling is refused in the extra arguments,
/// and in the host-fixed arguments when CapyCTL renders that field; names
/// compared exactly.
fn check_typed(input: &PlanInputTensorfold) -> Result<(), TensorfoldArgsError> {
    let rendered = |field: &str| match field {
        "tensorfold.max_tokens" => input.max_tokens.is_some(),
        "tensorfold.thinking" => input.thinking.is_some(),
        _ => false,
    };
    for name in option_names(&input.engine_args)? {
        if tensorfold_native::typed_field(&name).is_some_and(rendered) {
            return Err(TensorfoldArgsError::Duplicate(name));
        }
    }
    for name in option_names(&input.extra_args)? {
        if tensorfold_native::typed_field(&name).is_some() {
            return Err(TensorfoldArgsError::Duplicate(name));
        }
    }
    Ok(())
}

/// ADR 0039 §4: `<root>/bin/tensorfold-native serve <model dir> --name
/// <served> --host 127.0.0.1 --port <port> --no-update-check --backend cuda
/// --snapshot-dir none --context <n> --parallel <n> [--max-tokens <n>]
/// [--thinking | --no-thinking] [host args] [extra args]`, the `--parallel`
/// pair left out only when the arguments carry their own.
pub(super) fn render_command(
    input: &PlanInputTensorfold,
) -> Result<RenderedCommand, TensorfoldArgsError> {
    if input.context_length == 0 {
        return Err(TensorfoldArgsError::NoContext);
    }
    // ADR 0039 §13: no group on the native line; resolution refuses a
    // topology, and a group member that reaches here is refused again.
    if input.group.is_some() {
        return Err(TensorfoldArgsError::NativeRefused(format!(
            "{GROUP_UNSUPPORTED}: TensorFold native serves one GPU and no group"
        )));
    }
    // ADR 0039 §6: resolution refuses `kv_cache_dtype`; the binary has no
    // KV cache type option, so nothing would carry it.
    if input.kv_dtype.is_some() {
        return Err(TensorfoldArgsError::NativeRefused(
            KV_CACHE_DTYPE_REFUSAL.to_owned(),
        ));
    }
    let pass_through: Vec<String> = input
        .engine_args
        .iter()
        .chain(&input.extra_args)
        .cloned()
        .collect();
    // ADR 0039 §4, §6: the reserved names (every native version, certified
    // or not), `--drafter` and duplicates, by exact name.
    tensorfold_native::validate_args(&pass_through, None).map_err(args_error)?;
    check_typed(input)?;
    // ADR 0023 §4 (amended 2026-10-03), unchanged on this line: a rendered
    // `--parallel` and a passed one never both reach the engine.
    if input.parallel.is_some()
        && tensorfold_native::parallel(&pass_through)
            .map_err(args_error)?
            .is_some()
    {
        return Err(TensorfoldArgsError::Duplicate(PARALLEL.to_owned()));
    }
    let mut argv = vec![
        input.engine_bin.clone(),
        "serve".into(),
        input.model_path.clone(),
        // ADR 0023 §3, ADR 0039 §4: CapyCTL owns the served name and the
        // loopback listener.
        "--name".into(),
        input.served_model_name.clone(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        input.port.to_string(),
        // ADR 0039 §4: an accepted compatibility switch on 1.0 and 1.1.5,
        // kept in case a later release adds a release check.
        "--no-update-check".into(),
        "--backend".into(),
        "cuda".into(),
        "--snapshot-dir".into(),
        "none".into(),
    ];
    argv.extend(typed_args(input));
    argv.extend(pass_through);
    let mut env = BTreeMap::new();
    env.insert("TENSORFOLD_NO_UPDATE_CHECK".into(), "1".into());
    // ADR 0023 §3: the engine fetches nothing; capyctl's model store does.
    env.insert("HF_HUB_OFFLINE".into(), "1".into());
    // ADR 0039 §8 (owner decision 2026-10-27, question 3): the engine's own
    // reserve at the smallest value it takes on a unified device; CapyCTL's
    // ledger already holds the host's reserve.
    env.insert(
        "TENSORFOLD_MEMORY_RESERVE_GIB".into(),
        MEMORY_RESERVE_GIB.into(),
    );
    // ADR 0039 §8: the cap is the declared Ready allocation, as on 0.6.3+.
    if let Some(bytes) = input.memory_limit_bytes.filter(|bytes| *bytes > 0) {
        env.insert("TENSORFOLD_CUDA_MEMORY_LIMIT_GB".into(), gib_text(bytes));
    }
    if let Some(namespace) = &input.cuda_namespace {
        for (name, value) in namespace.environment() {
            env.insert(name.into(), value);
        }
    }
    Ok(RenderedCommand { argv, env })
}

/// Whether the resolved engine environment's `name` may reach the native
/// engine: approved, and neither refused (ADR 0039 §5, defence in depth:
/// resolution refuses these names on a native profile) nor a dropped 0.6
/// build variable.
fn carried(plan: &PlanInputTensorfold, name: &str) -> bool {
    plan.build_env.contains_key(name)
        && tensorfold_native::refused_env(name).is_none()
        && !DROPPED_ENV.contains(&name)
}

/// ADR 0039 §5: the native line's closed environment (SPEC §13.3): the
/// closed system PATH (no CUDA or installation `bin`: the binary runs
/// nothing), `HOME`, the CUDA namespace, the rendered variables,
/// `CAPYCTL_ENGINE_LOG`, `DO_NOT_TRACK=1` under the host's
/// `engine_telemetry: off`, and the resolved engine environment's approved
/// names.
pub(super) fn engine_environment(
    rendered: &BTreeMap<String, String>,
    plan: &PlanInputTensorfold,
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
    // renders, so a rendered or fixed value is never replaced.
    env.extend(
        plan.build_env
            .iter()
            .filter(|(name, _)| carried(plan, name))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    env.extend(rendered.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.insert("PATH".into(), crate::engine_env::SYSTEM_PATH.to_owned());
    if let Some(log) = &plan.engine_log {
        env.insert("CAPYCTL_ENGINE_LOG".into(), log.clone());
    }
    crate::engine_telemetry::apply(&mut env, crate::engine_telemetry::DO_NOT_TRACK, telemetry);
    env.retain(|name, _| ENGINE_ENV_ALLOWLIST.contains(&name.as_str()) || carried(plan, name));
    env
}

/// SPEC §8.2, ADR 0039 §9: the stream count `/health` `max_batch_size` must
/// report: the rendered `--parallel`, or the arguments' own when it is a
/// count the engine takes as written (1 to 4096; `auto` and anything the
/// engine would clamp are not checked). `None`: nothing to verify.
pub fn expected_batch_size(plan: &PlanInputTensorfold) -> Option<u32> {
    if let Some(parallel) = plan.parallel {
        return Some(parallel);
    }
    let pass_through: Vec<String> = plan
        .engine_args
        .iter()
        .chain(&plan.extra_args)
        .cloned()
        .collect();
    tensorfold_native::parallel(&pass_through)
        .ok()
        .flatten()
        .flatten()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|count| (1..=4096).contains(count))
}

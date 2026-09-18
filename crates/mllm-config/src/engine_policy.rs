//! Shared, closed launch argument and environment policy.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Vllm,
    Sglang,
}

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
    "--kv-cache-dtype",
    "--block-size",
    "--enable-sleep-mode",
    "--api-key",
    // Spec §3: the guard middleware is owned by mllm, not a profile — a
    // profile cannot pass its own `--middleware` to bypass or replace it.
    "--middleware",
    "--disable-log-requests",
    "--enable-log-requests",
    "--disable-log-stats",
    "--log-config-file",
    "--uvicorn-log-level",
    "--disable-uvicorn-access-log",
];
const VLLM_APPROVED_FLAGS: &[&str] = &[
    "--max-model-len",
    "--trust-remote-code",
    "--dtype",
    "--enforce-eager",
    "--max-num-seqs",
    "--max-num-batched-tokens",
    "--tokenizer-mode",
];
const VLLM_BOOLEAN_FLAGS: &[&str] = &["--trust-remote-code", "--enforce-eager"];
const SAFE_ENV: &[&str] = &["RUST_LOG", "TOKENIZERS_PARALLELISM", "PYTHONUNBUFFERED"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileArgError {
    #[error("reserved flag `{0}`")]
    Reserved(String),
    #[error("duplicate flag `{0}`")]
    Duplicate(String),
    #[error("unsupported flag `{0}`")]
    Unsupported(String),
    #[error("flag `{0}` requires a value")]
    MissingValue(String),
    #[error("unexpected positional argument `{0}`")]
    UnexpectedArgument(String),
}

pub fn normalize_option_name(argument: &str) -> String {
    argument
        .split_once('=')
        .map_or(argument, |(name, _)| name)
        .to_ascii_lowercase()
        .replace('_', "-")
}

pub fn validate_profile_args(engine: Engine, args: &[String]) -> Result<(), ProfileArgError> {
    if engine != Engine::Vllm {
        return args
            .first()
            .map_or(Ok(()), |arg| Err(ProfileArgError::Unsupported(arg.clone())));
    }
    let mut seen = BTreeSet::new();
    let mut i = 0;
    while i < args.len() {
        let argument = &args[i];
        if !argument.starts_with("--") {
            return Err(ProfileArgError::UnexpectedArgument(argument.clone()));
        }
        let normalized = normalize_option_name(argument);
        if VLLM_RESERVED_FLAGS.contains(&normalized.as_str()) {
            return Err(ProfileArgError::Reserved(normalized));
        }
        if !VLLM_APPROVED_FLAGS.contains(&normalized.as_str()) {
            return Err(ProfileArgError::Unsupported(normalized));
        }
        if !seen.insert(normalized.clone()) {
            return Err(ProfileArgError::Duplicate(normalized));
        }
        if argument
            .split_once('=')
            .is_some_and(|(_, value)| value.is_empty())
        {
            return Err(ProfileArgError::MissingValue(normalized));
        }
        if argument.contains('=') || VLLM_BOOLEAN_FLAGS.contains(&normalized.as_str()) {
            i += 1;
        } else if args.get(i + 1).is_some_and(|value| !value.starts_with('-')) {
            i += 2;
        } else {
            return Err(ProfileArgError::MissingValue(normalized));
        }
    }
    Ok(())
}

pub fn validate_profile_env(env: &BTreeMap<String, String>) -> Result<(), String> {
    env.keys()
        .find(|name| !SAFE_ENV.contains(&name.as_str()))
        .map_or(Ok(()), |name| Err(name.clone()))
}

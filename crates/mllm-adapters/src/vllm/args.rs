//! Argument rendering for the vLLM adapter (F1 design §4, T14): mllm
//! controls reserved settings with explicit units; reviewed profile arguments are
//! appended after the controlled block; security-sensitive flags are
//! reserved and their values redacted in recorded fingerprints.

use crate::traits::RenderedCommand;
use mllm_config::engine_policy::{validate_profile_args, Engine, ProfileArgError};

/// Flags mllm owns: user pass-through conflicts fail validation (T14) and
/// the adapter renders them from granted budgets/contracts.
pub use mllm_config::engine_policy::VLLM_RESERVED_FLAGS as RESERVED_FLAGS;

#[derive(Debug, Clone)]
pub struct PlanInputVllm {
    /// The engine executable (profile-owned launch context, SPEC §8.1).
    pub engine_bin: String,
    pub model_path: String,
    pub port: u16,
    pub granted: GrantedBudget,
    /// Engine-native arguments approved by the selected profile policy.
    pub engine_args: Vec<String>,
    /// Development/sleep startup flags — rendered only when the profile is
    /// policy-gated in (F1 design §7).
    pub sleep_flags: Vec<String>,
    /// Per-deployment engine API credential (mllm-controlled, never from
    /// user args); redacted in fingerprints (SPEC §8.2/§13.3).
    pub api_key: Option<String>,
    /// Extra PATH entries for the engine's runtime environment (venv bin:
    /// the JIT compile step needs the venv's tools, e.g. ninja).
    pub engine_path_extra: Option<String>,
    /// Where the launcher writes the engine's stdout/stderr (diagnosability
    /// + the runbook's evidence record).
    pub engine_log: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GrantedBudget {
    pub kv_cache_bytes: Option<i64>,
    pub gpu_utilization_pct: Option<u8>,
    pub swap_space_bytes: Option<i64>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ArgsError {
    #[error("reserved flag `{0}` may not be passed through (T14)")]
    ReservedConflict(String),
    #[error("duplicate engine flag `{0}` (T14: pass-through flags are declared once)")]
    DuplicateFlag(String),
    #[error("unsupported engine flag `{0}` (profile arguments require explicit approval)")]
    UnsupportedFlag(String),
    #[error("engine flag `{0}` requires a value")]
    MissingValue(String),
    #[error("unexpected positional engine argument `{0}`")]
    UnexpectedArgument(String),
    #[error("invalid granted budget: {0}")]
    InvalidBudget(String),
}

pub fn render_command(input: &PlanInputVllm) -> Result<RenderedCommand, ArgsError> {
    // The shared profile policy owns normalization, allowlisting, reserved
    // conflicts, value shape, and duplicate detection.
    validate_profile_args(Engine::Vllm, &input.engine_args).map_err(|error| match error {
        ProfileArgError::Reserved(flag) => ArgsError::ReservedConflict(flag),
        ProfileArgError::Duplicate(flag) => ArgsError::DuplicateFlag(flag),
        ProfileArgError::Unsupported(flag) => ArgsError::UnsupportedFlag(flag),
        ProfileArgError::MissingValue(flag) => ArgsError::MissingValue(flag),
        ProfileArgError::UnexpectedArgument(argument) => ArgsError::UnexpectedArgument(argument),
    })?;
    // Validate granted budgets are finite and in range.
    if let Some(pct) = input.granted.gpu_utilization_pct {
        if pct == 0 || pct > 100 {
            return Err(ArgsError::InvalidBudget(format!(
                "gpu_utilization_pct {pct}"
            )));
        }
    }
    for (name, bytes) in [
        ("kv_cache_bytes", input.granted.kv_cache_bytes),
        ("swap_space_bytes", input.granted.swap_space_bytes),
    ] {
        if let Some(b) = bytes {
            if b <= 0 {
                return Err(ArgsError::InvalidBudget(format!("{name} {b}")));
            }
        }
    }

    let mut argv: Vec<String> = vec![
        input.engine_bin.clone(),
        "serve".into(),
        input.model_path.clone(),
    ];
    fn push(argv: &mut Vec<String>, flag: &str, value: String) {
        argv.push(flag.to_string());
        argv.push(value);
    }
    push(&mut argv, "--port", input.port.to_string());
    if let Some(pct) = input.granted.gpu_utilization_pct {
        push(
            &mut argv,
            "--gpu-memory-utilization",
            format!("{}.{:02}", pct / 100, pct % 100),
        );
    }
    if let Some(kv) = input.granted.kv_cache_bytes {
        // vLLM 0.29 renders explicit KV bytes via `--kv-cache-memory` (the
        // gpu-memory-utilization heuristic misbehaves on unified-memory
        // hosts — live capture, Spark 2026-09-12). The granted budget maps
        // to bytes with the unit explicit (SPEC §7.5).
        push(&mut argv, "--kv-cache-memory", kv.to_string());
    }
    if let Some(swap) = input.granted.swap_space_bytes {
        // vLLM's --swap-space is expressed in GiB; convert from bytes with
        // the unit named explicitly (SPEC §7.5).
        push(&mut argv, "--swap-space", format!("{}", swap_gib(swap)));
    }
    // Development/sleep flags render only when the profile is gated in
    // (empty list otherwise): the gate is the profile, not the flag.
    for f in &input.sleep_flags {
        argv.push(f.clone());
    }
    if let Some(key) = &input.api_key {
        push(&mut argv, "--api-key", key.clone());
    }
    // Reviewed engine-native arguments render last.
    argv.extend(input.engine_args.iter().cloned());
    // vLLM gates its HTTP sleep/wake/reload routes separately from the
    // allocator flag. Explicitly disable them for stock profiles too, so
    // an inherited development environment cannot bypass the host opt-in.
    let dev_mode = input.sleep_flags.iter().any(|f| f == "--enable-sleep-mode");
    Ok(RenderedCommand {
        argv,
        env: [(
            "VLLM_SERVER_DEV_MODE".into(),
            if dev_mode { "1" } else { "0" }.into(),
        )]
        .into_iter()
        .collect(),
    })
}

/// Bytes → whole GiB (vLLM swap-space unit).
fn swap_gib(b: i64) -> i64 {
    b / (1024 * 1024 * 1024)
}

/// Launch fingerprint with secrets redacted (SPEC §8.2/§13.3): the recorded
/// provenance must never carry credential values.
pub fn fingerprint_of(cmd: &RenderedCommand) -> String {
    let mut redacted = cmd.argv.clone();
    let mut i = 0;
    while i < redacted.len() {
        if redacted[i] == "--api-key" && i + 1 < redacted.len() {
            redacted[i + 1] = "<redacted>".into();
            i += 2;
        } else {
            i += 1;
        }
    }
    redacted.join(" ")
}

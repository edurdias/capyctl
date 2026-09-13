//! Argument rendering for the vLLM adapter (F1 design §4, T14): mllm
//! controls reserved settings with explicit units; reviewed profile arguments are
//! appended after the controlled block; security-sensitive flags are
//! reserved and their values redacted in recorded fingerprints.

use crate::traits::RenderedCommand;

/// Flags mllm owns: user pass-through conflicts fail validation (T14) and
/// the adapter renders them from granted budgets/contracts.
pub const RESERVED_FLAGS: &[&str] = &[
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
    "--disable-log-requests",
    "--enable-log-requests",
    "--disable-log-stats",
    "--log-config-file",
    "--uvicorn-log-level",
    "--disable-uvicorn-access-log",
];

const APPROVED_FLAGS: &[&str] = &[
    "--max-model-len",
    "--trust-remote-code",
    "--dtype",
    "--enforce-eager",
    "--max-num-seqs",
    "--max-num-batched-tokens",
    "--tokenizer-mode",
];
const APPROVED_BOOLEAN_FLAGS: &[&str] = &["--trust-remote-code", "--enforce-eager"];

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
    // Approved-argument validation parses flag/value pairs by position: an
    // arg starting with `-` is a flag (the next position is its value); a
    // bare positional (odd-length tail) is engine-native and passes through.
    // Reserved flags conflict; duplicates of ordinary flags are declared
    // errors, never misreported as reserved conflicts.
    let mut seen = std::collections::BTreeSet::new();
    let mut i = 0;
    while i < input.engine_args.len() {
        let arg = &input.engine_args[i];
        if arg.starts_with('-') {
            let raw_name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
            let normalized = raw_name.to_ascii_lowercase().replace('_', "-");
            if RESERVED_FLAGS.contains(&normalized.as_str()) {
                return Err(ArgsError::ReservedConflict(normalized));
            }
            if !APPROVED_FLAGS.contains(&normalized.as_str()) {
                return Err(ArgsError::UnsupportedFlag(normalized));
            }
            if !seen.insert(normalized.clone()) {
                return Err(ArgsError::DuplicateFlag(normalized));
            }
            // The next position is this flag's value (skipped by the
            // stride; a trailing flag with no value parses as boolean).
            if arg.split_once('=').is_some_and(|(_, value)| value.is_empty()) {
                return Err(ArgsError::MissingValue(normalized));
            }
            if arg.contains('=') || APPROVED_BOOLEAN_FLAGS.contains(&normalized.as_str()) {
                i += 1;
            } else if input
                .engine_args
                .get(i + 1)
                .is_some_and(|v| !v.starts_with('-'))
            {
                i += 2;
            } else {
                return Err(ArgsError::MissingValue(normalized));
            }
        } else {
            return Err(ArgsError::UnexpectedArgument(arg.clone()));
        }
    }
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

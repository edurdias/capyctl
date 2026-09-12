//! Argument rendering for the vLLM adapter (F1 design §4, T14): mllm
//! controls reserved settings with explicit units; user pass-through is
//! appended after the controlled block; security-sensitive flags are
//! reserved and their values redacted in recorded fingerprints.

use crate::traits::RenderedCommand;

/// Flags mllm owns: user pass-through conflicts fail validation (T14) and
/// the adapter renders them from granted budgets/contracts.
pub const RESERVED_FLAGS: &[&str] = &[
    "--port",
    "--device",
    "--gpu-memory-utilization",
    "--swap-space",
    "--kv-cache-bytes",
    "--enable-sleep-mode",
    "--api-key",
];

#[derive(Debug, Clone)]
pub struct PlanInputVllm {
    /// The engine executable (profile-owned launch context, SPEC §8.1).
    pub engine_bin: String,
    pub model_path: String,
    pub port: u16,
    pub granted: GrantedBudget,
    /// Engine-native ordinary arguments the operator passes through
    /// (SPEC §8.2: unknown ordinary args pass subject to policy).
    pub engine_args: Vec<String>,
    /// Development/sleep startup flags — rendered only when the profile is
    /// policy-gated in (F1 design §7).
    pub sleep_flags: Vec<String>,
    /// Per-deployment engine API credential (mllm-controlled, never from
    /// user args); redacted in fingerprints (SPEC §8.2/§13.3).
    pub api_key: Option<String>,
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
    #[error("invalid granted budget: {0}")]
    InvalidBudget(String),
}

pub fn render_command(input: &PlanInputVllm) -> Result<RenderedCommand, ArgsError> {
    // Reserved-flag conflicts fail before anything renders (T14).
    let mut seen = std::collections::BTreeSet::new();
    for pair in input.engine_args.chunks(2) {
        let flag = &pair[0];
        if RESERVED_FLAGS.contains(&flag.as_str()) {
            return Err(ArgsError::ReservedConflict(flag.clone()));
        }
        if !seen.insert(flag.clone()) {
            return Err(ArgsError::ReservedConflict(flag.clone()));
        }
    }
    // Validate granted budgets are finite and in range.
    if let Some(pct) = input.granted.gpu_utilization_pct {
        if pct == 0 || pct > 100 {
            return Err(ArgsError::InvalidBudget(format!("gpu_utilization_pct {pct}")));
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
        push(&mut argv, "--gpu-memory-utilization", format!("{}.{:02}", pct / 100, pct % 100));
    }
    if let Some(kv) = input.granted.kv_cache_bytes {
        push(&mut argv, "--kv-cache-bytes", kv.to_string());
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
    // User pass-through last (SPEC §8.2: preserve engine-native args).
    argv.extend(input.engine_args.iter().cloned());
    Ok(RenderedCommand { argv, env: Default::default() })
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
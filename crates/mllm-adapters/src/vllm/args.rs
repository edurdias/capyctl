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
    /// Spec §3: mllm owns the listener address and the served name, so the
    /// served id is a launch setting, not a pass-through argument a
    /// profile could omit or spoof.
    pub served_model_name: String,
    pub tensor_parallel_size: u32,
    pub pipeline_parallel_size: u32,
    pub kv_cache_dtype: String,
    pub block_size_tokens: u32,
    /// CPU-offload budget in bytes; rendered as whole GiB (vLLM's unit).
    /// Zero means "no offload flag" (Step 3, Spec §3).
    pub cpu_offload_bytes: i64,
    pub granted: GrantedBudget,
    /// Engine-native arguments approved by the selected profile policy.
    pub engine_args: Vec<String>,
    /// Development/sleep startup flags — rendered only when the profile is
    /// policy-gated in (F1 design §7).
    pub sleep_flags: Vec<String>,
    /// Per-deployment engine API credential. Spec §3: never rendered on
    /// argv; delivered through the environment by the builder (the adapter
    /// clears this field before calling `render_command`). Kept so
    /// `fingerprint_of`'s redaction path keeps compiling and stays in
    /// place as a defense in depth.
    pub api_key: Option<String>,
    /// Extra PATH entries for the engine's runtime environment (venv bin:
    /// the JIT compile step needs the venv's tools, e.g. ninja).
    pub engine_path_extra: Option<String>,
    /// Where the launcher writes the engine's stdout/stderr (diagnosability
    /// + the runbook's evidence record).
    pub engine_log: Option<String>,
    /// Directory containing `mllm_vllm_guard.py`, mllm's own middleware
    /// that requires the engine key on vLLM's development routes (Spec
    /// §3). Required whenever `sleep_flags` gates development mode in:
    /// without it there is nowhere to point `PYTHONPATH` and the guard
    /// cannot be loaded, so dev mode without a runtime dir is refused
    /// rather than served unguarded.
    pub runtime_dir: Option<String>,
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
    #[error("development mode (sleep flags) requires a runtime dir for mllm's guard middleware (Spec §3)")]
    MissingRuntimeDir,
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
    // Spec §3: vLLM takes whole GiB; a positive sub-GiB budget would
    // silently round to zero and offload nothing, so it is refused instead.
    if input.cpu_offload_bytes > 0 && input.cpu_offload_bytes < 1024 * 1024 * 1024 {
        return Err(ArgsError::InvalidBudget(format!(
            "cpu_offload_bytes {}",
            input.cpu_offload_bytes
        )));
    }
    // Spec §3: development mode always ships with mllm's own guard
    // middleware, which needs a runtime dir to load from; without one the
    // dev routes would otherwise be reachable unguarded.
    let dev_mode = input.sleep_flags.iter().any(|f| f == "--enable-sleep-mode");
    if dev_mode && input.runtime_dir.is_none() {
        return Err(ArgsError::MissingRuntimeDir);
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
    // Spec §3: mllm owns the listener address and the served name; a
    // profile's engine_args cannot set them (they are reserved flags).
    push(&mut argv, "--host", "127.0.0.1".into());
    push(&mut argv, "--served-model-name", input.served_model_name.clone());
    // The five validated launch settings, rendered unconditionally except
    // the CPU-offload budget, which is omitted rather than sent as zero.
    push(
        &mut argv,
        "--tensor-parallel-size",
        input.tensor_parallel_size.to_string(),
    );
    push(
        &mut argv,
        "--pipeline-parallel-size",
        input.pipeline_parallel_size.to_string(),
    );
    push(&mut argv, "--kv-cache-dtype", input.kv_cache_dtype.clone());
    push(&mut argv, "--block-size", input.block_size_tokens.to_string());
    if input.cpu_offload_bytes > 0 {
        push(
            &mut argv,
            "--cpu-offload-gb",
            (input.cpu_offload_bytes / (1024 * 1024 * 1024)).to_string(),
        );
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
    // Spec §3: the guard middleware is owned by mllm, not a profile — a
    // profile cannot pass its own `--middleware` (it is reserved). Loading
    // it requires the guard module to be importable, hence PYTHONPATH.
    let mut env: std::collections::BTreeMap<String, String> = [(
        "VLLM_SERVER_DEV_MODE".into(),
        if dev_mode { "1" } else { "0" }.into(),
    )]
    .into_iter()
    .collect();
    if dev_mode {
        // Presence already checked above (MissingRuntimeDir otherwise).
        let runtime_dir = input.runtime_dir.clone().expect("checked above");
        push(&mut argv, "--middleware", "mllm_vllm_guard.RequireEngineKey".into());
        let existing = std::env::var("PYTHONPATH").ok().filter(|p| !p.is_empty());
        let python_path = match existing {
            Some(existing) => format!("{runtime_dir}:{existing}"),
            None => runtime_dir,
        };
        env.insert("PYTHONPATH".into(), python_path);
    }
    // The engine key is never rendered on argv (Spec §3): it is delivered
    // through the environment by the builder (see `PlanInputVllm::api_key`
    // doc comment), never as a `--api-key` command-line value.
    // Reviewed engine-native arguments render last.
    argv.extend(input.engine_args.iter().cloned());
    // vLLM gates its HTTP sleep/wake/reload routes separately from the
    // allocator flag. Explicitly disable them for stock profiles too, so
    // an inherited development environment cannot bypass the host opt-in.
    Ok(RenderedCommand { argv, env })
}

/// Bytes → whole GiB (vLLM swap-space unit).
fn swap_gib(b: i64) -> i64 {
    b / (1024 * 1024 * 1024)
}

/// The shortest run of credential-shaped characters that is redacted on sight.
/// mllm issues 32-byte keys, hex-encoded to 64 characters, so 48 covers every key
/// it issues with room to spare while leaving a 40-character git commit id — which
/// is what vLLM prints for a checkpoint revision — legible in a failure reason
/// somebody has to read.
const SECRET_RUN_MIN: usize = 48;

const REDACTED: &str = "<redacted>";

/// Characters a hex or base64 credential is made of. `/` is included because a
/// base64 value contains it; the cost is that a long path may be redacted too,
/// which is the cheaper mistake.
fn credential_shaped(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-')
}

fn starts_with_ignoring_case(text: &[char], marker: &str) -> bool {
    let marker: Vec<char> = marker.chars().collect();
    text.len() >= marker.len()
        && text
            .iter()
            .zip(marker.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
}

/// Blank credential material in free text before it is recorded or reported
/// (Spec §3, SPEC §13.3). Engine logs and failure reasons are quoted into
/// journals and errors, and an engine echoes its own key often enough that
/// quoting one verbatim is a question of when, not whether. Three shapes are
/// blanked: an `Authorization: Bearer` value, a `VLLM_API_KEY=` value, and any
/// long run of hex or base64 characters.
pub fn redact_text(text: &str) -> String {
    const MARKERS: [&str; 2] = ["bearer ", "vllm_api_key="];
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(marker) = MARKERS
            .iter()
            .find(|marker| starts_with_ignoring_case(&chars[i..], marker))
        {
            let width = marker.chars().count();
            out.extend(chars[i..i + width].iter());
            i += width;
            let end = chars[i..]
                .iter()
                .position(|c| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';'))
                .map_or(chars.len(), |offset| i + offset);
            if end > i {
                out.push_str(REDACTED);
            }
            i = end;
            continue;
        }
        if credential_shaped(chars[i]) {
            let end = chars[i..]
                .iter()
                .position(|c| !credential_shaped(*c))
                .map_or(chars.len(), |offset| i + offset);
            // A run that begins with `/` is a path, never a bare token: the engine
            // prints checkpoint directories deeper than this threshold, and a
            // failure reason that blanks the path the weights came from hides the
            // one fact an operator needs.
            if chars[i] != '/' && end - i >= SECRET_RUN_MIN {
                out.push_str(REDACTED);
            } else {
                out.extend(chars[i..end].iter());
            }
            i = end;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
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

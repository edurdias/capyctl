//! Argument rendering for the vLLM adapter (F1 design §4, T14; ADR 0014 §2, §6):
//! capyctl controls reserved settings with explicit units; typed deployment fields,
//! host-fixed profile arguments and accepted extra arguments follow a marker the
//! protected entry (`runtime/vllm_entry.py`, owner decision Q11) splits on. The
//! entry parses both blocks with vLLM's own parser and refuses any change to a
//! reserved field before serving; security-sensitive values are redacted in
//! recorded fingerprints.

use crate::traits::RenderedCommand;
use capyctl_config::engine_policy::{
    normalize_option_name, validate_rendered_args, Engine, ProfileArgError,
};

/// Flags capyctl owns: user pass-through conflicts fail validation (T14) and
/// the adapter renders them from granted budgets/contracts.
pub use capyctl_config::engine_policy::VLLM_RESERVED_FLAGS as RESERVED_FLAGS;

/// The protected entry's file name inside the host runtime directory.
pub const VLLM_ENTRY: &str = "vllm_entry.py";

/// Separates the reserved block from everything the deployment or host chose.
/// Must match `runtime/vllm_entry.py` `MARKER`; no user token may start with
/// `--capyctl-`.
pub const USER_ARGS_MARKER: &str = "--capyctl-user-args";

/// ADR 0014 §8, SPEC §8.2: the deployment's own extra arguments follow this
/// second marker, so the entry gates exactly the destinations they resolve to.
/// Must match `runtime/vllm_entry.py` `EXTRA_MARKER`.
pub const EXTRA_ARGS_MARKER: &str = "--capyctl-extra-args";

#[derive(Clone, Default)]
pub struct PlanInputVllm {
    /// The engine executable (profile-owned launch context, SPEC §8.1).
    pub engine_bin: String,
    pub model_path: String,
    pub port: u16,
    /// Spec §3: capyctl owns the listener address and the served name, so the
    /// served id is a launch setting, not a pass-through argument a
    /// profile could omit or spoof.
    pub served_model_name: String,
    pub tensor_parallel_size: u32,
    pub pipeline_parallel_size: u32,
    /// ADR 0014 §2 typed fields. `None`/`false` renders nothing, so the
    /// engine's own default applies.
    pub dtype: Option<String>,
    pub quantization: Option<String>,
    pub kv_cache_dtype: Option<String>,
    pub block_size_tokens: Option<u32>,
    /// `--max-model-len`.
    pub context_length: Option<u32>,
    /// `--max-num-seqs`.
    pub max_concurrent_requests: Option<u32>,
    pub max_num_batched_tokens: Option<u32>,
    /// `cuda_graphs: false` renders `--enforce-eager`.
    pub enforce_eager: bool,
    pub language_model_only: bool,
    /// Host-approved at deploy time (ADR 0014 §8).
    pub trust_remote_code: bool,
    /// ADR 0024: the tool-call and reasoning parsers chosen at launch, and
    /// `--enable-auto-tool-choice` beside a rendered tool parser.
    pub tool_call_parser: Option<String>,
    pub reasoning_parser: Option<String>,
    pub enable_auto_tool_choice: bool,
    /// CPU-offload budget in bytes; rendered as whole GiB (vLLM's unit).
    /// Zero means "no offload flag" (Step 3, Spec §3).
    pub cpu_offload_bytes: i64,
    pub granted: GrantedBudget,
    /// Host-fixed engine arguments (the approved profile's own).
    pub engine_args: Vec<String>,
    /// The deployment's accepted extra arguments (ADR 0014 §6). Rendered after
    /// [`EXTRA_ARGS_MARKER`], where the entry gates their parsed destinations.
    pub extra_args: Vec<String>,
    /// The host's approvals for sensitive extra arguments, as the JSON document
    /// the entry reads from `CAPYCTL_EXTRA_APPROVALS` (ADR 0014 §8). `None`
    /// approves nothing.
    pub extra_approvals: Option<String>,
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
    /// SPEC §13.3 amendment: the profile's host-approved CUDA toolkit root;
    /// `<cuda_home>/bin` joins PATH and `CUDA_HOME` is set (engine_env.rs).
    pub cuda_home: Option<String>,
    /// The profile `env` build-limit overrides (`MAX_JOBS`,
    /// `FLASHINFER_NVCC_THREADS`; engine_env.rs).
    pub build_env: std::collections::BTreeMap<String, String>,
    /// Where the launcher writes the engine's stdout/stderr (diagnosability
    /// + the runbook's evidence record).
    pub engine_log: Option<String>,
    /// Directory containing `capyctl_vllm_guard.py`, capyctl's own middleware
    /// that requires the engine key on vLLM's development routes (Spec
    /// §3). Required whenever `sleep_flags` gates development mode in:
    /// without it there is nowhere to point `PYTHONPATH` and the guard
    /// cannot be loaded, so dev mode without a runtime dir is refused
    /// rather than served unguarded.
    pub runtime_dir: Option<String>,
    /// Discrete GPU design §§6–7: the selected GPU, from the host's own
    /// approved policy, that the child's CUDA namespace is narrowed to so the
    /// engine sees exactly that device as `cuda:0` (by its published UUID, or
    /// by its index in PCI bus order). `None` keeps the agent's own
    /// pass-through (a one-device unified host).
    pub cuda_namespace: Option<capyctl_config::effective::CudaNamespace>,
}

/// SPEC §13.3: the engine key is never formatted.
impl std::fmt::Debug for PlanInputVllm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanInputVllm")
            .field("engine_bin", &self.engine_bin)
            .field("model_path", &self.model_path)
            .field("port", &self.port)
            .field("served_model_name", &self.served_model_name)
            .field("tensor_parallel_size", &self.tensor_parallel_size)
            .field("pipeline_parallel_size", &self.pipeline_parallel_size)
            .field("dtype", &self.dtype)
            .field("quantization", &self.quantization)
            .field("kv_cache_dtype", &self.kv_cache_dtype)
            .field("block_size_tokens", &self.block_size_tokens)
            .field("context_length", &self.context_length)
            .field("max_concurrent_requests", &self.max_concurrent_requests)
            .field("max_num_batched_tokens", &self.max_num_batched_tokens)
            .field("enforce_eager", &self.enforce_eager)
            .field("language_model_only", &self.language_model_only)
            .field("trust_remote_code", &self.trust_remote_code)
            .field("tool_call_parser", &self.tool_call_parser)
            .field("reasoning_parser", &self.reasoning_parser)
            .field("enable_auto_tool_choice", &self.enable_auto_tool_choice)
            .field("cpu_offload_bytes", &self.cpu_offload_bytes)
            .field("granted", &self.granted)
            .field("engine_args", &self.engine_args)
            .field("extra_args", &self.extra_args)
            .field("extra_approvals", &self.extra_approvals)
            .field("sleep_flags", &self.sleep_flags)
            .field("api_key", &crate::traits::redacted(self.api_key.is_some()))
            .field("engine_path_extra", &self.engine_path_extra)
            .field("cuda_home", &self.cuda_home)
            // ADR 0028 §2.1: values may hold tokens; names only.
            .field("build_env", &self.build_env.keys().collect::<Vec<_>>())
            .field("engine_log", &self.engine_log)
            .field("runtime_dir", &self.runtime_dir)
            .field("cuda_namespace", &self.cuda_namespace)
            .finish()
    }
}

/// Discrete GPU design §6 (ADR 0019): the `--gpu-memory-utilization` percent of
/// a launch on a discrete device, the device request's share of the card's
/// total rounded up to a whole percent. vLLM checks at start that this share
/// of the card is free; the planner and the launch check already made that
/// room. At least 75: vLLM 0.29 with CUDA graphs does not start a 4B model on a
/// 16 GB card below 0.75 (design §3, observed on the discrete-GPU laptop host).
/// At most 99: vLLM refuses a whole card.
pub fn device_utilization_pct(request: i64, device_total: i64) -> u8 {
    let total = device_total.max(1);
    let pct = request.max(0).saturating_mul(100).saturating_add(total - 1) / total;
    pct.clamp(75, 99) as u8
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
    #[error("a vLLM launch requires the runtime dir holding capyctl's protected entry and guard (Spec §3, ADR 0014 §6)")]
    MissingRuntimeDir,
    #[error("the engine executable has no directory to find its interpreter in")]
    NoInterpreter,
}

pub fn render_command(input: &PlanInputVllm) -> Result<RenderedCommand, ArgsError> {
    // ADR 0014 §3, §6: the shared policy owns normalization, reserved
    // conflicts (exact, abbreviated or negated), value shape, and duplicate
    // detection over the complete pass-through vector. The approved-flag list
    // is gone; deploy-time checks already refused sensitive options the host
    // did not approve.
    let dev_mode_requested = input.sleep_flags.iter().any(|f| f == "--enable-sleep-mode");
    let pass_through: Vec<String> = input
        .engine_args
        .iter()
        .chain(&input.extra_args)
        .cloned()
        .collect();
    validate_rendered_args(Engine::Vllm, &pass_through, dev_mode_requested).map_err(|error| {
        match error {
            ProfileArgError::Reserved(flag) | ProfileArgError::ConfigFile(flag) => {
                ArgsError::ReservedConflict(flag)
            }
            ProfileArgError::Duplicate(flag) => ArgsError::DuplicateFlag(flag),
            ProfileArgError::MissingValue(flag) => ArgsError::MissingValue(flag),
            ProfileArgError::UnexpectedArgument(index) => {
                ArgsError::UnexpectedArgument(format!("position {index}"))
            }
            ProfileArgError::ShortOption(flag) => ArgsError::UnexpectedArgument(flag),
            other => ArgsError::UnsupportedFlag(other.to_string()),
        }
    })?;
    // ADR 0014 §2: one way to say each thing. A typed field the deployment set
    // cannot also arrive in the pass-through vector; the marker is capyctl's own.
    let typed = typed_args(input);
    let typed_names: Vec<String> = typed
        .iter()
        .filter(|token| token.starts_with("--"))
        .map(|token| normalize_option_name(token))
        .collect();
    for name in pass_through
        .iter()
        .filter(|argument| argument.starts_with("--"))
        .map(|argument| normalize_option_name(argument))
    {
        if name.starts_with("--capyctl-") {
            return Err(ArgsError::ReservedConflict(name));
        }
        if typed_names.contains(&name) {
            return Err(ArgsError::DuplicateFlag(name));
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
    // Spec §3: vLLM takes whole GiB; a positive sub-GiB budget would
    // silently round to zero and offload nothing, so it is refused instead.
    if input.cpu_offload_bytes > 0 && input.cpu_offload_bytes < 1024 * 1024 * 1024 {
        return Err(ArgsError::InvalidBudget(format!(
            "cpu_offload_bytes {}",
            input.cpu_offload_bytes
        )));
    }
    // Spec §3 / ADR 0014 §6: every launch runs through capyctl's protected entry,
    // and development mode also loads capyctl's guard middleware from the same
    // runtime directory; without one neither can be loaded.
    let dev_mode = input.sleep_flags.iter().any(|f| f == "--enable-sleep-mode");
    let runtime_dir = input
        .runtime_dir
        .clone()
        .ok_or(ArgsError::MissingRuntimeDir)?;

    // Owner decision Q11: the installation's own interpreter runs the entry,
    // which runs the server in process with vLLM's own parser.
    let mut argv: Vec<String> = vec![
        interpreter_for(&input.engine_bin)?,
        // SPEC §9.1 / T21: no bytecode is written beside the checked source.
        "-B".into(),
        format!("{}/{VLLM_ENTRY}", runtime_dir.trim_end_matches('/')),
        "serve".into(),
        input.model_path.clone(),
    ];
    fn push(argv: &mut Vec<String>, flag: &str, value: String) {
        argv.push(flag.to_string());
        argv.push(value);
    }
    // Spec §3: capyctl owns the listener address and the served name; a
    // profile's engine_args cannot set them (they are reserved flags).
    push(&mut argv, "--host", "127.0.0.1".into());
    push(
        &mut argv,
        "--served-model-name",
        input.served_model_name.clone(),
    );
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
        // vLLM 0.29 renders explicit KV bytes via `--kv-cache-memory-bytes`
        // (the gpu-memory-utilization heuristic misbehaves on unified-memory
        // hosts — live capture, Spark 2026-09-12). The full spelling, verified
        // in the installed arg_utils.py, replaces the abbreviation S1 used;
        // both resolve to `kv_cache_memory_bytes`. Unit explicit (SPEC §7.5).
        push(&mut argv, "--kv-cache-memory-bytes", kv.to_string());
    }
    // SPEC §7.5 / T14: vLLM 0.29 has no `--swap-space` (its parser refuses it),
    // so a swap budget is validated above and never rendered.
    // Development/sleep flags render only when the profile is gated in
    // (empty list otherwise): the gate is the profile, not the flag.
    for f in &input.sleep_flags {
        argv.push(f.clone());
    }
    // Spec §3: the guard middleware is owned by capyctl, not a profile — a
    // profile cannot pass its own `--middleware` (it is reserved). Loading
    // it requires the guard module to be importable, hence PYTHONPATH.
    let mut env: std::collections::BTreeMap<String, String> = [(
        "VLLM_SERVER_DEV_MODE".into(),
        if dev_mode { "1" } else { "0" }.into(),
    )]
    .into_iter()
    .collect();
    if dev_mode {
        push(
            &mut argv,
            "--middleware",
            "capyctl_vllm_guard.RequireEngineKey".into(),
        );
        // SPEC §9.1 / T21: the verified runtime directory alone; nothing of
        // the agent's own PYTHONPATH reaches the engine's import path.
        env.insert("PYTHONPATH".into(), runtime_dir);
    }
    // ADR 0014 §6: everything after the marker is parsed by vLLM, and the entry
    // refuses it if any reserved field above resolves differently.
    argv.push(USER_ARGS_MARKER.into());
    argv.extend(typed);
    // The engine key is never rendered on argv (Spec §3): it is delivered
    // through the environment by the builder (see `PlanInputVllm::api_key`
    // doc comment), never as a `--api-key` command-line value.
    // Host-fixed arguments, then the deployment's accepted extras after their
    // own marker (ADR 0014 §8: the entry gates what those resolve to).
    argv.extend(input.engine_args.iter().cloned());
    if !input.extra_args.is_empty() {
        argv.push(EXTRA_ARGS_MARKER.into());
        argv.extend(input.extra_args.iter().cloned());
    }
    // Discrete GPU design §7: the chosen device, by the UUID the host itself
    // published or its PCI-ordered index, replaces whatever namespace the
    // agent was started with.
    if let Some(namespace) = &input.cuda_namespace {
        for (name, value) in namespace.environment() {
            env.insert(name.into(), value);
        }
    }
    if let Some(approvals) = &input.extra_approvals {
        env.insert(
            capyctl_config::engine_policy::EXTRA_APPROVALS_ENV.into(),
            approvals.clone(),
        );
    }
    // vLLM gates its HTTP sleep/wake/reload routes separately from the
    // allocator flag. Explicitly disable them for stock profiles too, so
    // an inherited development environment cannot bypass the host opt-in.
    Ok(RenderedCommand { argv, env })
}

/// ADR 0014 §2: typed deployment fields in vLLM 0.29.0 spellings (verified in
/// the installed `vllm/engine/arg_utils.py`). Omitted fields render nothing.
fn typed_args(input: &PlanInputVllm) -> Vec<String> {
    let mut args = Vec::new();
    let mut value = |flag: &str, value: Option<String>| {
        if let Some(value) = value {
            args.push(flag.to_string());
            args.push(value);
        }
    };
    value("--dtype", input.dtype.clone());
    value("--quantization", input.quantization.clone());
    value("--kv-cache-dtype", input.kv_cache_dtype.clone());
    value(
        "--block-size",
        input.block_size_tokens.map(|v| v.to_string()),
    );
    value(
        "--max-model-len",
        input.context_length.map(|v| v.to_string()),
    );
    value(
        "--max-num-seqs",
        input.max_concurrent_requests.map(|v| v.to_string()),
    );
    value(
        "--max-num-batched-tokens",
        input.max_num_batched_tokens.map(|v| v.to_string()),
    );
    // ADR 0024: names verified registered in vLLM 0.29.0 and 0.30.0.
    value("--tool-call-parser", input.tool_call_parser.clone());
    value("--reasoning-parser", input.reasoning_parser.clone());
    for (flag, on) in [
        ("--enforce-eager", input.enforce_eager),
        ("--language-model-only", input.language_model_only),
        ("--trust-remote-code", input.trust_remote_code),
        ("--enable-auto-tool-choice", input.enable_auto_tool_choice),
    ] {
        if on {
            args.push(flag.into());
        }
    }
    args
}

/// The installation's interpreter: the executable itself when it is a Python
/// interpreter, otherwise the `python3` beside it (a virtual environment's
/// `bin`, where the `vllm` console script lives). The path is not resolved, so
/// a venv interpreter keeps its environment.
pub fn interpreter_for(engine_bin: &str) -> Result<String, ArgsError> {
    let path = std::path::Path::new(engine_bin);
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("python"))
    {
        return Ok(engine_bin.to_owned());
    }
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.join("python3").to_string_lossy().into_owned())
        .ok_or(ArgsError::NoInterpreter)
}

/// Bytes → whole GiB (vLLM swap-space unit).
/// The shortest run of credential-shaped characters that is redacted on sight.
/// capyctl issues 32-byte keys, hex-encoded to 64 characters, so 48 covers every key
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
            // one fact an operator needs. A path is kept segment by segment, so a
            // credential carried inside it (`/v1/models/<key>`) is still blanked.
            if chars[i] == '/' {
                let mut first = true;
                for segment in chars[i..end].split(|c| *c == '/') {
                    if !first {
                        out.push('/');
                    }
                    first = false;
                    if segment.len() >= SECRET_RUN_MIN {
                        out.push_str(REDACTED);
                    } else {
                        out.extend(segment.iter());
                    }
                }
            } else if end - i >= SECRET_RUN_MIN {
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

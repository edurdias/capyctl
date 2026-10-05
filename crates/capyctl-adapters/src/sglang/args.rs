//! Secret-free input to the protected Python entrypoint, not native engine argv.
//!
//! ADR 0014 §2, §6: the descriptor carries the deployment's typed settings, its
//! memory request and its extra arguments. The wrapper maps them onto the
//! installed ServerArgs (`runtime/sglang_server_args.py`), parses extra arguments
//! with SGLang's own parser, renders the reserved subset itself and rechecks it
//! after SGLang's resolution. Native flag spellings are deliberately not inferred
//! here. The final launcher owns the
//! private launch/root descriptor and the two credential descriptors; this module
//! never resolves references, reads descriptors, or starts a process.

use crate::sglang::pinned::NATIVE_SGLANG_CONTRACT;
use crate::traits::{RenderedCommand, RuntimeError};
use capyctl_config::engine_policy::{validate_rendered_args, Engine};
use capyctl_domain::launch::{NativeLaunch, SglangLaunchSettings};
use serde_json::{json, Value};
use std::{fmt, path::Path};

/// Inherited descriptor numbers selected by the final launcher. This validates
/// numeric shape only; protection, inheritance, ownership, and contents remain
/// launcher obligations. Numbers and rendering are not proof of an armed send.
#[derive(Debug)]
pub struct ProtectedDescriptorFds {
    launch: i32,
    inference: i32,
    admin: i32,
}

impl ProtectedDescriptorFds {
    pub fn for_launcher(launch: i64, inference: i64, admin: i64) -> Result<Self, RuntimeError> {
        let convert = |value| {
            i32::try_from(value)
                .ok()
                .filter(|value| *value >= 3)
                .ok_or(RuntimeError::Unsupported)
        };
        let (launch, inference, admin) = (convert(launch)?, convert(inference)?, convert(admin)?);
        if launch == inference || launch == admin || inference == admin {
            return Err(RuntimeError::Unsupported);
        }
        Ok(Self {
            launch,
            inference,
            admin,
        })
    }
}

/// Validated public settings only. Checkpoint roots and credential references are
/// never retained in this object, including its Debug/Display/metadata surfaces.
///
/// This value cannot authenticate persistence or authorize launch. Only the
/// controller holding the current persisted `ArmResult::New` may hand the rendered
/// command to a supervised launcher; repeated rendering has no effects.
///
/// An ordinary profile/request is not a candidate descriptor:
/// ```compile_fail
/// # use capyctl_adapters::{sglang::SglangLaunch, PlanInput};
/// fn ordinary(input: &PlanInput) {
///     let _ = SglangLaunch::from_frozen(input);
/// }
/// ```
#[derive(Debug)]
pub struct SglangLaunch {
    executable: String,
    public: Value,
}

impl SglangLaunch {
    pub fn from_frozen(frozen: &NativeLaunch) -> Result<Self, RuntimeError> {
        let m = frozen.metadata();
        let s = frozen.settings();
        let valid_reference = |value: &str| {
            !value.is_empty()
                && value.len() <= 4096
                && !value.chars().any(|c| c.is_whitespace() || c.is_control())
        };
        if m.engine != "sglang"
            || m.recipe != NATIVE_SGLANG_CONTRACT
            || !served_name_token(&m.checkpoint_revision)
            || !ulid(&m.binding_id)
            || !ulid(&m.incarnation)
            || !served_name_token(&m.served_name)
            || !private_endpoint(&m.endpoint)
            || m.rendered_settings_digest.len() != 64
            || !m
                .rendered_settings_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !absolute_path(frozen.checkpoint_root())
            || !absolute_path(frozen.executable())
            || !valid_reference(frozen.inference_credential_ref())
            || !valid_reference(frozen.admin_credential_ref())
            || frozen.inference_credential_ref() == frozen.admin_credential_ref()
            || !selector(&m.device.host_id)
            || !selector(&m.device.device_id)
            || !selector(&m.device.memory_domain)
            || !selector(&m.device.hardware_fingerprint)
            || m.device
                .physical_gpu_uuid
                .as_deref()
                .is_some_and(|uuid| !physical_gpu_uuid(uuid))
        {
            return Err(RuntimeError::Unsupported);
        }
        let settings = public_settings(s)?;
        // The checkpoint revision stays out of the public descriptor: the
        // entry no longer pins a checkpoint (ADR 0014 §9; WE3 verifies a digest).
        let public = json!({
            "schema_version": 2,
            "kind": "sglang_launch",
            "engine": m.engine,
            "binding_id": m.binding_id,
            "incarnation": m.incarnation,
            "endpoint": m.endpoint,
            "served_name": m.served_name,
            "rendered_settings_digest": m.rendered_settings_digest,
            "device": m.device,
            "settings": settings,
        });
        Ok(Self {
            executable: frozen.executable().into(),
            public,
        })
    }

    /// Public, secret-free serialized command metadata. Mutating a returned copy
    /// cannot change the immutable settings rendered for the launcher.
    pub fn public_metadata(&self) -> &Value {
        &self.public
    }

    /// The final launcher substitutes three distinct protected descriptor numbers
    /// only after resolving the private frozen inputs outside this renderer.
    pub fn render_for_launcher(
        &self,
        fds: ProtectedDescriptorFds,
        wrapper: &Path,
    ) -> Result<RenderedCommand, RuntimeError> {
        Self::validate_wrapper_path(wrapper)?;
        let public = serde_json::to_string(&self.public).map_err(|_| RuntimeError::Unsupported)?;
        Ok(RenderedCommand {
            argv: vec![
                self.executable.clone(),
                // Installed .pth/sitecustomize hooks otherwise run before our
                // protected entry, even in isolated mode. Trusted package paths
                // must be composed explicitly without invoking site processing.
                // SPEC §9.1 / T21: -B, so no bytecode is written beside the
                // checked source for a later import to prefer.
                "-BIS".into(),
                wrapper.to_str().ok_or(RuntimeError::Unsupported)?.into(),
                "--public-settings-json".into(),
                public,
                "--launch-descriptor-fd".into(),
                fds.launch.to_string(),
                "--inference-credential-fd".into(),
                fds.inference.to_string(),
                "--admin-credential-fd".into(),
                fds.admin.to_string(),
            ],
            env: Default::default(),
        })
    }

    /// Service configuration supplies this path, never a candidate or HTTP request.
    /// The file and its directory chain are capyctl's own runtime helpers, so they
    /// follow the owner-only rule (owner decision 2026-09-23,
    /// `crate::owner_only`): owned by root or the service user, never writable
    /// by other, and group-writable only through the owner's private group.
    /// Owner writes remain inside the service trust boundary. Recheck
    /// immediately before passing protected descriptors to a child.
    pub fn validate_wrapper_path(path: &Path) -> Result<(), RuntimeError> {
        Self::validate_wrapper_path_with(path, &crate::owner_only::system_private_group)
    }

    /// `validate_wrapper_path` with the private-group lookup supplied, so the
    /// rule is testable without editing the account database.
    #[doc(hidden)]
    pub fn validate_wrapper_path_with(
        path: &Path,
        private_group: &crate::owner_only::PrivateGroup,
    ) -> Result<(), RuntimeError> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            let reject = || RuntimeError::Unsupported;
            if !path.to_str().is_some_and(absolute_path)
                || path.canonicalize().map_err(|_| reject())? != path
            {
                return Err(reject());
            }
            let service_uid = std::fs::metadata("/proc/self").map_err(|_| reject())?.uid();
            for (index, component) in path.ancestors().enumerate() {
                let metadata = std::fs::symlink_metadata(component).map_err(|_| reject())?;
                if (index == 0 && !metadata.is_file()) || (index != 0 && !metadata.is_dir()) {
                    return Err(reject());
                }
                crate::owner_only::check(&metadata, &[0, service_uid], private_group)
                    .map_err(|_| reject())?;
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (path, private_group);
            Err(RuntimeError::Unsupported)
        }
    }
}

impl fmt::Display for SglangLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SGLang launch {}", self.public)
    }
}

/// Most extra arguments the entry accepts (`runtime/sglang_entry.py`).
const MAX_EXTRA_ARGS: usize = 256;

/// ADR 0014 §2: the dtypes capyctl accepts by name, as the entry does.
const DTYPES: &[&str] = &["auto", "bfloat16", "float16", "float32"];

/// An engine-spelled value (quantization method, KV dtype): a short ASCII token.
fn engine_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn positive_i32(value: Option<u32>) -> bool {
    value.is_none_or(|value| value >= 1 && i32::try_from(value).is_ok())
}

/// ADR 0014 §2, §5, §6: the closed typed settings object the entry validates.
/// capyctl checks type, range and closure; whether the engine supports a value on
/// this checkpoint is the user's responsibility (ADR 0011). Reserved settings
/// are absent: the entry renders them from the binding, placement and grant.
fn public_settings(s: &SglangLaunchSettings) -> Result<Value, RuntimeError> {
    let common = &s.common;
    let memory = &s.memory;
    // ADR 0014 §5: SGLang's static pool (weights plus KV) is the request minus
    // the overhead margin; the entry turns it into `mem_fraction_static`. An
    // explicit request smaller than KV plus the placeholder margin (a declared
    // `resources:` Ready phase) cannot honour the margin: the static pool then
    // gets the declared KV cache, and never more than the whole request.
    if memory.request_bytes <= 0
        || memory.kv_cache_bytes <= 0
        || memory.kv_cache_bytes > memory.request_bytes
        || memory.margin_bytes < 0
        || memory.device_total_bytes.is_some_and(|total| total <= 0)
    {
        return Err(RuntimeError::Unsupported);
    }
    // Discrete GPU design §3, §6: a device request is `weights x 1.10 + kv`,
    // so on a discrete device the margin is a tenth of the weights share
    // (`(request - kv) / 11`) and the static pool holds the weights and the
    // KV cache. The unified placeholder margin (8 GiB) would leave a card's
    // static pool smaller than the weights it must load.
    let (margin_bytes, static_bytes) = capyctl_config::context_fit::static_pool_bytes(memory);
    let expected_restore = if s.cpu_weight_backup {
        "cpu_backup"
    } else {
        "disk_reload"
    };
    if common
        .dtype
        .as_deref()
        .is_some_and(|dtype| !DTYPES.contains(&dtype))
        || common
            .quantization
            .as_deref()
            .is_some_and(|q| !engine_token(q))
        || common
            .kv_cache_dtype
            .as_deref()
            .is_some_and(|k| !engine_token(k))
        || !positive_i32(common.context_length)
        || !positive_i32(common.max_concurrent_requests)
        || !positive_i32(s.max_total_tokens)
        || !positive_i32(s.max_mamba_cache_size)
        || s.chunked_prefill_size
            .is_some_and(|size| size == 0 || size < -1)
        || !(1..=1024).contains(&s.tokenizer_workers)
        || [&s.tool_call_parser, &s.reasoning_parser]
            .into_iter()
            .flatten()
            .any(|name| !capyctl_config::parsers::valid_value(name))
        // ADR 0014 A17: `resident` needs the memory saver and no backup.
        || (s.weight_restore != expected_restore
            && !(s.weight_restore == "resident" && s.memory_saver && !s.cpu_weight_backup))
        || s.extra_args.len() > MAX_EXTRA_ARGS
        || s.extra_args.iter().any(|token| {
            token.is_empty() || token.len() > 4096 || token.chars().any(char::is_control)
        })
    {
        return Err(RuntimeError::Unsupported);
    }
    // SPEC §8.2 / ADR 0014 §3: reserved names, configuration files and
    // duplicates are refused again at render; the entry rechecks after parsing.
    validate_rendered_args(Engine::Sglang, &s.extra_args, false)
        .map_err(|_| RuntimeError::Unsupported)?;
    let mut rendered = json!({
        "dtype": common.dtype,
        "quantization": common.quantization,
        "kv_cache_dtype": common.kv_cache_dtype,
        "context_length": common.context_length,
        "max_running_requests": common.max_concurrent_requests,
        "cuda_graphs": common.cuda_graphs,
        "language_model_only": common.language_model_only,
        "trust_remote_code": common.trust_remote_code,
        "max_total_tokens": s.max_total_tokens,
        "chunked_prefill_size": s.chunked_prefill_size,
        "tokenizer_workers": s.tokenizer_workers,
        "memory_saver": s.memory_saver,
        "cpu_weight_backup": s.cpu_weight_backup,
        "weight_restore": s.weight_restore,
        "memory": public_memory(memory, margin_bytes, static_bytes, s.static_allowance_bytes),
        "extra_args": s.extra_args,
    });
    // ADR 0014 amendment A14: present only when CapyCTL sized a hybrid
    // model's recurrent-state pool, so every other launch renders as before.
    if let Some(slots) = s.max_mamba_cache_size {
        rendered["max_mamba_cache_size"] = json!(slots);
    }
    // ADR 0024: present only when a parser was chosen, so every earlier launch
    // renders exactly as before. `none` is the deployment's switch, never a name.
    for (key, value) in [
        ("tool_call_parser", &s.tool_call_parser),
        ("reasoning_parser", &s.reasoning_parser),
    ] {
        if let Some(name) = value
            .as_deref()
            .filter(|name| *name != capyctl_config::parsers::OFF)
        {
            rendered[key] = json!(name);
        }
    }
    Ok(rendered)
}

/// The closed memory object. `device_total_bytes` is present only on a
/// discrete device (design §6): the entry then sizes `mem_fraction_static`
/// against the card's total instead of `MemAvailable`, and a unified launch
/// renders exactly as before.
///
/// ADR 0014, note on amendment A14 (found live 2026-10-04): on a discrete
/// device whose pools CapyCTL fixed, `static_allowance_bytes` is what the
/// fraction carries beyond the static pool, because SGLang takes its fraction
/// of the GPU memory free when it starts rather than of the card's total.
fn public_memory(
    memory: &capyctl_domain::launch::MemoryRequest,
    margin_bytes: i64,
    static_bytes: i64,
    static_allowance_bytes: Option<i64>,
) -> Value {
    let mut rendered = json!({
        "request_bytes": memory.request_bytes,
        "kv_cache_bytes": memory.kv_cache_bytes,
        "margin_bytes": margin_bytes,
        "static_bytes": static_bytes,
    });
    if let Some(total) = memory.device_total_bytes {
        rendered["device_total_bytes"] = json!(total);
        if let Some(allowance) = static_allowance_bytes.filter(|bytes| *bytes > 0) {
            rendered["static_allowance_bytes"] = json!(allowance);
        }
    }
    rendered
}

fn selector(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

/// The physical UUID shape `runtime/sglang_device.py` validates (`GPU-` +
/// 8-4-4-4 lowercase hex). The launcher sets the child's `CUDA_VISIBLE_DEVICES`
/// from this value, so a UUID the collector would not have observed is refused
/// before it can name a namespace.
fn physical_gpu_uuid(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("GPU-") else {
        return false;
    };
    rest.len() == 36
        && rest.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
        })
}

fn ulid(value: &str) -> bool {
    value.len() == 26
        && matches!(value.as_bytes()[0], b'0'..=b'7')
        && value
            .bytes()
            .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b))
}

/// The served name is the deployment's route (Spec §3), not a derived binding
/// artifact. The rule is ASCII printable only: a non-empty token of at most
/// 256 bytes whose characters are all in 0x21..=0x7E (no space, no control
/// characters, no non-ASCII code points). This mirrors the entry's check in
/// `runtime/sglang_entry.py`, which also requires `str.isascii()` and
/// `str.isprintable()`, so both validators refuse exactly the same inputs.
/// Shapes produced by the retired `candidate-{binding_id}` derivation are now
/// ordinary valid tokens: the rule is gone, not deprecated, and no special
/// case remains for it.
pub(crate) fn served_name_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.chars().all(|c| matches!(c, '\u{21}'..='\u{7E}'))
}

fn private_endpoint(value: &str) -> bool {
    let Some(port) = value.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    port.parse::<u16>()
        .is_ok_and(|number| number != 0 && number.to_string() == port)
}

fn absolute_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && value != "/"
        && Path::new(value).is_absolute()
        && !value.chars().any(char::is_control)
        && !value.split('/').any(|part| matches!(part, "." | ".."))
}

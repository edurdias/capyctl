//! Secret-free input to the protected Python entrypoint, not native engine argv.
//!
//! The wrapper must validate this closed semantic descriptor and map every field
//! through a verified pinned ServerArgs contract before any engine import. Native
//! flag spellings are deliberately not inferred here. The final launcher owns the
//! private launch/root descriptor and the two credential descriptors; this module
//! never resolves references, reads descriptors, or starts a process.

use crate::traits::{RenderedCommand, RuntimeError};
use mllm_config::effective::sglang::{
    NATIVE_CHECKPOINT_REVISION, NATIVE_SGLANG_RECIPE, NATIVE_SGLANG_SOURCE_REVISION,
};
use mllm_domain::launch::{NativeLaunch, SglangLaunchSettings};
use serde_json::{Value, json};
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
/// # use mllm_adapters::{sglang::SglangLaunch, PlanInput};
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
            || m.recipe != NATIVE_SGLANG_RECIPE
            || m.source_revision != NATIVE_SGLANG_SOURCE_REVISION
            || m.checkpoint_revision != NATIVE_CHECKPOINT_REVISION
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
        {
            return Err(RuntimeError::Unsupported);
        }
        validate_settings(s)?;

        // Qwen3-4B's pinned geometry: 36 layers, 8 KV heads, head dimension
        // 128, K and V, two-byte BF16. This excludes allocator overhead and is
        // only a necessary lower bound, never allocation or grant evidence.
        let minimum_kv_bytes = [8, 128, 2, 2, u64::from(s.max_total_tokens)]
            .into_iter()
            .try_fold(36_u64, u64::checked_mul)
            .ok_or(RuntimeError::Unsupported)?;
        let budget = u64::try_from(s.requested_budget.kv_cache_bytes)
            .map_err(|_| RuntimeError::Unsupported)?;
        if budget < minimum_kv_bytes {
            return Err(RuntimeError::Unsupported);
        }
        let bps = s.requested_budget.static_memory_fraction_bps;
        // Integer-only decimal rendering preserves every basis point; bytes
        // remain integer bytes and are never converted into a native CLI flag.
        let fraction = format!("{}.{:04}", bps / 10000, bps % 10000);
        let public = json!({
            "schema_version": 1,
            "kind": "sglang_launch",
            "engine": m.engine,
            "recipe": m.recipe,
            "source_revision": m.source_revision,
            "checkpoint_revision": m.checkpoint_revision,
            "binding_id": m.binding_id,
            "incarnation": m.incarnation,
            "endpoint": m.endpoint,
            "served_name": m.served_name,
            "rendered_settings_digest": m.rendered_settings_digest,
            "device": m.device,
            "settings": s,
            "minimum_kv_bytes": minimum_kv_bytes,
            "static_memory_fraction": fraction,
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
                "-IS".into(),
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
    /// The file and its directory chain must be owned by root or the service user
    /// and unwritable by other users. Owner writes remain inside the service trust
    /// boundary. Recheck immediately before passing protected descriptors to a child.
    pub fn validate_wrapper_path(path: &Path) -> Result<(), RuntimeError> {
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
                if (index == 0 && !metadata.is_file())
                    || (index != 0 && !metadata.is_dir())
                    || ![0, service_uid].contains(&metadata.uid())
                    || metadata.mode() & 0o022 != 0
                {
                    return Err(reject());
                }
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            Err(RuntimeError::Unsupported)
        }
    }
}

impl fmt::Display for SglangLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SGLang launch {}", self.public)
    }
}

fn validate_settings(s: &SglangLaunchSettings) -> Result<(), RuntimeError> {
    let SglangLaunchSettings {
        recipe,
        tensor_parallel_size: 1,
        data_parallel_size: 1,
        tokenizer_workers: 1,
        model_dtype,
        context_tokens: 4096,
        max_running_requests: 8,
        max_total_tokens: 4096,
        prefill_cuda_graphs: false,
        decode_cuda_graphs: false,
        memory_saver: true,
        cpu_weight_backup: false,
        speculative_decoding: false,
        lora: false,
        trust_remote_code: false,
        disaggregation: false,
        external_cache: false,
        cpu_kv_offload: false,
        native_grpc: false,
        weight_restore,
        requested_budget,
    } = s
    else {
        return Err(RuntimeError::Unsupported);
    };
    if recipe != NATIVE_SGLANG_RECIPE
        || model_dtype != "bfloat16"
        || weight_restore != "disk_reload"
        || !(1..=10000).contains(&requested_budget.static_memory_fraction_bps)
    {
        return Err(RuntimeError::Unsupported);
    }
    Ok(())
}

fn selector(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

fn ulid(value: &str) -> bool {
    value.len() == 26
        && matches!(value.as_bytes()[0], b'0'..=b'7')
        && value
            .bytes()
            .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b))
}

/// The served name is the deployment's route (Spec §3), not a derived binding
/// artifact: the entry and the coordinator's render both accept exactly the
/// same token here. A non-empty printable token, 1..=256 bytes, carrying no
/// whitespace and no control characters. No further shape is imposed, so the
/// deleted `candidate-{binding_id}` derivation is gone rather than deprecated.
pub(crate) fn served_name_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
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

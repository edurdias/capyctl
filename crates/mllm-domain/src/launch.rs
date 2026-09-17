//! Normalized, immutable engine launch choices. These are requests, not grants or evidence.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "engine", rename_all = "lowercase")]
pub enum ProfileLaunchSettings {
    Vllm(VllmLaunchSettings),
    Sglang(SglangLaunchSettings),
    Fake(FakeLaunchSettings),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VllmLaunchSettings {
    pub tensor_parallel_size: u32,
    pub pipeline_parallel_size: u32,
    pub enable_sleep_mode: bool,
    pub kv_cache_dtype: String,
    pub block_size_tokens: u32,
    pub cpu_offload_bytes: i64,
    pub requested_budget: VllmRequestedBudget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VllmRequestedBudget {
    pub kv_cache_bytes: i64,
    pub swap_space_bytes: i64,
    pub gpu_utilization_pct: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SglangLaunchSettings {
    pub recipe: String,
    pub tensor_parallel_size: u32,
    pub data_parallel_size: u32,
    pub tokenizer_workers: u32,
    pub model_dtype: String,
    pub context_tokens: u32,
    pub max_running_requests: u32,
    pub max_total_tokens: u32,
    pub prefill_cuda_graphs: bool,
    pub decode_cuda_graphs: bool,
    pub memory_saver: bool,
    pub cpu_weight_backup: bool,
    pub speculative_decoding: bool,
    pub lora: bool,
    pub trust_remote_code: bool,
    pub disaggregation: bool,
    pub external_cache: bool,
    pub cpu_kv_offload: bool,
    pub native_grpc: bool,
    pub weight_restore: String,
    pub requested_budget: SglangRequestedBudget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SglangRequestedBudget {
    pub kv_cache_bytes: i64,
    pub static_memory_fraction_bps: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FakeLaunchSettings;

/// Reviewed logical placement, not an observed CUDA index or physical UUID.
/// Native startup must independently resolve and corroborate this selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NativeDeviceSelection {
    pub host_id: String,
    pub hardware_fingerprint: String,
    pub device_id: String,
    pub memory_domain: String,
}

/// Redacted native launch description. Neither metadata nor its digest is send authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeLaunchMetadata {
    pub engine: String,
    pub recipe: String,
    pub source_revision: String,
    pub checkpoint_revision: String,
    pub binding_id: String,
    pub incarnation: String,
    pub endpoint: String,
    pub served_name: String,
    pub rendered_settings_digest: String,
    pub device: NativeDeviceSelection,
}

/// Trusted, process-local projection of a persisted launch descriptor.
///
/// Deliberately has no Debug, Display, or serialization implementation. Reading,
/// constructing, or retaining this value does not authorize a send. The controller
/// must separately own the current persisted arm's `New` outcome.
pub struct NativeLaunch {
    metadata: NativeLaunchMetadata,
    checkpoint_root: String,
    executable: String,
    inference_credential_ref: String,
    admin_credential_ref: String,
    settings: SglangLaunchSettings,
}
impl NativeLaunch {
    /// Internal cross-crate bridge. Call only with a validated persisted store read;
    /// this constructor does not supply proof of persistence or launch authority.
    #[doc(hidden)]
    pub fn from_frozen_store(
        metadata: NativeLaunchMetadata,
        checkpoint_root: String,
        executable: String,
        inference_credential_ref: String,
        admin_credential_ref: String,
        settings: SglangLaunchSettings,
    ) -> Self {
        Self {
            metadata,
            checkpoint_root,
            executable,
            inference_credential_ref,
            admin_credential_ref,
            settings,
        }
    }
    pub fn metadata(&self) -> &NativeLaunchMetadata {
        &self.metadata
    }
    /// Trusted launcher/adapter use only; never expose through a management DTO.
    #[doc(hidden)]
    pub fn checkpoint_root(&self) -> &str {
        &self.checkpoint_root
    }
    #[doc(hidden)]
    pub fn executable(&self) -> &str {
        &self.executable
    }
    #[doc(hidden)]
    pub fn inference_credential_ref(&self) -> &str {
        &self.inference_credential_ref
    }
    #[doc(hidden)]
    pub fn admin_credential_ref(&self) -> &str {
        &self.admin_credential_ref
    }
    pub fn settings(&self) -> &SglangLaunchSettings {
        &self.settings
    }
}

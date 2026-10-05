//! Normalized, immutable engine launch choices. These are requests, not grants or evidence.
//!
//! ADR 0014 §1: a deployment owns its `engine_config`; the host installation owns
//! only the executable, environment, security policy and host-fixed arguments.
//! These types are the resolved form of a deployment's `engine_config`, plus the
//! values capyctl derives from residency and host policy. Settings capyctl reserves
//! (ports, devices, ranks, memory grants, keys) never appear here: adapters
//! render them from the grant, placement and binding.

use serde::Serialize;
use std::collections::BTreeMap;

/// Owner decision 2026-10-02: the requests CapyCTL keeps in flight per
/// deployment (the router's bound), and so the sequences vLLM is started for
/// (`--max-num-seqs`) unless the deployment sets `max_concurrent_requests` or
/// the installation's host-fixed arguments set it. One constant, so the
/// router's bound and the engine's cannot drift apart.
pub const MAX_REQUESTS_PER_DEPLOYMENT: u32 = 32;

/// ADR 0023 §4 (amended 2026-10-03, owner decision): the requests TensorFold
/// decodes together (`--parallel`) unless the deployment sets
/// `max_concurrent_requests` or its arguments pass `--parallel`. Lower than
/// the router's bound: TensorFold sizes its drafter's buffers for every stream
/// at start, inside the declared memory (about 0.7 GiB a stream for
/// Qwen3.8-27B with DFlash2), and refuses a context that no longer fits. The
/// router's other requests wait in TensorFold's queue.
pub const TENSORFOLD_DEFAULT_PARALLEL: u32 = 8;

/// ADR 0014 amendment A16 (owner decision 2026-10-05): the requests SGLang
/// runs at once (`--max-running-requests`) on a hybrid model, and the
/// recurrent state CapyCTL sizes and reserves for them, unless the deployment
/// sets `max_concurrent_requests`. SGLang keeps a state slot set per running
/// request, so the reservation grows with the count: for Qwen3.8-27B with
/// DFlash2 the router's bound (32) held about 61 GiB of state. TensorFold's
/// default, for the same reason; the router's other requests wait in SGLang's
/// queue. A dense model keeps the router's bound.
pub const SGLANG_HYBRID_DEFAULT_RUNNING: u32 = TENSORFOLD_DEFAULT_PARALLEL;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "engine", rename_all = "lowercase")]
pub enum LaunchSettings {
    Vllm(VllmLaunchSettings),
    Sglang(SglangLaunchSettings),
    Tensorfold(TensorfoldLaunchSettings),
}

impl LaunchSettings {
    pub fn common(&self) -> &CommonEngineSettings {
        match self {
            Self::Vllm(settings) => &settings.common,
            Self::Sglang(settings) => &settings.common,
            Self::Tensorfold(settings) => &settings.common,
        }
    }

    pub fn memory(&self) -> &MemoryRequest {
        match self {
            Self::Vllm(settings) => &settings.memory,
            Self::Sglang(settings) => &settings.memory,
            Self::Tensorfold(settings) => &settings.memory,
        }
    }

    pub fn memory_mut(&mut self) -> &mut MemoryRequest {
        match self {
            Self::Vllm(settings) => &mut settings.memory,
            Self::Sglang(settings) => &mut settings.memory,
            Self::Tensorfold(settings) => &mut settings.memory,
        }
    }

    pub fn extra_args(&self) -> &[String] {
        match self {
            Self::Vllm(settings) => &settings.extra_args,
            Self::Sglang(settings) => &settings.extra_args,
            Self::Tensorfold(settings) => &settings.extra_args,
        }
    }

    pub fn provenance_mut(&mut self) -> &mut BTreeMap<String, SettingSource> {
        match self {
            Self::Vllm(settings) => &mut settings.provenance,
            Self::Sglang(settings) => &mut settings.provenance,
            Self::Tensorfold(settings) => &mut settings.provenance,
        }
    }

    pub fn provenance(&self) -> &BTreeMap<String, SettingSource> {
        match self {
            Self::Vllm(settings) => &settings.provenance,
            Self::Sglang(settings) => &settings.provenance,
            Self::Tensorfold(settings) => &settings.provenance,
        }
    }
}

/// SPEC §7 / T14: where an effective value came from when the deployment did
/// not state it. A value the deployment declared has no entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SettingSource {
    /// ADR 0014 §4: a safe default capyctl applies; the deployment may override it.
    #[serde(rename = "capyctl default")]
    CapyctlDefault,
    /// Computed by capyctl from other declared values (residency, host policy,
    /// memory arithmetic, checkpoint size). Not declarable.
    #[serde(rename = "derived")]
    Derived,
}

/// ADR 0014 §2: typed parameters common to every engine family. `None` means
/// the engine's own default applies. capyctl validates type and range only; whether
/// the engine supports a value on this checkpoint is the user's responsibility
/// (ADR 0011).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CommonEngineSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dtype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kv_cache_dtype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_requests: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cuda_graphs: Option<bool>,
    pub language_model_only: bool,
    pub trust_remote_code: bool,
}

/// ADR 0014 §5 (owner decision P2): the per-instance memory request.
/// Reservations always use `request_bytes`, never a sampled value (SPEC §7.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryRequest {
    /// The per-instance reservation, declared or derived.
    pub request_bytes: i64,
    /// The KV cache the engine is asked to size, declared or derived.
    pub kv_cache_bytes: i64,
    /// The per-family overhead margin used when anything was derived.
    pub margin_bytes: i64,
    /// Sum of weight-file sizes from the checkpoint manifest (ADR 0014 §7),
    /// when known at resolution. Recorded so a snapshot re-derives identically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weights_bytes: Option<i64>,
    /// Owner decision 2026-09-23 (startup memory budget): the per-instance
    /// peak admission reserves from arm until Ready (the cold phase, ADR 0007),
    /// declared as `memory.startup` or the conservative placeholder default.
    /// A launch reached Ready drops to `request_bytes`. Absent when the
    /// deployment declares its `resources:` phases, and in a revision resolved
    /// before the budget existed (its cold phase is the request).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_bytes: Option<i64>,
    /// Discrete GPU design §6 (ADR 0019): the total memory of the GPU a launch
    /// on a device domain runs on, as the host observed it for that launch.
    /// Never part of a resolved configuration: resolution leaves it `None`, and
    /// only the host that launches fills it in its own copy, so SGLang's static
    /// fraction and vLLM's `--gpu-memory-utilization` are fractions of the card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_total_bytes: Option<i64>,
    /// ADR 0019 (parity rule): the engine's CUDA context and graphs, charged
    /// beside the request in every active derived phase on every host shape.
    /// Absent in a revision frozen before the charge existed (or one whose
    /// `resources:` are declared), which re-derives without it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overhead_bytes: Option<i64>,
    /// ADR 0014 amendment A8: the first-start graph allowance a derived
    /// startup placeholder carries (the CUDA graphs of the checkpoint and of a
    /// draft model). Absent when the startup is declared or follows a device
    /// request, when `resources:` are declared, and in a revision frozen
    /// before the allowance existed, which re-derives without it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_graphs_bytes: Option<i64>,
    /// ADR 0014 amendment A16: one request slot of a hybrid model's recurrent
    /// state on SGLang, a checkpoint fact the host measured from
    /// `config.json` beside the weights. Recorded so a snapshot re-derives
    /// identically; absent for any other model or engine, and in a revision
    /// measured before the fact existed, which keeps its sizing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_slot_bytes: Option<i64>,
    /// ADR 0014 amendment A16: the recurrent state a derived request holds
    /// for its running requests (their slots, the padding slot and any
    /// draft-token states). Absent when the request holds none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_bytes: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VllmLaunchSettings {
    pub common: CommonEngineSettings,
    pub memory: MemoryRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_size_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_num_batched_tokens: Option<u32>,
    /// ADR 0024: the deployment's tool-call and reasoning parser choice:
    /// a parser name, or `none` to turn it off. `None` is `auto`: capyctl
    /// chooses by model family where the checkpoint is read, at launch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_parser: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_parser: Option<String>,
    /// Derived, reserved: sleep (development) mode is on only where the host
    /// leaves deep parking enabled and the deployment parks (SPEC §6.2, §9.1).
    pub enable_sleep_mode: bool,
    /// ADR 0014 §6: ordinary engine arguments the deployment accepted, already
    /// checked against the reserved and sensitive lists.
    pub extra_args: Vec<String>,
    pub provenance: BTreeMap<String, SettingSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SglangLaunchSettings {
    pub common: CommonEngineSettings,
    pub memory: MemoryRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u32>,
    /// ADR 0014 amendment A14: the recurrent-state slots of a hybrid model,
    /// derived where the checkpoint is read at launch; never declared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_mamba_cache_size: Option<u32>,
    /// SGLang accepts `-1` to disable chunked prefill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunked_prefill_size: Option<i32>,
    /// ADR 0014 §4: capyctl default 1 (live finding), overridable.
    pub tokenizer_workers: u32,
    /// ADR 0024: the deployment's tool-call and reasoning parser choice:
    /// a parser name, or `none` to turn it off. `None` is `auto`: capyctl
    /// chooses by model family where the checkpoint is read, at launch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_parser: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_parser: Option<String>,
    /// Derived from residency (ADR 0010): SGLang takes its park strategy at
    /// launch, so these are startup settings, not park parameters.
    pub memory_saver: bool,
    pub cpu_weight_backup: bool,
    pub weight_restore: String,
    pub extra_args: Vec<String>,
    pub provenance: BTreeMap<String, SettingSource>,
}

/// ADR 0023 §4: a TensorFold deployment's resolved settings. TensorFold has
/// no park strategy, so nothing here is derived from residency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TensorfoldLaunchSettings {
    pub common: CommonEngineSettings,
    pub memory: MemoryRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    pub extra_args: Vec<String>,
    pub provenance: BTreeMap<String, SettingSource>,
}

/// Reviewed logical placement, not an observed CUDA index or physical UUID.
/// Native startup must independently resolve and corroborate this selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NativeDeviceSelection {
    pub host_id: String,
    pub hardware_fingerprint: String,
    pub device_id: String,
    pub memory_domain: String,
    /// The host policy's service-authorized physical UUID for this device.
    /// It is a guarded launch parameter, not descriptor content: the launcher
    /// sets the engine child's `CUDA_VISIBLE_DEVICES` from it (profile env
    /// rejects that name, `engine_policy.rs::SAFE_ENV`), and the native entry
    /// corroborates the inherited namespace against it and the placement
    /// digest. Deliberately never serialized: the descriptor's device object
    /// is closed at exactly the four reviewed selectors above.
    #[serde(skip_serializing)]
    pub physical_gpu_uuid: Option<String>,
    /// Discrete GPU design §7 (review decision): on a host with a choice of
    /// GPU that published no UUID for this one, its driver index; the guarded
    /// launcher then pins the child with `CUDA_DEVICE_ORDER=PCI_BUS_ID` and
    /// `CUDA_VISIBLE_DEVICES=<index>` rather than handing it every GPU. A
    /// launch parameter like the UUID, never descriptor content.
    #[serde(skip_serializing)]
    pub cuda_pci_index: Option<u32>,
}

/// Redacted native launch description. Neither metadata nor its digest is send authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeLaunchMetadata {
    pub engine: String,
    pub recipe: String,
    pub checkpoint_revision: String,
    pub binding_id: String,
    pub incarnation: String,
    pub endpoint: String,
    pub served_name: String,
    pub rendered_settings_digest: String,
    /// The host's service-authorized device inventory digest, or `None` when
    /// the host published none. Carried in the private descriptor only, so the
    /// native entry can assert placement against it; never a public field.
    pub placement_digest: Option<String>,
    pub device: NativeDeviceSelection,
}

/// Trusted, process-local projection of a persisted launch descriptor.
///
/// Deliberately has no Debug, Display, or serialization implementation. Reading,
/// constructing, or retaining this value does not authorize a send. The controller
/// must separately own the current persisted arm's `New` outcome.
///
/// `Clone` exists so an application-supplied source can hand out the same frozen
/// value on every call. Copying it still authorizes nothing.
#[derive(Clone)]
pub struct NativeLaunch {
    metadata: NativeLaunchMetadata,
    checkpoint_root: String,
    executable: String,
    inference_credential_ref: String,
    admin_credential_ref: String,
    settings: SglangLaunchSettings,
    cuda_home: Option<String>,
    build_env: std::collections::BTreeMap<String, String>,
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
            cuda_home: None,
            build_env: std::collections::BTreeMap::new(),
        }
    }
    /// SPEC §13.3 amendment (owner decision 2026-09-25): the profile's
    /// host-approved CUDA toolkit root, for the engine's PATH and `CUDA_HOME`;
    /// `build_env` holds the profile `env` build-limit overrides
    /// (`MAX_JOBS`, `FLASHINFER_NVCC_THREADS`).
    #[doc(hidden)]
    pub fn with_toolchain(
        mut self,
        cuda_home: Option<String>,
        build_env: std::collections::BTreeMap<String, String>,
    ) -> Self {
        self.cuda_home = cuda_home;
        self.build_env = build_env;
        self
    }
    #[doc(hidden)]
    pub fn cuda_home(&self) -> Option<&str> {
        self.cuda_home.as_deref()
    }
    #[doc(hidden)]
    pub fn build_env(&self) -> &std::collections::BTreeMap<String, String> {
        &self.build_env
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

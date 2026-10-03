//! Pure resolution of strict manifests into immutable, serializable launch inputs.

mod checkpoint;
mod core;
mod current_policy;
mod declared_resources;
mod engine_config;
mod legacy;
mod snapshot;
mod startup;
mod timeouts;
pub use current_policy::{compose_current_resource_controls, deployment_command_fingerprint};
pub use declared_resources::validate_declared_resources;
pub use engine_config::{
    default_startup_bytes, host_backed_copy_bytes, overhead_margin, resolve_memory,
    resolve_startup, startup_graph_allowance, CheckpointFacts, MemoryInputs, ResolvedMemory,
    ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES, ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES,
    HOST_BACKED_COPY_FACTOR, LEGACY_STARTUP_WEIGHTS_FACTOR,
    PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES, PARKED_RESIDUAL_PLACEHOLDER_BYTES,
    SGLANG_OVERHEAD_MARGIN_BYTES, STARTUP_GRAPH_ALLOWANCE_BYTES, STARTUP_WEIGHTS_FACTOR,
    VLLM_OVERHEAD_MARGIN_BYTES,
};
pub use legacy::{
    is_legacy_effective, legacy_engine_config, legacy_retained_deployment,
    migrate_legacy_effective, strip_legacy_launch_settings, LegacyEffectiveMigration,
    LegacyRefusal,
};
pub use snapshot::decode_effective_snapshot;
// Owner decision 2026-09-23: the startup memory budget.
pub use startup::{
    measurable, startup_budget, validate_declared_startup, StartupBudget, StartupProvenance,
};
// Owner decision 2026-09-22 (1), ADR 0014 amendment A1: lifecycle timeouts.
pub use timeouts::{
    derived_initialize_ms, derived_wake_ms, lifecycle_windows, validate_declared_timeouts,
    DeploymentTimeouts, TimeoutBasis, TimeoutSource, INITIALIZE_BASE_MS, INITIALIZE_CAP_MS,
    INITIALIZE_PER_GB_MS, MIN_INITIALIZE_MS, MIN_WAKE_MS, PENDING_INITIALIZE_MS, PENDING_WAKE_MS,
    STARTUP_WARMUP_MS, STOP_WINDOW_MS, TENSORFOLD_FIRST_BUILD_MS, WAKE_BASE_MS, WAKE_CAP_MS,
    WAKE_PER_GB_MS,
};
// ADR 0014 §7 (WE3): checkpoint identity.
pub use checkpoint::{
    checkpoint_location, declared_checkpoint_digest, drafter_location, is_checkpoint_digest,
    resolve_snapshot_with_checkpoint, CheckpointLocation, DrafterLocation,
    CHECKPOINT_DIGEST_PREFIX,
};

use crate::engine_policy::{
    normalize_option_name, validate_profile_args, validate_profile_env, ExtraArgsPolicy,
};
use crate::resource_controls::{ResourceContext, ResourceControls};
use crate::{ConfigError, ConfigErrorCode};
use capyctl_domain::launch::LaunchSettings;
use capyctl_domain::resources as domain;
use engine_config::RawEngineConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn invalid(path: impl Into<String>, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

const DEFAULT_PENDING_PER_DEPLOYMENT: u32 = 64;
const DEFAULT_PENDING_TOTAL: u32 = 256;
const DEFAULT_QUEUED_BYTES: i64 = 64 << 20;
const DEFAULT_REQUEST_DEADLINE_MS: i64 = 600_000;
const DEFAULT_ADMISSION_WINDOW_MS: i64 = 2_000;
/// SPEC §10: how long a relayed stream may go without a backend event before
/// the router gives up on it. Not a cap on a progressing stream's length.
pub const DEFAULT_STREAM_IDLE_MS: i64 = 120_000;
const DEFAULT_OBSERVATION_TTL_MS: i64 = 2_000;
const DEFAULT_PLANNER_STATES: u32 = 4_096;
const DEFAULT_MAX_PARKED: u32 = 16;

fn parse_decimal_unit(text: &str, units: &[(&str, i128)], path: &str) -> Result<i64, ConfigError> {
    let (number, multiplier) = units
        .iter()
        .find_map(|(suffix, multiplier)| text.strip_suffix(suffix).map(|n| (n, *multiplier)))
        .ok_or_else(|| {
            ConfigError::new(
                ConfigErrorCode::InvalidUnit,
                path,
                format!("invalid unit `{text}`"),
            )
        })?;
    if number.is_empty() || number.starts_with('-') || number.starts_with('+') {
        return Err(ConfigError::new(
            ConfigErrorCode::InvalidUnit,
            path,
            format!("invalid quantity `{text}`"),
        ));
    }
    let mut parts = number.split('.');
    let whole = parts.next().unwrap();
    let fraction = parts.next();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.is_some_and(|f| f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(ConfigError::new(
            ConfigErrorCode::InvalidUnit,
            path,
            format!("invalid quantity `{text}`"),
        ));
    }
    let denominator = match fraction {
        Some(f) => 10_i128
            .checked_pow(
                f.len()
                    .try_into()
                    .map_err(|_| invalid(path, "quantity precision overflow"))?,
            )
            .ok_or_else(|| invalid(path, "quantity precision overflow"))?,
        None => 1,
    };
    let whole: i128 = whole
        .parse()
        .map_err(|_| invalid(path, "quantity overflow"))?;
    let frac: i128 = fraction.unwrap_or("").parse().unwrap_or(0);
    let numerator = whole
        .checked_mul(denominator)
        .and_then(|v| v.checked_add(frac))
        .ok_or_else(|| invalid(path, "quantity overflow"))?;
    let scaled = numerator
        .checked_mul(multiplier)
        .ok_or_else(|| invalid(path, "quantity overflow"))?;
    if scaled % denominator != 0 {
        return Err(ConfigError::new(
            ConfigErrorCode::InvalidUnit,
            path,
            "quantity is not an integral base unit",
        ));
    }
    i64::try_from(scaled / denominator).map_err(|_| invalid(path, "quantity overflow"))
}

pub fn parse_bytes(text: &str) -> Result<i64, ConfigError> {
    parse_decimal_unit(
        text,
        &[
            ("TiB", 1_i128 << 40),
            ("GiB", 1_i128 << 30),
            ("MiB", 1_i128 << 20),
            ("KiB", 1_i128 << 10),
            ("B", 1),
        ],
        "bytes",
    )
}

pub fn parse_duration_ms(text: &str) -> Result<i64, ConfigError> {
    parse_decimal_unit(
        text,
        &[("ms", 1), ("s", 1_000), ("m", 60_000), ("h", 3_600_000)],
        "duration",
    )
}

pub use crate::engine_policy::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sharing {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveDeployment {
    pub schema_version: u32,
    pub name: String,
    pub model: ModelIdentity,
    pub routes: Vec<String>,
    pub recipe: String,
    pub residency: Residency,
    pub recovery: Recovery,
    pub selected_devices: Vec<DeviceClaim>,
    pub resources: RecipeFootprints,
    pub request_deadline_ms: i64,
    /// ADR 0014 amendment A1: the Initialize and wake timeouts, declared or
    /// derived from the checkpoint's weights, with provenance (T14). Not part
    /// of the recipe fingerprint: it bounds operations, not what is launched.
    pub timeouts: DeploymentTimeouts,
    /// ADR 0014 §1: the deployment's resolved engine configuration, with the
    /// values capyctl derived or defaulted named in its provenance (T14).
    pub engine_config: LaunchSettings,
    pub profile: RuntimeProfile,
    pub host: HostPolicy,
    pub recipe_fingerprint: String,
}

/// Where a deployment's weights come from, per SPEC §7 and ADR 0008.
pub use crate::model_source::{Archive, ModelSource, ModelSourcePolicy, SourceSwitch};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelIdentity {
    pub source: ModelSource,
    /// The absolute local path the source resolves to, or `None` where it does not
    /// resolve to one yet. Read it through [`ModelIdentity::require_resolved_path`]
    /// rather than unwrapping, so a caller that needs a real file fails closed.
    pub resolved_path: Option<String>,
    pub content_fingerprint: String,
    pub revision: String,
}

impl ModelIdentity {
    /// The local path, or `NotMaterializable` for a source that has none.
    ///
    /// SPEC §13.3: a launch needs a directory on disk. Anything that cannot name
    /// one must refuse rather than invent a cache location.
    pub fn require_resolved_path(&self) -> Result<&str, ConfigError> {
        self.resolved_path.as_deref().ok_or_else(|| {
            ConfigError::new(
                ConfigErrorCode::NotMaterializable,
                "model.source",
                "this model source names no local path here; a remote source \
                 resolves to its directory in a host's model store",
            )
        })
    }
}

/// Which park a deployment asks for, per SPEC §6.2.
///
/// `auto` from the spec is deliberately absent: selecting a tier at runtime is a
/// fallback ladder, and SGLang cannot implement one because its memory-saver and
/// weights-CPU-backup are startup flags that a running engine cannot acquire.
/// `deep_required` is absent for the same reason it is unnecessary — a declared tier
/// the profile or host cannot deliver is already a validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Residency {
    /// Stop and initialize again. First-class behaviour, including for backends with
    /// no qualified memory-release API.
    RestartOnly,
    /// Weights retained in host RAM, KV dropped. Frees nothing where device and host
    /// memory are one pool, which the host check refuses (ADR 0010 decision 5).
    HostBacked,
    /// Weights and KV released; weights re-read from the checkpoint on wake.
    Deep,
}

impl Residency {
    /// Whether this tier parks at all, as opposed to stopping and starting again.
    pub fn parks(self) -> bool {
        matches!(self, Self::HostBacked | Self::Deep)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recovery {
    Reconcile,
    ColdRestart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceClaim {
    pub id: String,
    pub sharing: Sharing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Allocation {
    pub domain: String,
    pub bytes: i64,
    pub host_kv_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PhaseFootprint {
    pub allocations: Vec<Allocation>,
    pub devices: Vec<DeviceClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecipeFootprints {
    pub cold: PhaseFootprint,
    pub ready: PhaseFootprint,
    pub parking: PhaseFootprint,
    pub parked: PhaseFootprint,
    pub wake: PhaseFootprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeProfile {
    pub engine: Engine,
    pub revision: u64,
    pub executable: String,
    pub build_fingerprint: String,
    /// ADR 0014 §1: host-fixed arguments of the installation.
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// SPEC §13.3 amendment (owner decision 2026-09-25): the host-approved CUDA
    /// toolkit root. The engine gets `<cuda_home>/bin` on PATH and `CUDA_HOME`;
    /// without it the engine PATH stays minimal. Omitted from serialization
    /// when absent, so existing snapshots and fingerprints do not change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cuda_home: Option<String>,
    pub security: Security,
    pub log_policy: LogPolicy,
}

/// Whether this profile may park at all.
/// SPEC §9.1 / T21 / ADR 0012: deep parking is enabled unless host policy
/// forbids it; `deep_park: disabled` is the host opt-out. Enablement never
/// relaxes the mandatory controls (loopback engine listener, per-launch key,
/// guard middleware, no control path through ingress or the router).
/// Legacy `experimental_controls` remains rejected by `Security`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeepPark {
    #[default]
    Enabled,
    Disabled,
}

impl DeepPark {
    pub fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// SPEC §7 / T14: where a profile's `deep_park` value came from. Derived at
/// resolution, never declared: a host document naming it is refused as an
/// unknown field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeepParkSource {
    /// The host policy declared `deep_park`.
    #[default]
    HostPolicy,
    /// The host policy omitted it and the ADR 0012 default applied.
    Default,
}

impl DeepParkSource {
    pub fn is_host_policy(&self) -> bool {
        matches!(self, Self::HostPolicy)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawSecurity")]
pub struct Security {
    pub deep_park: DeepPark,
    /// SPEC §7 / T14: shown only when the value was defaulted, so a profile that
    /// declares the switch serializes exactly as it did before provenance existed.
    #[serde(skip_serializing_if = "DeepParkSource::is_host_policy")]
    pub deep_park_source: DeepParkSource,
    /// Whether this profile may run an engine flag that executes Python shipped
    /// inside a checkpoint. SPEC §3: off unless the host says otherwise.
    pub trust_remote_code: bool,
    pub credential_ref: Option<String>,
    pub admin_credential_ref: Option<String>,
    /// ADR 0014 §6 (owner decision Q10): deployment extra arguments are allowed
    /// unless the host denies them.
    pub extra_args: ExtraArgsPolicy,
    /// ADR 0014 §8: security-sensitive options approved by name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub approved_options: Vec<String>,
    /// ADR 0014 §8: directories an approved path option may name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub approved_paths: Vec<String>,
    /// ADR 0008 (owner decision 2026-09-23): what a launch does when the
    /// installation no longer measures to the fingerprint recorded at
    /// registration. Shown only when the host declared `refuse`, so a profile
    /// without it serializes exactly as before.
    #[serde(skip_serializing_if = "InstallationDrift::is_warn")]
    pub installation_drift: InstallationDrift,
}

/// ADR 0008 (owner decision 2026-09-23): host policy for installation drift.
/// `warn` (the default) flags drift in the host's status and the event journal
/// and launches; `refuse` refuses the launch with the closed reason
/// `installation_drift` before any effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallationDrift {
    #[default]
    Warn,
    Refuse,
}

impl InstallationDrift {
    pub fn is_warn(&self) -> bool {
        matches!(self, Self::Warn)
    }
}

/// The declared shape of a profile's `security` block. `deep_park` is optional
/// here so resolution can tell a declared value from the ADR 0012 default.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecurity {
    // An explicit `null` is not an omission: it is refused like any other
    // value that is neither `enabled` nor `disabled`.
    #[serde(default, deserialize_with = "declared_deep_park")]
    deep_park: Option<DeepPark>,
    #[serde(default)]
    trust_remote_code: bool,
    credential_ref: Option<String>,
    admin_credential_ref: Option<String>,
    #[serde(default)]
    extra_args: ExtraArgsPolicy,
    #[serde(default)]
    approved_options: Vec<String>,
    #[serde(default)]
    approved_paths: Vec<String>,
    #[serde(default)]
    installation_drift: InstallationDrift,
}

fn declared_deep_park<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<DeepPark>, D::Error> {
    DeepPark::deserialize(deserializer).map(Some)
}

impl From<RawSecurity> for Security {
    fn from(raw: RawSecurity) -> Self {
        let (deep_park, deep_park_source) = match raw.deep_park {
            Some(declared) => (declared, DeepParkSource::HostPolicy),
            None => (DeepPark::default(), DeepParkSource::Default),
        };
        Self {
            deep_park,
            deep_park_source,
            trust_remote_code: raw.trust_remote_code,
            credential_ref: raw.credential_ref,
            admin_credential_ref: raw.admin_credential_ref,
            extra_args: raw.extra_args,
            approved_options: raw.approved_options,
            approved_paths: raw.approved_paths,
            installation_drift: raw.installation_drift,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogPolicy {
    pub max_file_bytes: i64,
    pub retained_files: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostPolicy {
    pub name: String,
    pub hardware_fingerprint: String,
    pub environment_fingerprint: String,
    /// The host's published NVIDIA device inventory digest
    /// (`runtime/sglang_device.collect_inventory()` schema
    /// `capyctl-nvidia-inventory-v1`), stated at boot like the fingerprints.
    /// Optional: a host that has not published one launches with placement
    /// unasserted and fails closed at the native placement gate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_inventory_digest: Option<String>,
    /// Absolute directory this host keeps model weights under. SPEC §7: a relative
    /// local model path is resolved against it. A published host document
    /// always states it: the roles fill `~/models` (or `CAPYCTL_MODELS_ROOT`,
    /// `--models-root`) before publishing (owner decision 2026-09-25).
    pub model_store: PathBuf,
    pub domains: BTreeMap<String, DomainPolicy>,
    pub devices: BTreeMap<String, DevicePolicy>,
    pub max_parked: u32,
    pub observation_ttl_ms: i64,
    pub device_sharing: Sharing,
    pub endpoint_port_range: PortRange,
    pub planner_max_states: u32,
    pub queue: QueuePolicy,
    /// ADR 0008: which remote model sources this host materializes, and the
    /// model store's ceiling for them. Encoded only when stated.
    #[serde(skip_serializing_if = "ModelSourcePolicy::is_default")]
    pub model_sources: ModelSourcePolicy,
}

/// Whether a domain's device memory and host memory are one physical pool.
///
/// On a unified-memory host such as a GB10, retaining a weight backup "in host RAM"
/// allocates from the same pool the device allocates from, so it frees nothing. Only
/// the operator registering the host knows this; it must not be inferred from a
/// domain's name or from which limits are set. SPEC §6.2's host-backed park is
/// meaningful only where this is `Distinct`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainMemory {
    Unified,
    Distinct,
    /// ADR 0019: a discrete GPU's own memory. The domain names its device, and
    /// host RAM is a separate `distinct` system domain beside it.
    Device,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DomainPolicy {
    pub managed_limit: i64,
    pub free_reserve: i64,
    pub host_kv_limit: Option<i64>,
    pub parked_limit: Option<i64>,
    pub memory: DomainMemory,
    /// ADR 0019: the device whose memory this is. `Some` exactly when `memory` is
    /// `Device`; omitted from the encoding otherwise, so a unified or distinct
    /// domain encodes exactly as it did before device domains existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicePolicy {
    pub domain: String,
    pub sharing: Sharing,
    /// The device's physical GPU UUID as the `capyctl-nvidia-inventory-v1`
    /// collector observed it. Service-authorized placement evidence: the
    /// guarded launcher sets the engine child's `CUDA_VISIBLE_DEVICES` from
    /// it, and the native placement gate corroborates it against a freshly
    /// collected inventory. Optional: a host that publishes no inventory
    /// publishes no UUID, and placement then fails closed at the gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_gpu_uuid: Option<String>,
}
/// Discrete GPU design §7 (review decision): how an engine child's CUDA
/// namespace is narrowed to the one GPU its launch selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CudaNamespace {
    /// `CUDA_VISIBLE_DEVICES` set to the physical UUID the host published.
    Uuid(String),
    /// `CUDA_DEVICE_ORDER=PCI_BUS_ID` and `CUDA_VISIBLE_DEVICES` set to the
    /// driver index the host published the GPU under (`gpuN`). `nvidia-smi`
    /// numbers GPUs in PCI bus order; CUDA's default order is fastest first,
    /// so without the order variable the index could name another GPU.
    PciIndex(u32),
}

impl CudaNamespace {
    /// The variables that narrow the engine child to this GPU.
    pub fn environment(&self) -> Vec<(&'static str, String)> {
        match self {
            CudaNamespace::Uuid(uuid) => vec![("CUDA_VISIBLE_DEVICES", uuid.clone())],
            CudaNamespace::PciIndex(index) => vec![
                ("CUDA_DEVICE_ORDER", "PCI_BUS_ID".into()),
                ("CUDA_VISIBLE_DEVICES", index.to_string()),
            ],
        }
    }
}

/// A launch that must be pinned to one GPU names no GPU it can pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the selected GPU has neither a published UUID nor a gpuN index to pin the engine to")]
pub struct UnpinnableDevice;

impl EffectiveDeployment {
    /// The directory this deployment's checkpoint must be inside: the host's
    /// model store for a local source, its sources store for a remote one
    /// (ADR 0008; owner decision 2026-09-25 gives downloads a store of their
    /// own). Whoever opens the checkpoint checks containment against it.
    pub fn checkpoint_store(&self) -> &Path {
        match self.model.source {
            ModelSource::Local { .. } => &self.host.model_store,
            _ => self.host.model_sources.root(&self.host.model_store),
        }
    }

    /// Discrete GPU design §7 (review decision): the namespace a launch's
    /// engine child is narrowed to. Where there is a choice of GPU (several
    /// devices) or the GPU is a discrete device domain, the selected GPU is
    /// always pinned: by the physical UUID the host published, else by its
    /// `gpuN` index in PCI bus order. A launch is never handed every GPU, so
    /// with several devices one it cannot name is [`UnpinnableDevice`].
    /// `Ok(None)` keeps the agent's own namespace: a one-device unified host
    /// (a GB10), or a lone discrete GPU with no name to pin.
    pub fn cuda_namespace(&self) -> Result<Option<CudaNamespace>, UnpinnableDevice> {
        let several = self.host.devices.len() > 1;
        let unpinned = || {
            if several {
                Err(UnpinnableDevice)
            } else {
                Ok(None)
            }
        };
        let [claim] = self.selected_devices.as_slice() else {
            return unpinned();
        };
        let Some(device) = self.host.devices.get(&claim.id) else {
            return unpinned();
        };
        let discrete = self
            .host
            .domains
            .get(&device.domain)
            .is_some_and(|domain| domain.memory == DomainMemory::Device);
        if !several && !discrete {
            return Ok(None);
        }
        if let Some(uuid) = &device.physical_gpu_uuid {
            return Ok(Some(CudaNamespace::Uuid(uuid.clone())));
        }
        match gpu_index(&claim.id) {
            Some(index) => Ok(Some(CudaNamespace::PciIndex(index))),
            None => unpinned(),
        }
    }

    /// Discrete GPU design §6 (ADR 0019): the device domain this deployment's
    /// Ready footprint charges, as the driver index of the GPU the domain names
    /// (`gpuN`) and the bytes the Ready phase holds there. `None` on a unified
    /// or host-only footprint. A device domain whose device is not a `gpuN` id
    /// has no index (`Some((None, bytes))`): no sample can name it.
    pub fn ready_device_allocation(&self) -> Option<(Option<u32>, i64)> {
        self.resources
            .ready
            .allocations
            .iter()
            .find_map(|allocation| {
                let domain = self.host.domains.get(&allocation.domain)?;
                (domain.memory == DomainMemory::Device).then(|| {
                    (
                        domain.device.as_deref().and_then(gpu_index),
                        allocation.bytes,
                    )
                })
            })
    }

    /// Discrete GPU design §6: state the observed total of the card a launch on
    /// a device domain runs on, in this launch's own copy of the settings.
    /// `totals` maps a driver index to the card's total bytes (a GPU sample).
    /// A deployment that charges no device domain is left unchanged; one whose
    /// card the sample does not report is [`DeviceTotalUnknown`]: sizing it
    /// against anything else would size the engine against the wrong memory.
    pub fn with_device_total(
        mut self,
        totals: impl Fn(u32) -> Option<i64>,
    ) -> Result<Self, DeviceTotalUnknown> {
        let Some((index, _)) = self.ready_device_allocation() else {
            return Ok(self);
        };
        let total = index
            .and_then(totals)
            .filter(|total| *total > 0)
            .ok_or(DeviceTotalUnknown)?;
        self.engine_config.memory_mut().device_total_bytes = Some(total);
        Ok(self)
    }
}

/// Discrete GPU design §6: a launch on a device domain whose card total was not
/// observed. The launch is refused rather than sized against host memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the total memory of the GPU this launch runs on was not observed")]
pub struct DeviceTotalUnknown;

/// The driver index a `gpuN` device id names; `None` for any other id.
fn gpu_index(device_id: &str) -> Option<u32> {
    device_id
        .strip_prefix("gpu")
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))?
        .parse()
        .ok()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueuePolicy {
    pub max_pending_per_deployment: u32,
    pub max_pending_total: u32,
    pub max_buffered_bytes_total: i64,
    pub request_deadline_ms: i64,
    pub admission_window_ms: i64,
    /// SPEC §10: the idle bound between a relayed stream's backend events
    /// (`queue.stream_idle_timeout`). Omitted from the encoding at its default,
    /// so policies published before it existed keep their identity.
    #[serde(skip_serializing_if = "is_default_stream_idle")]
    pub stream_idle_ms: i64,
}

fn is_default_stream_idle(value: &i64) -> bool {
    *value == DEFAULT_STREAM_IDLE_MS
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentInput {
    schema_version: u32,
    kind: String,
    name: String,
    model: RawModel,
    routes: Vec<String>,
    runtime_profile: String,
    /// Owner decision 2026-09-25: optional; the revision the host publishes
    /// (`deployment_defaults::for_host`).
    #[serde(default)]
    runtime_profile_revision: Option<u64>,
    recipe: String,
    /// Owner decision 2026-09-25: optional; chosen by host type and checkpoint
    /// (`deployment_defaults::default_residency`).
    #[serde(default)]
    residency: Option<Residency>,
    recovery: Recovery,
    /// Owner decision 2026-09-25: optional; the host's first GPU, or one per
    /// GPU on a discrete host (`deployment_defaults::for_host`).
    #[serde(default)]
    devices: Option<Vec<DeviceClaim>>,
    /// ADR 0014 §5: optional; derived from `engine_config.memory` when omitted.
    #[serde(default)]
    resources: Option<RawRecipe>,
    request_deadline: Option<String>,
    /// ADR 0014 amendment A1: optional; derived when omitted.
    #[serde(default)]
    timeouts: Option<timeouts::RawTimeouts>,
    #[serde(default)]
    engine_config: RawEngineConfig,
    /// ADR 0013 §2: instance count, placement constraints and the `host`
    /// shorthand are deployment-level and shared by every instance; they are
    /// validated by `instances::parse_instance_spec` and never enter the
    /// per-host recipe or its fingerprint.
    #[serde(default, rename = "instances")]
    _instances: Option<serde_json::Value>,
    #[serde(default, rename = "placement")]
    _placement: Option<serde_json::Value>,
    #[serde(default, rename = "host")]
    _host: Option<serde_json::Value>,
    /// SPEC §6.5 (ADR 0013 amendment 2026-09-23): `lifecycle.warm` is a
    /// deployment-level policy parsed with the instance spec; it never enters
    /// the per-host recipe or its fingerprint.
    #[serde(default, rename = "lifecycle")]
    _lifecycle: Option<serde_json::Value>,
}
/// A deployment's `model` block as written. SPEC §7 accepts two spellings: the
/// original `path`, and `source`, which can also name a remote origin. Exactly one
/// of them must be present, so a file cannot state a path and a source that differ.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    source: Option<ModelSource>,
    content_fingerprint: String,
    revision: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModelStore {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecipe {
    cold: RawPhase,
    ready: RawPhase,
    parking: RawPhase,
    parked: RawPhase,
    wake: RawPhase,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPhase {
    allocations: Vec<RawAllocation>,
    devices: Vec<DeviceClaim>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAllocation {
    domain: String,
    bytes: String,
    host_kv_bytes: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostInput {
    schema_version: u32,
    kind: String,
    name: String,
    hardware_fingerprint: String,
    environment_fingerprint: String,
    #[serde(default)]
    device_inventory_digest: Option<String>,
    model_store: RawModelStore,
    /// ADR 0008: remote model sources are denied unless stated here.
    #[serde(default)]
    model_sources: Option<crate::model_source::RawModelSources>,
    resource_policy: RawHostPolicy,
    runtime_profiles: BTreeMap<String, RawProfile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHostPolicy {
    domains: BTreeMap<String, RawDomain>,
    devices: BTreeMap<String, DevicePolicy>,
    max_parked: Option<u32>,
    observation_ttl: Option<String>,
    device_sharing: Sharing,
    endpoint_port_range: PortRange,
    planner_max_states: Option<u32>,
    queue: Option<RawQueue>,
    /// ADR 0013 §2: placement labels, validated by
    /// `instances::host_labels`; they select hosts and change no resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    labels: Option<BTreeMap<String, String>>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDomain {
    managed_limit: String,
    free_reserve: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    host_kv_limit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parked_limit: Option<String>,
    memory: DomainMemory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQueue {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_pending_per_deployment: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_pending_total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_buffered_bytes_total: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_deadline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    admission_window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stream_idle_timeout: Option<String>,
}

/// Composes the `resource_policy` object of a host document's raw JSON shape from
/// normalized resource controls, by constructing the same `Raw*` structs that
/// `resolve_effective` parses rather than writing keys by hand. This is the one
/// writer of that shape: a required field added to `RawDomain`, `RawHostPolicy`, or
/// `RawQueue` breaks this struct literal at compile time instead of silently being
/// omitted at runtime by an independent hand-built JSON writer.
///
/// SPEC §6.2: a domain's memory topology is a declared hardware fact, required on
/// every domain, so it must round-trip through composition like the other required
/// fields.
pub fn compose_resource_policy(
    controls: &crate::resource_controls::ResourceControls,
    context: &crate::resource_controls::ResourceContext,
) -> serde_json::Value {
    let domains: BTreeMap<String, RawDomain> = controls
        .domains
        .iter()
        .map(|(id, d)| {
            (
                id.clone(),
                RawDomain {
                    managed_limit: format!("{}B", d.managed_limit),
                    free_reserve: format!("{}B", d.free_reserve),
                    host_kv_limit: d.host_kv_limit.map(|n| format!("{n}B")),
                    parked_limit: d.parked_limit.map(|n| format!("{n}B")),
                    memory: d.memory,
                    device: d.device.clone(),
                },
            )
        })
        .collect();
    let devices: BTreeMap<String, DevicePolicy> = context
        .device_domains
        .iter()
        .map(|(id, domain)| {
            let sharing = controls
                .device_sharing_overrides
                .get(id)
                .copied()
                .unwrap_or(controls.device_sharing);
            (
                id.clone(),
                DevicePolicy {
                    domain: domain.clone(),
                    sharing,
                    physical_gpu_uuid: None,
                },
            )
        })
        .collect();
    let raw = RawHostPolicy {
        domains,
        devices,
        max_parked: Some(controls.max_parked),
        observation_ttl: Some(format!("{}ms", controls.observation_ttl_ms)),
        device_sharing: controls.device_sharing,
        endpoint_port_range: context.endpoint_port_range.clone(),
        planner_max_states: Some(controls.planner_max_states),
        queue: Some(RawQueue {
            max_pending_per_deployment: Some(controls.queue.max_pending_per_deployment),
            max_pending_total: Some(controls.queue.max_pending_total),
            max_buffered_bytes_total: Some(format!("{}B", controls.queue.max_buffered_bytes_total)),
            request_deadline: Some(format!("{}ms", controls.queue.request_deadline_ms)),
            admission_window: Some(format!("{}ms", controls.queue.admission_window_ms)),
            stream_idle_timeout: (controls.queue.stream_idle_ms != DEFAULT_STREAM_IDLE_MS)
                .then(|| format!("{}ms", controls.queue.stream_idle_ms)),
        }),
        labels: None,
    };
    serde_json::to_value(&raw).expect("Raw* composition types always encode to JSON")
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    engine: Engine,
    revision: u64,
    executable: String,
    build_fingerprint: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    #[serde(default)]
    cuda_home: Option<String>,
    security: Security,
    log_policy: RawLogPolicy,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLogPolicy {
    max_file_bytes: String,
    retained_files: u32,
}

const TENSORFOLD_NEEDS_RESOURCES: &str =
    "a TensorFold deployment states resources: TensorFold sizes \
    itself from free memory, and its Ready allocation is the cap it is launched with";

fn decode<T: for<'de> Deserialize<'de>>(
    value: &serde_json::Value,
    path: &str,
) -> Result<T, ConfigError> {
    let encoded =
        serde_json::to_vec(value).map_err(|_| invalid(path, "typed input could not be encoded"))?;
    let mut deserializer = serde_json::Deserializer::from_slice(&encoded);
    serde_path_to_error::deserialize(&mut deserializer).map_err(|e| {
        let detail = e.inner().to_string();
        let code = if detail.starts_with("missing field") {
            ConfigErrorCode::MissingRequired
        } else if detail.starts_with("unknown field") {
            ConfigErrorCode::UnknownField
        } else {
            ConfigErrorCode::UnsupportedCombination
        };
        let safe_detail = match code {
            ConfigErrorCode::MissingRequired => "required typed field is missing",
            ConfigErrorCode::UnknownField => "unknown typed field",
            _ => "wrong type or invalid typed value",
        };
        let mut nested = e.path().to_string();
        if matches!(
            code,
            ConfigErrorCode::MissingRequired | ConfigErrorCode::UnknownField
        ) {
            if let Some(field) = detail.split('`').nth(1).filter(|field| {
                !field.is_empty()
                    && field
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            }) {
                if !nested.ends_with(field) {
                    if !nested.is_empty() {
                        nested.push('.');
                    }
                    nested.push_str(field);
                }
            }
        }
        let error_path = if nested.is_empty() {
            path.to_owned()
        } else {
            format!("{path}.{nested}")
        };
        ConfigError::new(code, error_path, safe_detail)
    })
}

fn phase(raw: RawPhase) -> Result<PhaseFootprint, ConfigError> {
    Ok(PhaseFootprint {
        allocations: raw
            .allocations
            .into_iter()
            .map(|a| {
                Ok(Allocation {
                    domain: a.domain,
                    bytes: parse_bytes(&a.bytes)?,
                    host_kv_bytes: parse_bytes(&a.host_kv_bytes)?,
                })
            })
            .collect::<Result<_, ConfigError>>()?,
        devices: raw.devices,
    })
}

fn raw_recipe(raw: RawRecipe) -> Result<RecipeFootprints, ConfigError> {
    Ok(RecipeFootprints {
        cold: phase(raw.cold)?,
        ready: phase(raw.ready)?,
        parking: phase(raw.parking)?,
        parked: phase(raw.parked)?,
        wake: phase(raw.wake)?,
    })
}

/// Decode a host document, refusing moved fields with a pointer first.
fn decode_host(host: &serde_json::Value) -> Result<HostInput, ConfigError> {
    engine_config::refuse_moved_profile_fields(host)?;
    decode(host, "host")
}

pub fn resolve_effective(
    deployment: &serde_json::Value,
    host: &serde_json::Value,
) -> Result<EffectiveDeployment, ConfigError> {
    resolve_effective_with_checkpoint(deployment, host, CheckpointFacts::default())
}

/// ADR 0014 §5, §7: resolve with what the checkpoint manifest says, so a memory
/// request the deployment omitted can be derived from the weights' size. Both
/// sides of a launch must resolve with the same facts to agree on the result.
pub fn resolve_effective_with_checkpoint(
    deployment: &serde_json::Value,
    host: &serde_json::Value,
    facts: CheckpointFacts,
) -> Result<EffectiveDeployment, ConfigError> {
    // Owner decision 2026-09-25 (ADR 0014 amendment): complete a minimal
    // document, first from itself, then from this host. A full document is
    // unchanged by both.
    let completed = {
        let mut document = deployment.clone();
        crate::deployment_defaults::expand(&mut document)?;
        crate::deployment_defaults::for_host(&document, host)?
    };
    let deployment = &completed;
    let mut d: DeploymentInput = decode(deployment, "deployment")?;
    let h: HostInput = decode_host(host)?;
    // ADR 0013 §2–3: refuse an unplaceable or contradictory instance
    // declaration before resolving anything against this host.
    crate::instances::parse_instance_spec(deployment)?;
    if d.schema_version != 1 || d.kind != "deployment" {
        return Err(invalid(
            "schema_version",
            "schema version 1 and deployment kind required",
        ));
    }
    if d.name.is_empty() {
        return Err(invalid("deployment.name", "must not be empty"));
    }
    if d.routes.is_empty()
        || d.routes.iter().any(String::is_empty)
        || d.routes.iter().collect::<BTreeSet<_>>().len() != d.routes.len()
    {
        return Err(invalid("routes", "must be nonempty and unique"));
    }
    let raw_profile = h
        .runtime_profiles
        .get(&d.runtime_profile)
        .ok_or_else(|| invalid("runtime_profile", "unknown runtime profile"))?
        .clone();
    // ADR 0023 §4: TensorFold has no memory cap, so its reservation is the
    // operator's explicit statement.
    if raw_profile.engine == Engine::Tensorfold && d.resources.is_none() {
        return Err(ConfigError::new(
            ConfigErrorCode::MissingRequired,
            "resources",
            TENSORFOLD_NEEDS_RESOURCES,
        ));
    }
    let host = core::normalize_host(h)?;
    let devices = d.devices.take().unwrap_or_default();
    // Owner decision 2026-09-25: an undeclared residency follows the host
    // type and the checkpoint (`deployment_defaults::default_residency`); it
    // is named in the provenance so a re-resolution with the measured weights
    // chooses again (ADR 0014 §7).
    let residency_defaulted = d.residency.is_none();
    let device_sizing = core::derived_device_sizing(&devices, &host);
    let residency = d.residency.unwrap_or_else(|| {
        let discrete =
            device_sizing.map(|_| (facts.weights_bytes, core::system_parked_limit(&host)));
        // ADR 0023 §6: TensorFold never parks, so its default is restart_only.
        crate::deployment_defaults::default_residency(
            raw_profile.security.deep_park.is_enabled() && raw_profile.engine != Engine::Tensorfold,
            discrete,
        )
    });
    // Owner decision 2026-09-25: a deployment that states no memory (and no
    // resources) gets the default KV cache of the domain it runs in; its
    // request derives from the checkpoint's weights (ADR 0014 §5).
    let mut kv_defaulted = false;
    if d.resources.is_none() && !d.engine_config.states_memory() {
        let managed = match device_sizing {
            Some(sizing) => Some(sizing.managed_limit),
            None => core::single_domain(&devices, &host).map(|domain| domain.managed_limit),
        };
        if let Some(managed) = managed {
            d.engine_config
                .default_kv_cache(crate::deployment_defaults::default_kv_cache(managed));
            kv_defaulted = true;
        }
    }
    let profile = core::normalize_profile(
        &raw_profile,
        d.runtime_profile_revision.unwrap_or(raw_profile.revision),
        residency,
    )?;
    let model = core::normalize_model(
        d.model,
        Some(&host.model_store),
        Some(host.model_sources.root(&host.model_store)),
    )?;
    // ADR 0008: a remote source resolves only on a host that opted in to it.
    host.model_sources.permits(&model.source)?;
    core::check_single_device(&devices, &host)?;
    let declared_resources = d.resources.map(raw_recipe).transpose()?;
    if let Some(resources) = &declared_resources {
        core::check_system_allocation(resources, &host)?;
    }
    // ADR 0014 §5: an explicit Ready phase states the memory request. On a
    // discrete host (review decision, discrete GPU design §6) that is the
    // device allocation alone: the request sizes the engine on the card (vLLM's
    // utilization, SGLang's static fraction), and the system allocation beside
    // it is the engine process's host RAM, which the card does not hold.
    let declared_ready_total = declared_resources.as_ref().map(|resources| {
        let on_device = |domain: &str| {
            host.domains
                .get(domain)
                .is_some_and(|policy| policy.memory == DomainMemory::Device)
        };
        let ready = &resources.ready.allocations;
        let discrete = ready.iter().any(|a| on_device(&a.domain));
        ready
            .iter()
            .filter(|a| !discrete || on_device(&a.domain))
            .fold(0_i64, |total, a| total.saturating_add(a.bytes))
    });
    let mut engine_config = engine_config::normalize_engine_config(
        d.engine_config,
        engine_config::EngineInputs {
            engine: profile.engine,
            residency,
            security: &profile.security,
            profile_args: &profile.args,
            checkpoint_root: model.resolved_path.as_deref().map(Path::new),
            declared_ready_total,
            facts,
            device: match &declared_resources {
                Some(_) => None,
                None => device_sizing,
            },
        },
    )?;
    {
        // T14: what capyctl chose is named as its default, so a snapshot
        // re-resolution chooses it again rather than restating it.
        let provenance = engine_config.provenance_mut();
        if residency_defaulted {
            provenance.insert(
                "residency".into(),
                capyctl_domain::launch::SettingSource::CapyctlDefault,
            );
        }
        if kv_defaulted {
            provenance.insert(
                "memory.kv_cache".into(),
                capyctl_domain::launch::SettingSource::CapyctlDefault,
            );
        }
    }
    let resources = match declared_resources {
        Some(resources) => resources,
        None => {
            // Re-review parity rule: the engine's CUDA context and graphs are
            // charged on every host shape; a revision frozen before the charge
            // re-derives without it.
            let overhead = if facts.legacy_overhead {
                0
            } else {
                ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES
            };
            let derived = engine_config::derive_resources(
                engine_config.memory().request_bytes,
                engine_config.memory().startup_bytes,
                residency,
                &devices,
                &host,
                facts.weights_bytes,
                overhead,
            )?;
            if !facts.legacy_overhead {
                engine_config.memory_mut().overhead_bytes = Some(overhead);
            }
            engine_config.provenance_mut().insert(
                "resources".into(),
                capyctl_domain::launch::SettingSource::Derived,
            );
            derived
        }
    };
    // ADR 0023 §4: the first build's bound must not be lowered by
    // the request deadline, so a TensorFold deployment's deadline, undeclared,
    // is at least that bound, and may be declared up to it.
    let deadline_ceiling_ms = if raw_profile.engine == Engine::Tensorfold {
        host.queue
            .request_deadline_ms
            .max(timeouts::TENSORFOLD_FIRST_BUILD_MS)
    } else {
        host.queue.request_deadline_ms
    };
    let recipe = core::NormalizedRecipe {
        model,
        recipe: d.recipe,
        residency,
        recovery: d.recovery,
        devices,
        resources,
        request_deadline_ms: d
            .request_deadline
            .as_deref()
            .map(parse_duration_ms)
            .transpose()?
            .unwrap_or(deadline_ceiling_ms),
    };
    core::validate_recipe(&recipe, &host, deadline_ceiling_ms)?;
    let timeouts = timeouts::resolve_timeouts(
        d.timeouts.as_ref(),
        recipe.request_deadline_ms,
        facts,
        raw_profile.engine,
    )?;
    let recipe_fingerprint = core::recipe_fingerprint(&recipe, &profile, &engine_config, &host)?;
    Ok(EffectiveDeployment {
        schema_version: 1,
        name: d.name,
        model: recipe.model,
        routes: d.routes,
        recipe: recipe.recipe,
        residency: recipe.residency,
        recovery: recipe.recovery,
        selected_devices: recipe.devices,
        resources: recipe.resources,
        request_deadline_ms: recipe.request_deadline_ms,
        timeouts,
        engine_config,
        profile: RuntimeProfile {
            engine: profile.engine,
            revision: profile.revision,
            executable: profile.executable,
            build_fingerprint: profile.build_fingerprint,
            args: profile.args,
            env: profile.env,
            cuda_home: profile.cuda_home,
            security: profile.security,
            log_policy: profile.log_policy,
        },
        host,
        recipe_fingerprint,
    })
}

/// ADR 0018: one runtime profile checked with the rules deployment resolution
/// applies (`core::normalize_profile`), before `capyctl engine add` writes it. A
/// parking residency is checked too when the profile allows deep parking, so a
/// sleep-mode-reserved argument is refused now rather than at the first
/// deployment (T14, T21).
pub fn check_runtime_profile(profile: &serde_json::Value) -> Result<(), ConfigError> {
    let raw: RawProfile = decode(profile, "runtime_profiles")?;
    core::normalize_profile(&raw, raw.revision, Residency::RestartOnly)?;
    if raw.security.deep_park.is_enabled() {
        core::normalize_profile(&raw, raw.revision, Residency::Deep)?;
    }
    Ok(())
}

/// SPEC §7: normalize a declared host policy without selecting or launching a model.
pub fn normalize_host_policy(host: &serde_json::Value) -> Result<HostPolicy, ConfigError> {
    core::normalize_host(decode_host(host)?)
}

pub fn binding_fingerprint(
    endpoint: &str,
    served_name: &str,
    credential_ref: &str,
    incarnation: &str,
) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(&(endpoint, served_name, credential_ref, incarnation))
            .expect("tuple serialization cannot fail"),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManagedCeiling {
    pub observed_capacity_bytes: i64,
    pub protected_headroom_bytes: i64,
    pub managed_limit_bytes: i64,
    pub provenance: String,
}

pub fn derive_default_managed_ceiling(
    observed_capacity_bytes: Option<i64>,
) -> Result<Option<ManagedCeiling>, ConfigError> {
    let Some(capacity) = observed_capacity_bytes else {
        return Ok(None);
    };
    if capacity <= 0 {
        return Err(invalid("observed_capacity_bytes", "must be positive"));
    }
    let fifth = capacity
        .checked_add(4)
        .ok_or_else(|| invalid("observed_capacity_bytes", "overflow"))?
        / 5;
    let headroom = fifth.max(16_i64 << 30);
    let managed = capacity.checked_sub(headroom).ok_or_else(|| {
        invalid(
            "observed_capacity_bytes",
            "capacity below protected headroom",
        )
    })?;
    if managed <= 0 {
        return Err(invalid(
            "observed_capacity_bytes",
            "capacity below protected headroom",
        ));
    }
    Ok(Some(ManagedCeiling {
        observed_capacity_bytes: capacity,
        protected_headroom_bytes: headroom,
        managed_limit_bytes: managed,
        provenance: "observed capacity minus max(16GiB, ceil(capacity/5))".into(),
    }))
}

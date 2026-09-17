//! Pure resolution of strict manifests into immutable, serializable launch inputs.

pub mod sglang;
mod core;
mod current_policy;
mod snapshot;
pub use snapshot::decode_effective_snapshot;
pub use current_policy::{compose_current_resource_controls, deployment_command_fingerprint};

use crate::engine_policy::{validate_profile_args, validate_profile_env};
use crate::resource_controls::{ResourceContext, ResourceControls};
use crate::{ConfigError, ConfigErrorCode};
use mllm_domain::launch::{
    FakeLaunchSettings, ProfileLaunchSettings, SglangLaunchSettings, SglangRequestedBudget,
    VllmLaunchSettings, VllmRequestedBudget,
};
use mllm_domain::resources as domain;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn invalid(path: impl Into<String>, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

const DEFAULT_PENDING_PER_DEPLOYMENT: u32 = 64;
const DEFAULT_PENDING_TOTAL: u32 = 256;
const DEFAULT_QUEUED_BYTES: i64 = 64 << 20;
const DEFAULT_REQUEST_DEADLINE_MS: i64 = 600_000;
const DEFAULT_ADMISSION_WINDOW_MS: i64 = 2_000;
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
    pub profile: RuntimeProfile,
    pub host: HostPolicy,
    pub qualification_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelIdentity {
    pub path: String,
    pub content_fingerprint: String,
    pub revision: String,
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
    pub qualification_id: String,
    pub args: Vec<String>,
    pub launch_settings: ProfileLaunchSettings,
    pub env: BTreeMap<String, String>,
    pub security: Security,
    pub log_policy: LogPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    pub experimental_controls: bool,
    pub credential_ref: Option<String>,
    pub admin_credential_ref: Option<String>,
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
    pub domains: BTreeMap<String, DomainPolicy>,
    pub devices: BTreeMap<String, DevicePolicy>,
    pub max_parked: u32,
    pub observation_ttl_ms: i64,
    pub device_sharing: Sharing,
    pub endpoint_port_range: PortRange,
    pub planner_max_states: u32,
    pub queue: QueuePolicy,
    pub qualification_policy: Option<QualificationPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QualificationPolicy {
    pub revision: i64,
    pub allow_qualification_runs: bool,
    pub allow_experimental_controls: bool,
    pub allowed_manifest_digests: Vec<String>,
    pub max_run_duration_ms: i64,
    pub max_cleanup_duration_ms: i64,
    pub max_cases: u32,
    pub max_requests: u32,
    pub max_request_body_bytes: i64,
    pub max_input_tokens_per_request: u32,
    pub max_output_tokens_per_request: u32,
}

impl QualificationPolicy {
    /// Revalidates normalized policy data before a persistence boundary.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let encoding_len = serde_json::to_vec(self)
            .map_err(|_| invalid("qualification_policy", "policy could not be encoded"))?
            .len();
        if encoding_len > 1 << 20
            || self.revision <= 0
            || self.allowed_manifest_digests.len() > 1_024
            || self.max_cases == 0
            || self.max_cases > 128
            || self.max_requests == 0
            || self.max_requests > 4_096
            || self.max_input_tokens_per_request == 0
            || self.max_input_tokens_per_request > 131_072
            || self.max_output_tokens_per_request == 0
            || self.max_output_tokens_per_request > 16_384
            || !(1..=86_400_000).contains(&self.max_run_duration_ms)
            || !(1..=3_600_000).contains(&self.max_cleanup_duration_ms)
            || !(1..=(1 << 20)).contains(&self.max_request_body_bytes)
        {
            return Err(invalid(
                "qualification_policy",
                "qualification policy exceeds bounded positive limits",
            ));
        }
        if self.allowed_manifest_digests.iter().any(|digest| {
            digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            return Err(invalid(
                "qualification_policy.allowed_manifest_digests",
                "manifest digest must be canonical lowercase SHA-256 hex",
            ));
        }
        if self
            .allowed_manifest_digests
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(invalid(
                "qualification_policy.allowed_manifest_digests",
                "manifest digests must be sorted and distinct",
            ));
        }
        Ok(())
    }
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DomainPolicy {
    pub managed_limit: i64,
    pub free_reserve: i64,
    pub host_kv_limit: Option<i64>,
    pub parked_limit: Option<i64>,
    pub memory: DomainMemory,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicePolicy {
    pub domain: String,
    pub sharing: Sharing,
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentInput {
    schema_version: u32,
    kind: String,
    name: String,
    model: ModelIdentity,
    routes: Vec<String>,
    runtime_profile: String,
    runtime_profile_revision: u64,
    recipe: String,
    residency: Residency,
    recovery: Recovery,
    devices: Vec<DeviceClaim>,
    resources: RawRecipe,
    request_deadline: Option<String>,
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
    resource_policy: RawHostPolicy,
    runtime_profiles: BTreeMap<String, RawProfile>,
    qualification_policy: Option<RawQualificationPolicy>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQualificationPolicy {
    revision: i64,
    allow_qualification_runs: bool,
    allow_experimental_controls: bool,
    allowed_manifest_digests: Vec<String>,
    max_run_duration: String,
    max_cleanup_duration: String,
    max_cases: u32,
    max_requests: u32,
    max_request_body_bytes: String,
    max_input_tokens_per_request: u32,
    max_output_tokens_per_request: u32,
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
        }),
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
    #[serde(default)]
    qualification_id: MissingAwareQualification,
    args: Vec<String>,
    launch_settings: RawLaunchSettings,
    env: BTreeMap<String, String>,
    security: Security,
    log_policy: RawLogPolicy,
}
#[derive(Clone, Default)]
enum MissingAwareQualification {
    #[default]
    Missing,
    Present(Option<String>),
}
impl<'de> Deserialize<'de> for MissingAwareQualification {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<String>::deserialize(deserializer).map(Self::Present)
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLogPolicy {
    max_file_bytes: String,
    retained_files: u32,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "engine", rename_all = "lowercase", deny_unknown_fields)]
enum RawLaunchSettings {
    Vllm {
        tensor_parallel_size: u32,
        pipeline_parallel_size: u32,
        enable_sleep_mode: bool,
        kv_cache_dtype: String,
        block_size_tokens: u32,
        cpu_offload_bytes: String,
        requested_budget: RawVllmBudget,
    },
    Sglang {
        recipe: String,
        requested_budget: RawSglangBudget,
    },
    Fake,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVllmBudget {
    kv_cache_bytes: String,
    swap_space_bytes: String,
    gpu_utilization_pct: u8,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSglangBudget {
    kv_cache_bytes: String,
    static_memory_fraction_bps: u16,
}

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

fn normalize_qualification_policy(
    raw: Option<RawQualificationPolicy>,
) -> Result<Option<QualificationPolicy>, ConfigError> {
    let Some(mut raw) = raw else {
        return Ok(None);
    };
    raw.allowed_manifest_digests.sort();
    let max_run_duration_ms = parse_duration_ms(&raw.max_run_duration)?;
    let max_cleanup_duration_ms = parse_duration_ms(&raw.max_cleanup_duration)?;
    let max_request_body_bytes = parse_bytes(&raw.max_request_body_bytes)?;
    let policy = QualificationPolicy {
        revision: raw.revision,
        allow_qualification_runs: raw.allow_qualification_runs,
        allow_experimental_controls: raw.allow_experimental_controls,
        allowed_manifest_digests: raw.allowed_manifest_digests,
        max_run_duration_ms,
        max_cleanup_duration_ms,
        max_cases: raw.max_cases,
        max_requests: raw.max_requests,
        max_request_body_bytes,
        max_input_tokens_per_request: raw.max_input_tokens_per_request,
        max_output_tokens_per_request: raw.max_output_tokens_per_request,
    };
    policy.validate()?;
    Ok(Some(policy))
}

fn normalize_launch(
    raw: RawLaunchSettings,
    engine: Engine,
    residency: Residency,
) -> Result<ProfileLaunchSettings, ConfigError> {
    let settings = match raw {
        RawLaunchSettings::Vllm {
            tensor_parallel_size,
            pipeline_parallel_size,
            enable_sleep_mode,
            kv_cache_dtype,
            block_size_tokens,
            cpu_offload_bytes,
            requested_budget,
        } => {
            if engine != Engine::Vllm {
                return Err(invalid(
                    "runtime_profiles.launch_settings.engine",
                    "launch settings engine mismatch",
                ));
            }
            let value = VllmLaunchSettings {
                tensor_parallel_size,
                pipeline_parallel_size,
                enable_sleep_mode,
                kv_cache_dtype,
                block_size_tokens,
                cpu_offload_bytes: parse_bytes(&cpu_offload_bytes)?,
                requested_budget: VllmRequestedBudget {
                    kv_cache_bytes: parse_bytes(&requested_budget.kv_cache_bytes)?,
                    swap_space_bytes: parse_bytes(&requested_budget.swap_space_bytes)?,
                    gpu_utilization_pct: requested_budget.gpu_utilization_pct,
                },
            };
            if value.tensor_parallel_size == 0
                || value.pipeline_parallel_size == 0
                || value.block_size_tokens == 0
                || value.kv_cache_dtype.is_empty()
                || value.cpu_offload_bytes < 0
                || value.requested_budget.kv_cache_bytes <= 0
                || value.requested_budget.swap_space_bytes < 0
                || !(1..=100).contains(&value.requested_budget.gpu_utilization_pct)
            {
                return Err(invalid(
                    "runtime_profiles.launch_settings",
                    "invalid vLLM launch settings",
                ));
            }
            if residency.parks() && !value.enable_sleep_mode {
                return Err(invalid(
                    "runtime_profiles.launch_settings.enable_sleep_mode",
                    "a parking vLLM deployment requires sleep mode",
                ));
            }
            ProfileLaunchSettings::Vllm(value)
        }
        RawLaunchSettings::Sglang {
            recipe,
            requested_budget,
        } => {
            if engine != Engine::Sglang {
                return Err(invalid(
                    "runtime_profiles.launch_settings.engine",
                    "launch settings engine mismatch",
                ));
            }
            const RECIPE: &str = "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1";
            if recipe != RECIPE {
                return Err(invalid(
                    "runtime_profiles.launch_settings.recipe",
                    "unsupported SGLang recipe",
                ));
            }
            let budget = SglangRequestedBudget {
                kv_cache_bytes: parse_bytes(&requested_budget.kv_cache_bytes)?,
                static_memory_fraction_bps: requested_budget.static_memory_fraction_bps,
            };
            if budget.kv_cache_bytes <= 0
                || !(1..=10_000).contains(&budget.static_memory_fraction_bps)
            {
                return Err(invalid(
                    "runtime_profiles.launch_settings.requested_budget",
                    "invalid SGLang requested budget",
                ));
            }
            // SGLang takes its park strategy at launch: --enable-memory-saver and
            // --enable-weights-cpu-backup cannot be added to a running engine. The
            // declared tier therefore has to reach the launch settings, unlike
            // vLLM's level, which is a parameter of the sleep call.
            let memory_saver = residency.parks();
            let cpu_weight_backup = residency == Residency::HostBacked;
            let weight_restore = if cpu_weight_backup {
                // --enable-weights-cpu-backup, available since SGLang v0.5: weights
                // are copied to pinned host memory on sleep and restored from there.
                "cpu_backup"
            } else {
                "disk_reload"
            };
            ProfileLaunchSettings::Sglang(SglangLaunchSettings {
                recipe,
                tensor_parallel_size: 1,
                data_parallel_size: 1,
                tokenizer_workers: 1,
                model_dtype: "bfloat16".into(),
                context_tokens: 4096,
                max_running_requests: 8,
                max_total_tokens: 4096,
                prefill_cuda_graphs: false,
                decode_cuda_graphs: false,
                memory_saver,
                cpu_weight_backup,
                speculative_decoding: false,
                lora: false,
                trust_remote_code: false,
                disaggregation: false,
                external_cache: false,
                cpu_kv_offload: false,
                native_grpc: false,
                weight_restore: weight_restore.into(),
                requested_budget: budget,
            })
        }
        RawLaunchSettings::Fake => {
            if engine != Engine::Fake {
                return Err(invalid(
                    "runtime_profiles.launch_settings.engine",
                    "launch settings engine mismatch",
                ));
            }
            ProfileLaunchSettings::Fake(FakeLaunchSettings)
        }
    };
    Ok(settings)
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

pub fn resolve_effective(
    deployment: &serde_json::Value,
    host: &serde_json::Value,
) -> Result<EffectiveDeployment, ConfigError> {
    if host
        .get("qualification_policy")
        .is_some_and(|policy| serde_json::to_vec(policy).is_ok_and(|bytes| bytes.len() > 1 << 20))
    {
        return Err(invalid(
            "host.qualification_policy",
            "qualification policy encoding exceeds 1MiB",
        ));
    }
    let d: DeploymentInput = decode(deployment, "deployment")?;
    let h: HostInput = decode(host, "host")?;
    if d.schema_version != 1 || d.kind != "deployment" {
        return Err(invalid(
            "schema_version",
            "schema version 1 and deployment kind required",
        ));
    }
    if h.runtime_profiles.values().any(|profile| {
        !matches!(
            profile.qualification_id,
            MissingAwareQualification::Present(Some(_))
        )
    }) {
        return Err(invalid(
            "runtime_profiles.qualification_id",
            "qualification reference is required for ordinary host resolution",
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
        .ok_or_else(|| invalid("runtime_profile", "unknown runtime profile"))?;
    let qualification_id = match &raw_profile.qualification_id {
        MissingAwareQualification::Present(Some(value)) if !value.is_empty() => value.clone(),
        _ => {
            return Err(invalid(
                "runtime_profiles.qualification_id",
                "selected qualification reference must be nonempty",
            ))
        }
    };
    let profile = core::normalize_profile(raw_profile, d.runtime_profile_revision, d.residency)?;
    let host = core::normalize_host(h)?;
    let recipe = core::NormalizedRecipe {
        model: d.model,
        recipe: d.recipe,
        residency: d.residency,
        recovery: d.recovery,
        devices: d.devices,
        resources: RecipeFootprints {
            cold: phase(d.resources.cold)?,
            ready: phase(d.resources.ready)?,
            parking: phase(d.resources.parking)?,
            parked: phase(d.resources.parked)?,
            wake: phase(d.resources.wake)?,
        },
        request_deadline_ms: d
            .request_deadline
            .as_deref()
            .map(parse_duration_ms)
            .transpose()?
            .unwrap_or(host.queue.request_deadline_ms),
    };
    core::validate_recipe(&recipe, &host)?;
    let qualification_fingerprint = core::qualification_fingerprint(&recipe, &profile, &host)?;
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
        profile: RuntimeProfile {
            engine: profile.engine,
            revision: profile.revision,
            executable: profile.executable,
            build_fingerprint: profile.build_fingerprint,
            qualification_id,
            args: profile.args,
            launch_settings: profile.launch_settings,
            env: profile.env,
            security: profile.security,
            log_policy: profile.log_policy,
        },
        host,
        qualification_fingerprint,
    })
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

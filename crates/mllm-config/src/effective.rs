//! Pure resolution of strict manifests into immutable, serializable launch inputs.

use crate::engine_policy::{validate_profile_args, validate_profile_env};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Residency {
    Warm,
    RestartOnly,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DomainPolicy {
    pub managed_limit: i64,
    pub free_reserve: i64,
    pub host_kv_limit: Option<i64>,
    pub parked_limit: Option<i64>,
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
#[derive(Deserialize)]
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDomain {
    managed_limit: String,
    free_reserve: String,
    host_kv_limit: Option<String>,
    parked_limit: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQueue {
    max_pending_per_deployment: Option<u32>,
    max_pending_total: Option<u32>,
    max_buffered_bytes_total: Option<String>,
    request_deadline: Option<String>,
    admission_window: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    engine: Engine,
    revision: u64,
    executable: String,
    build_fingerprint: String,
    qualification_id: String,
    args: Vec<String>,
    launch_settings: RawLaunchSettings,
    env: BTreeMap<String, String>,
    security: Security,
    log_policy: RawLogPolicy,
}
#[derive(Deserialize)]
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
            if residency == Residency::Warm && !value.enable_sleep_mode {
                return Err(invalid(
                    "runtime_profiles.launch_settings.enable_sleep_mode",
                    "warm vLLM requires sleep mode",
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
                memory_saver: true,
                cpu_weight_backup: false,
                speculative_decoding: false,
                lora: false,
                trust_remote_code: false,
                disaggregation: false,
                external_cache: false,
                cpu_kv_offload: false,
                native_grpc: false,
                weight_restore: "disk_reload".into(),
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

fn phase(
    raw: RawPhase,
    expected: domain::ResourcePhase,
) -> Result<(PhaseFootprint, domain::PhaseFootprint), ConfigError> {
    let allocations: Vec<Allocation> = raw
        .allocations
        .into_iter()
        .map(|a| {
            Ok(Allocation {
                domain: a.domain,
                bytes: parse_bytes(&a.bytes)?,
                host_kv_bytes: parse_bytes(&a.host_kv_bytes)?,
            })
        })
        .collect::<Result<_, ConfigError>>()?;
    let public = PhaseFootprint {
        allocations,
        devices: raw.devices,
    };
    let internal = domain::PhaseFootprint {
        phase: expected,
        allocations: public
            .allocations
            .iter()
            .map(|a| domain::Allocation {
                domain: a.domain.clone(),
                bytes: a.bytes,
                host_kv_bytes: a.host_kv_bytes,
            })
            .collect(),
        devices: public
            .devices
            .iter()
            .map(|d| domain::DeviceClaim {
                device: d.id.clone(),
                sharing: match d.sharing {
                    Sharing::Shared => domain::Sharing::Shared,
                    Sharing::Exclusive => domain::Sharing::Exclusive,
                },
            })
            .collect(),
    };
    Ok((public, internal))
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
    if d.schema_version != 1 || h.schema_version != 1 || d.kind != "deployment" || h.kind != "host"
    {
        return Err(invalid(
            "schema_version",
            "schema version 1 and matching kinds required",
        ));
    }
    for (path, value) in [
        ("deployment.name", &d.name),
        ("model.path", &d.model.path),
        ("model.content_fingerprint", &d.model.content_fingerprint),
        ("model.revision", &d.model.revision),
        ("recipe", &d.recipe),
        ("host.name", &h.name),
        ("hardware_fingerprint", &h.hardware_fingerprint),
        ("environment_fingerprint", &h.environment_fingerprint),
    ] {
        if value.is_empty() {
            return Err(invalid(path, "must not be empty"));
        }
    }
    if !Path::new(&d.model.path).is_absolute() {
        return Err(invalid("model.path", "must be absolute"));
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
    if raw_profile.revision != d.runtime_profile_revision {
        return Err(invalid(
            "runtime_profile_revision",
            "profile revision mismatch",
        ));
    }
    if !Path::new(&raw_profile.executable).is_absolute() {
        return Err(invalid("runtime_profiles.executable", "must be absolute"));
    }
    if raw_profile.build_fingerprint.is_empty()
        || raw_profile.qualification_id.is_empty()
        || matches!(raw_profile.engine, Engine::Vllm | Engine::Sglang)
            && raw_profile
                .security
                .credential_ref
                .as_ref()
                .is_none_or(String::is_empty)
    {
        return Err(invalid(
            "runtime_profiles",
            "fingerprints, qualification reference, and runtime credential reference are required",
        ));
    }
    if raw_profile.engine == Engine::Sglang
        && raw_profile
            .security
            .admin_credential_ref
            .as_ref()
            .is_none_or(String::is_empty)
    {
        return Err(invalid(
            "runtime_profiles.security.admin_credential_ref",
            "SGLang requires a distinct admin credential reference",
        ));
    }
    if raw_profile.security.admin_credential_ref.is_some()
        && raw_profile.security.admin_credential_ref.as_ref()
            == raw_profile.security.credential_ref.as_ref()
    {
        return Err(invalid(
            "runtime_profiles.security",
            "runtime and admin credential references must differ",
        ));
    }
    validate_profile_args(raw_profile.engine, &raw_profile.args)
        .map_err(|e| invalid("runtime_profiles.args", e.to_string()))?;
    validate_profile_env(&raw_profile.env).map_err(|name| {
        invalid(
            format!("runtime_profiles.env.{name}"),
            "environment name is not allowlisted",
        )
    })?;
    let (cold, cold_i) = phase(d.resources.cold, domain::ResourcePhase::Cold)?;
    let (ready, ready_i) = phase(d.resources.ready, domain::ResourcePhase::Ready)?;
    let (parking, parking_i) = phase(d.resources.parking, domain::ResourcePhase::Parking)?;
    let (parked, parked_i) = phase(d.resources.parked, domain::ResourcePhase::Parked)?;
    let (wake, wake_i) = phase(d.resources.wake, domain::ResourcePhase::Wake)?;
    domain::validate_recipe(&domain::RecipeFootprints {
        cold: cold_i,
        ready: ready_i,
        parking: parking_i,
        parked: parked_i,
        wake: wake_i,
    })
    .map_err(|e| invalid("resources", e.to_string()))?;
    let resources = RecipeFootprints {
        cold,
        ready,
        parking,
        parked,
        wake,
    };
    let mut domains = BTreeMap::new();
    for (name, raw) in h.resource_policy.domains {
        let value = DomainPolicy {
            managed_limit: parse_bytes(&raw.managed_limit)?,
            free_reserve: parse_bytes(&raw.free_reserve)?,
            host_kv_limit: raw.host_kv_limit.as_deref().map(parse_bytes).transpose()?,
            parked_limit: raw.parked_limit.as_deref().map(parse_bytes).transpose()?,
        };
        if value.managed_limit <= 0 || value.free_reserve < 0 {
            return Err(invalid("resource_policy.domains", "invalid domain limits"));
        }
        domains.insert(name, value);
    }
    if domains.is_empty() {
        return Err(invalid(
            "resource_policy.domains",
            "at least one domain required",
        ));
    }
    for (id, policy) in &h.resource_policy.devices {
        if !domains.contains_key(&policy.domain) {
            return Err(invalid(
                format!("resource_policy.devices.{id}.domain"),
                "unknown domain",
            ));
        }
        if h.resource_policy.device_sharing == Sharing::Exclusive
            && policy.sharing == Sharing::Shared
        {
            return Err(invalid(
                format!("resource_policy.devices.{id}.sharing"),
                "device policy cannot relax global exclusive policy",
            ));
        }
    }
    let selected: BTreeMap<_, _> = d
        .devices
        .iter()
        .map(|x| (x.id.as_str(), x.sharing))
        .collect();
    if selected.len() != d.devices.len() {
        return Err(invalid("devices", "device IDs must be unique"));
    }
    for claim in &d.devices {
        let policy = h
            .resource_policy
            .devices
            .get(&claim.id)
            .ok_or_else(|| invalid("devices", format!("unknown device `{}`", claim.id)))?;
        if h.resource_policy.device_sharing == Sharing::Exclusive
            && claim.sharing == Sharing::Shared
            || policy.sharing == Sharing::Exclusive && claim.sharing == Sharing::Shared
        {
            return Err(invalid("devices", "sharing claim exceeds policy"));
        }
    }
    for p in [
        &resources.cold,
        &resources.ready,
        &resources.parking,
        &resources.parked,
        &resources.wake,
    ] {
        for a in &p.allocations {
            if !domains.contains_key(&a.domain) {
                return Err(invalid(
                    "resources.allocations.domain",
                    format!("unknown domain `{}`", a.domain),
                ));
            }
        }
        for claim in &p.devices {
            if selected.get(claim.id.as_str()) != Some(&claim.sharing) {
                return Err(invalid(
                    "resources.devices",
                    "phase device claim must match selected deployment claim",
                ));
            }
        }
    }
    let raw_queue = h.resource_policy.queue.unwrap_or(RawQueue {
        max_pending_per_deployment: None,
        max_pending_total: None,
        max_buffered_bytes_total: None,
        request_deadline: None,
        admission_window: None,
    });
    let queue = QueuePolicy {
        max_pending_per_deployment: raw_queue
            .max_pending_per_deployment
            .unwrap_or(DEFAULT_PENDING_PER_DEPLOYMENT),
        max_pending_total: raw_queue.max_pending_total.unwrap_or(DEFAULT_PENDING_TOTAL),
        max_buffered_bytes_total: raw_queue
            .max_buffered_bytes_total
            .as_deref()
            .map(parse_bytes)
            .transpose()?
            .unwrap_or(DEFAULT_QUEUED_BYTES),
        request_deadline_ms: raw_queue
            .request_deadline
            .as_deref()
            .map(parse_duration_ms)
            .transpose()?
            .unwrap_or(DEFAULT_REQUEST_DEADLINE_MS),
        admission_window_ms: raw_queue
            .admission_window
            .as_deref()
            .map(parse_duration_ms)
            .transpose()?
            .unwrap_or(DEFAULT_ADMISSION_WINDOW_MS),
    };
    let max_parked = h.resource_policy.max_parked.unwrap_or(DEFAULT_MAX_PARKED);
    let observation_ttl_ms = h
        .resource_policy
        .observation_ttl
        .as_deref()
        .map(parse_duration_ms)
        .transpose()?
        .unwrap_or(DEFAULT_OBSERVATION_TTL_MS);
    let planner_max_states = h
        .resource_policy
        .planner_max_states
        .unwrap_or(DEFAULT_PLANNER_STATES);
    if queue.max_pending_per_deployment == 0
        || queue.max_pending_per_deployment > 4_096
        || queue.max_pending_total == 0
        || queue.max_pending_total > 16_384
        || queue.max_pending_per_deployment > queue.max_pending_total
        || queue.max_buffered_bytes_total <= 0
        || queue.max_buffered_bytes_total > (1_i64 << 30)
        || queue.request_deadline_ms <= 0
        || queue.request_deadline_ms > 3_600_000
        || queue.admission_window_ms <= 0
        || queue.admission_window_ms > 30_000
        || queue.admission_window_ms > queue.request_deadline_ms
        || max_parked > 16
        || observation_ttl_ms <= 0
        || observation_ttl_ms > 10_000
        || planner_max_states == 0
        || planner_max_states > 65_536
        || h.resource_policy.endpoint_port_range.start == 0
        || h.resource_policy.endpoint_port_range.start > h.resource_policy.endpoint_port_range.end
    {
        return Err(invalid("resource_policy", "invalid bounded host policy"));
    }
    let deadline = d
        .request_deadline
        .as_deref()
        .map(parse_duration_ms)
        .transpose()?
        .unwrap_or(queue.request_deadline_ms);
    if deadline <= 0 || deadline > queue.request_deadline_ms {
        return Err(invalid(
            "request_deadline",
            "deployment deadline may only shorten host limit",
        ));
    }
    let launch_settings = normalize_launch(
        raw_profile.launch_settings.clone(),
        raw_profile.engine,
        d.residency,
    )?;
    let uses_experimental_controls = match &launch_settings {
        ProfileLaunchSettings::Vllm(settings) => settings.enable_sleep_mode,
        ProfileLaunchSettings::Sglang(settings) => settings.memory_saver,
        ProfileLaunchSettings::Fake(_) => false,
    };
    if uses_experimental_controls && !raw_profile.security.experimental_controls {
        return Err(invalid(
            "runtime_profiles.security.experimental_controls",
            "launch settings require explicit experimental controls policy",
        ));
    }
    let profile = RuntimeProfile {
        engine: raw_profile.engine,
        revision: raw_profile.revision,
        executable: raw_profile.executable.clone(),
        build_fingerprint: raw_profile.build_fingerprint.clone(),
        qualification_id: raw_profile.qualification_id.clone(),
        args: raw_profile.args.clone(),
        launch_settings,
        env: raw_profile.env.clone(),
        security: raw_profile.security.clone(),
        log_policy: LogPolicy {
            max_file_bytes: parse_bytes(&raw_profile.log_policy.max_file_bytes)?,
            retained_files: raw_profile.log_policy.retained_files,
        },
    };
    let qualification_policy = normalize_qualification_policy(h.qualification_policy)?;
    let host = HostPolicy {
        name: h.name,
        hardware_fingerprint: h.hardware_fingerprint,
        environment_fingerprint: h.environment_fingerprint,
        domains,
        devices: h.resource_policy.devices,
        max_parked,
        observation_ttl_ms,
        device_sharing: h.resource_policy.device_sharing,
        endpoint_port_range: h.resource_policy.endpoint_port_range,
        planner_max_states,
        queue,
        qualification_policy,
    };
    #[derive(Serialize)]
    struct Qualification<'a> {
        model: &'a ModelIdentity,
        recipe: &'a str,
        residency: Residency,
        recovery: Recovery,
        devices: &'a [DeviceClaim],
        resources: &'a RecipeFootprints,
        host_devices: &'a BTreeMap<String, DevicePolicy>,
        device_sharing: Sharing,
        engine: Engine,
        revision: u64,
        executable: &'a str,
        build_fingerprint: &'a str,
        args: &'a [String],
        launch_settings: &'a ProfileLaunchSettings,
        env: &'a BTreeMap<String, String>,
        experimental_controls: bool,
        runtime_auth: bool,
        admin_auth: bool,
        log_policy: &'a LogPolicy,
        hardware_fingerprint: &'a str,
        environment_fingerprint: &'a str,
    }
    let material = Qualification {
        model: &d.model,
        recipe: &d.recipe,
        residency: d.residency,
        recovery: d.recovery,
        devices: &d.devices,
        resources: &resources,
        host_devices: &host.devices,
        device_sharing: host.device_sharing,
        engine: profile.engine,
        revision: profile.revision,
        executable: &profile.executable,
        build_fingerprint: &profile.build_fingerprint,
        args: &profile.args,
        launch_settings: &profile.launch_settings,
        env: &profile.env,
        experimental_controls: profile.security.experimental_controls,
        runtime_auth: profile.security.credential_ref.is_some(),
        admin_auth: profile.security.admin_credential_ref.is_some(),
        log_policy: &profile.log_policy,
        hardware_fingerprint: &host.hardware_fingerprint,
        environment_fingerprint: &host.environment_fingerprint,
    };
    let qualification_fingerprint = hex::encode(Sha256::digest(
        serde_json::to_vec(&material).map_err(|e| invalid("fingerprint", e.to_string()))?,
    ));
    Ok(EffectiveDeployment {
        schema_version: 1,
        name: d.name,
        model: d.model,
        routes: d.routes,
        recipe: d.recipe,
        residency: d.residency,
        recovery: d.recovery,
        selected_devices: d.devices,
        resources,
        request_deadline_ms: deadline,
        profile,
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

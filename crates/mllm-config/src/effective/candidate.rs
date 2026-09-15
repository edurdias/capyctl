//! Strict, informational normalization of reviewed candidate recipes.

use super::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_ENCODED: usize = 1 << 20;
const DIGEST_DOMAIN: &[u8] = b"mllm.candidate-manifest.v1\0";
pub const NATIVE_SGLANG_SOURCE_REVISION: &str = "fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1";
pub const NATIVE_CHECKPOINT_REVISION: &str = "cdbee75f17c01a7cc42f958dc650907174af0554";
pub const NATIVE_SGLANG_RECIPE: &str = "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1";

/// Public behavior only. Validation is informational and grants no launch authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeLaunchMetadata {
    rendered_settings_digest: String,
}
impl NativeLaunchMetadata {
    pub fn rendered_settings_digest(&self) -> &str {
        &self.rendered_settings_digest
    }
}

#[derive(Debug, Clone)]
pub struct NormalizedCandidateManifest {
    host: CandidateHost,
    effective_recipe: CandidateRecipe,
    limits: CandidateLimits,
    cases: Vec<CandidateCase>,
    reviewed_json: Vec<u8>,
    manifest_digest: String,
    recipe_fingerprint: String,
    credential_refs: CandidateCredentialRefs,
    total_case_request_budget: u32,
}

/// Immutable, informational view of an intrinsically valid reviewed manifest.
///
/// Fields are private and cannot be used to construct deployment authority.
///
/// ```compile_fail
/// # use mllm_config::effective::candidate::validate_candidate_reviewed_snapshot;
/// # let value = serde_json::json!({});
/// let snapshot = validate_candidate_reviewed_snapshot(&value).unwrap();
/// let _ = snapshot.host;
/// ```
///
/// ```compile_fail
/// # use mllm_config::effective::candidate::{CandidateReviewedSnapshot, NormalizedCandidateManifest};
/// fn authorize(snapshot: CandidateReviewedSnapshot) -> NormalizedCandidateManifest {
///     snapshot.into()
/// }
/// ```
#[derive(Debug, Clone)]
pub struct CandidateReviewedSnapshot {
    host: CandidateHost,
    effective_recipe: CandidateRecipe,
    limits: CandidateLimits,
    cases: Vec<CandidateCase>,
    reviewed_json: Vec<u8>,
    manifest_digest: String,
    total_case_request_budget: u32,
}

impl CandidateReviewedSnapshot {
    /// Checks the closed native recipe without filesystem or engine effects.
    /// Credential references are checked for structure only and never hashed.
    pub fn native_launch_metadata(
        &self,
        inference_reference: Option<&str>,
        admin_reference: Option<&str>,
    ) -> Result<NativeLaunchMetadata, ConfigError> {
        let reject = || {
            invalid(
                "candidate.native_launch",
                "unsupported native launch descriptor",
            )
        };
        let p = &self.effective_recipe.resolved_profile;
        let m = &self.effective_recipe.model;
        let valid_reference = |value: &str| {
            !value.is_empty()
                && value.len() <= 4096
                && !value.chars().any(char::is_whitespace)
                && !value.chars().any(char::is_control)
        };
        let (Some(inference), Some(admin)) = (inference_reference, admin_reference) else {
            return Err(reject());
        };
        if !valid_reference(inference)
            || !valid_reference(admin)
            || inference == admin
            || !p.runtime_auth
            || !p.admin_auth
            || !p.experimental_controls
            || p.engine != Engine::Sglang
            || p.build_fingerprint != NATIVE_SGLANG_SOURCE_REVISION
            || m.revision != NATIVE_CHECKPOINT_REVISION
            || !p.args.is_empty()
            || !Path::new(&m.path).is_absolute()
            || m.path.chars().any(char::is_control)
            || m.path.split('/').any(|part| matches!(part, "." | ".."))
            || m.path == "/"
            || p.executable.chars().any(char::is_control)
            || p.env
                .iter()
                .any(|(key, value)| key != "RUST_LOG" || value != "info")
            || self.effective_recipe.devices.len() != 1
        {
            return Err(reject());
        }
        let CandidateLaunch::Sglang {
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
        } = &p.launch_settings
        else {
            return Err(reject());
        };
        if recipe != NATIVE_SGLANG_RECIPE
            || model_dtype != "bfloat16"
            || weight_restore != "disk_reload"
            || requested_budget.kv_cache_bytes <= 0
            || !(1..=10000).contains(&requested_budget.static_memory_fraction_bps)
        {
            return Err(reject());
        }
        // Paths and references are retained separately; this digest describes only
        // the public behavior that the native renderer must preserve.
        let public = serde_json::to_vec(&(
            1_u8,
            NATIVE_SGLANG_SOURCE_REVISION,
            NATIVE_CHECKPOINT_REVISION,
            &p.launch_settings,
            &self.effective_recipe.devices,
        ))
        .map_err(|_| reject())?;
        let mut digest = Sha256::new();
        digest.update(b"mllm.native-candidate-launch.v1\0");
        digest.update(public);
        Ok(NativeLaunchMetadata {
            rendered_settings_digest: format!("{:x}", digest.finalize()),
        })
    }

    pub fn host_id(&self) -> &str {
        &self.host.id
    }
    pub fn host(&self) -> &CandidateHost {
        &self.host
    }
    pub fn effective_recipe(&self) -> &CandidateRecipe {
        &self.effective_recipe
    }
    pub fn limits(&self) -> &CandidateLimits {
        &self.limits
    }
    pub fn cases(&self) -> &[CandidateCase] {
        &self.cases
    }
    pub fn reviewed_json(&self) -> &[u8] {
        &self.reviewed_json
    }
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }
    pub fn total_case_request_budget(&self) -> u32 {
        self.total_case_request_budget
    }
}

impl NormalizedCandidateManifest {
    pub fn host_id(&self) -> &str {
        &self.host.id
    }
    pub fn host(&self) -> &CandidateHost {
        &self.host
    }
    pub fn effective_recipe(&self) -> &CandidateRecipe {
        &self.effective_recipe
    }
    pub fn limits(&self) -> &CandidateLimits {
        &self.limits
    }
    pub fn cases(&self) -> &[CandidateCase] {
        &self.cases
    }
    pub fn reviewed_json(&self) -> &[u8] {
        &self.reviewed_json
    }
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }
    pub fn recipe_fingerprint(&self) -> &str {
        &self.recipe_fingerprint
    }
    pub fn credential_refs(&self) -> &CandidateCredentialRefs {
        &self.credential_refs
    }
    pub fn total_case_request_budget(&self) -> u32 {
        self.total_case_request_budget
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateHost {
    id: String,
    hardware_fingerprint: String,
    environment_fingerprint: String,
}
impl CandidateHost {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn hardware_fingerprint(&self) -> &str {
        &self.hardware_fingerprint
    }
    pub fn environment_fingerprint(&self) -> &str {
        &self.environment_fingerprint
    }
}

#[derive(Debug, Clone)]
pub struct CandidateCredentialRefs {
    runtime: Option<String>,
    admin: Option<String>,
}
impl CandidateCredentialRefs {
    pub fn runtime(&self) -> Option<&str> {
        self.runtime.as_deref()
    }
    pub fn admin(&self) -> Option<&str> {
        self.admin.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateLimits {
    max_run_duration_ms: i64,
    max_cleanup_duration_ms: i64,
    max_requests: u32,
    max_request_body_bytes: i64,
    max_input_tokens_per_request: u32,
    max_output_tokens_per_request: u32,
}
impl CandidateLimits {
    pub fn max_run_duration_ms(&self) -> i64 {
        self.max_run_duration_ms
    }
    pub fn max_cleanup_duration_ms(&self) -> i64 {
        self.max_cleanup_duration_ms
    }
    pub fn max_requests(&self) -> u32 {
        self.max_requests
    }
    pub fn max_request_body_bytes(&self) -> i64 {
        self.max_request_body_bytes
    }
    pub fn max_input_tokens_per_request(&self) -> u32 {
        self.max_input_tokens_per_request
    }
    pub fn max_output_tokens_per_request(&self) -> u32 {
        self.max_output_tokens_per_request
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateCase {
    id: String,
    kind: CandidateCaseKind,
    cycle: u32,
    count: u32,
    request_budget: u32,
    #[serde(
        default,
        deserialize_with = "present_digest",
        skip_serializing_if = "Option::is_none"
    )]
    corpus_digest: Option<String>,
}
fn present_digest<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

impl CandidateCase {
    pub fn kind(&self) -> CandidateCaseKind {
        self.kind
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn cycle(&self) -> u32 {
        self.cycle
    }
    pub fn count(&self) -> u32 {
        self.count
    }
    pub fn request_budget(&self) -> u32 {
        self.request_budget
    }
    pub fn corpus_digest(&self) -> Option<&str> {
        self.corpus_digest.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateCaseKind {
    ColdInitialize,
    Park,
    Restore,
    ReadyProbe,
    MarkerNonstreaming,
    MarkerStreaming,
    Security,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateRecipe {
    model: ModelIdentity,
    recipe: String,
    residency: Residency,
    recovery: Recovery,
    runtime_profile: String,
    runtime_profile_revision: u64,
    resolved_profile: CandidateProfile,
    devices: Vec<DeviceClaim>,
    resources: CandidateResources,
    host_devices: BTreeMap<String, DevicePolicy>,
    host_device_sharing: Sharing,
    request_deadline_ms: i64,
}
impl CandidateRecipe {
    pub fn recipe(&self) -> &str {
        &self.recipe
    }
    pub fn residency(&self) -> Residency {
        self.residency
    }
    pub fn recovery(&self) -> Recovery {
        self.recovery
    }
    pub fn host_devices(&self) -> &BTreeMap<String, DevicePolicy> {
        &self.host_devices
    }
    pub fn host_device_sharing(&self) -> Sharing {
        self.host_device_sharing
    }

    pub fn model(&self) -> &ModelIdentity {
        &self.model
    }
    pub fn profile(&self) -> &CandidateProfile {
        &self.resolved_profile
    }
    pub fn resources(&self) -> &CandidateResources {
        &self.resources
    }
    pub fn devices(&self) -> &[DeviceClaim] {
        &self.devices
    }
    pub fn runtime_profile(&self) -> &str {
        &self.runtime_profile
    }
    pub fn runtime_profile_revision(&self) -> u64 {
        self.runtime_profile_revision
    }
    pub fn request_deadline_ms(&self) -> i64 {
        self.request_deadline_ms
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateProfile {
    engine: Engine,
    revision: u64,
    executable: String,
    build_fingerprint: String,
    args: Vec<String>,
    launch_settings: CandidateLaunch,
    env: BTreeMap<String, String>,
    experimental_controls: bool,
    runtime_auth: bool,
    admin_auth: bool,
    log_policy: CandidateLogPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateLogPolicy {
    max_file_bytes: i64,
    retained_files: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "engine", rename_all = "lowercase", deny_unknown_fields)]
pub enum CandidateLaunch {
    Vllm {
        tensor_parallel_size: u32,
        pipeline_parallel_size: u32,
        enable_sleep_mode: bool,
        kv_cache_dtype: String,
        block_size_tokens: u32,
        cpu_offload_bytes: i64,
        requested_budget: CandidateVllmBudget,
    },
    Sglang {
        recipe: String,
        tensor_parallel_size: u32,
        data_parallel_size: u32,
        tokenizer_workers: u32,
        model_dtype: String,
        context_tokens: u32,
        max_running_requests: u32,
        max_total_tokens: u32,
        prefill_cuda_graphs: bool,
        decode_cuda_graphs: bool,
        memory_saver: bool,
        cpu_weight_backup: bool,
        speculative_decoding: bool,
        lora: bool,
        trust_remote_code: bool,
        disaggregation: bool,
        external_cache: bool,
        cpu_kv_offload: bool,
        native_grpc: bool,
        weight_restore: String,
        requested_budget: CandidateSglangBudget,
    },
    Fake,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateVllmBudget {
    kv_cache_bytes: i64,
    swap_space_bytes: i64,
    gpu_utilization_pct: u8,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateSglangBudget {
    kv_cache_bytes: i64,
    static_memory_fraction_bps: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateResources {
    cold: CandidatePhase,
    ready: CandidatePhase,
    parking: CandidatePhase,
    parked: CandidatePhase,
    wake: CandidatePhase,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidatePhase {
    allocations: Vec<CandidateAllocation>,
    devices: Vec<DeviceClaim>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateAllocation {
    domain: String,
    bytes: i64,
    host_kv_bytes: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateInput {
    schema_version: u32,
    kind: String,
    host: CandidateHost,
    effective_recipe: CandidateRecipe,
    limits: CandidateLimits,
    evaluator_suite: String,
    cases: Vec<CandidateCase>,
}

#[derive(Serialize)]
struct Reviewed<'a> {
    schema_version: u32,
    kind: &'static str,
    host: &'a CandidateHost,
    effective_recipe: &'a CandidateRecipe,
    limits: &'a CandidateLimits,
    evaluator_suite: &'static str,
    cases: &'a [CandidateCase],
}

#[derive(Serialize)]
struct DescriptorBudget<'a> {
    version: u32,
    reviewed_manifest: &'a Reviewed<'a>,
    manifest_digest: &'a str,
    recipe_fingerprint: &'a str,
    credential_refs: DescriptorCredentialRefs<'a>,
    total_case_request_budget: u32,
}

#[derive(Serialize)]
struct DescriptorCredentialRefs<'a> {
    runtime: Option<&'a str>,
    admin: Option<&'a str>,
}

/// Normalize a pre-parsed candidate. Callers must use the text API when duplicate-key
/// rejection is required because `serde_json::Value` cannot retain duplicate keys.
pub fn normalize_candidate_manifest(
    value: &Value,
    trusted_host: &Value,
) -> Result<NormalizedCandidateManifest, ConfigError> {
    let input = decode_candidate_input(value)?;
    normalize(input, trusted_host)
}

fn decode_candidate_input(value: &Value) -> Result<CandidateInput, ConfigError> {
    check_size(value, "candidate")?;
    reject_fake_launch_extras(value)?;
    decode(value, "candidate")
}

/// Validate an informational reviewed snapshot. Untrusted original input must first
/// reject duplicate keys because `serde_json::Value` cannot retain them.
pub fn validate_candidate_reviewed_snapshot(
    value: &Value,
) -> Result<CandidateReviewedSnapshot, ConfigError> {
    let input = decode_candidate_input(value)?;
    let total = validate_candidate_intrinsic(&input)?;
    let (reviewed_json, manifest_digest) = encode_reviewed(&input)?;
    Ok(CandidateReviewedSnapshot {
        host: input.host,
        effective_recipe: input.effective_recipe,
        limits: input.limits,
        cases: input.cases,
        reviewed_json,
        manifest_digest,
        total_case_request_budget: total,
    })
}

pub fn validate_candidate_reviewed_snapshot_text(
    text: &str,
) -> Result<CandidateReviewedSnapshot, ConfigError> {
    if text.len() > MAX_ENCODED {
        return Err(invalid("candidate", "input exceeds 1MiB"));
    }
    let value = crate::strict_yaml::build_value(text)?;
    validate_candidate_reviewed_snapshot(&value)
}

fn reject_fake_launch_extras(value: &Value) -> Result<(), ConfigError> {
    let Some(launch) = value
        .pointer("/effective_recipe/resolved_profile/launch_settings")
        .and_then(Value::as_object)
    else {
        return Ok(());
    };
    if launch.get("engine").and_then(Value::as_str) == Some("fake") && launch.len() != 1 {
        return Err(invalid(
            "effective_recipe.resolved_profile.launch_settings",
            "unknown fake launch setting",
        ));
    }
    Ok(())
}

pub fn normalize_candidate_manifest_text(
    text: &str,
    trusted_host: &Value,
) -> Result<NormalizedCandidateManifest, ConfigError> {
    if text.len() > MAX_ENCODED {
        return Err(invalid("candidate", "input exceeds 1MiB"));
    }
    let value = crate::strict_yaml::build_value(text)?;
    normalize_candidate_manifest(&value, trusted_host)
}

fn check_size(value: &Value, path: &str) -> Result<(), ConfigError> {
    let size = serde_json::to_vec(value)
        .map_err(|_| invalid(path, "input could not be encoded"))?
        .len();
    if size > MAX_ENCODED {
        Err(invalid(path, "encoding exceeds 1MiB"))
    } else {
        Ok(())
    }
}

impl CandidateProfile {
    pub fn engine(&self) -> Engine {
        self.engine
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn executable(&self) -> &str {
        &self.executable
    }
    pub fn build_fingerprint(&self) -> &str {
        &self.build_fingerprint
    }
    pub fn args(&self) -> &[String] {
        &self.args
    }
    pub fn launch_settings(&self) -> &CandidateLaunch {
        &self.launch_settings
    }
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }
    pub fn experimental_controls(&self) -> bool {
        self.experimental_controls
    }
    pub fn runtime_auth(&self) -> bool {
        self.runtime_auth
    }
    pub fn admin_auth(&self) -> bool {
        self.admin_auth
    }
    pub fn log_policy(&self) -> &CandidateLogPolicy {
        &self.log_policy
    }
}

impl CandidateLogPolicy {
    pub fn max_file_bytes(&self) -> i64 {
        self.max_file_bytes
    }
    pub fn retained_files(&self) -> u32 {
        self.retained_files
    }
}

impl CandidateVllmBudget {
    pub fn kv_cache_bytes(&self) -> i64 {
        self.kv_cache_bytes
    }
    pub fn swap_space_bytes(&self) -> i64 {
        self.swap_space_bytes
    }
    pub fn gpu_utilization_pct(&self) -> u8 {
        self.gpu_utilization_pct
    }
}

impl CandidateSglangBudget {
    pub fn kv_cache_bytes(&self) -> i64 {
        self.kv_cache_bytes
    }
    pub fn static_memory_fraction_bps(&self) -> u16 {
        self.static_memory_fraction_bps
    }
}

impl CandidateResources {
    pub fn cold(&self) -> &CandidatePhase {
        &self.cold
    }
    pub fn ready(&self) -> &CandidatePhase {
        &self.ready
    }
    pub fn parking(&self) -> &CandidatePhase {
        &self.parking
    }
    pub fn parked(&self) -> &CandidatePhase {
        &self.parked
    }
    pub fn wake(&self) -> &CandidatePhase {
        &self.wake
    }
}

impl CandidatePhase {
    pub fn allocations(&self) -> &[CandidateAllocation] {
        &self.allocations
    }
    pub fn devices(&self) -> &[DeviceClaim] {
        &self.devices
    }
}

impl CandidateAllocation {
    pub fn domain(&self) -> &str {
        &self.domain
    }
    pub fn bytes(&self) -> i64 {
        self.bytes
    }
    pub fn host_kv_bytes(&self) -> i64 {
        self.host_kv_bytes
    }
}

fn normalize(
    input: CandidateInput,
    trusted_host: &Value,
) -> Result<NormalizedCandidateManifest, ConfigError> {
    let total = validate_candidate_intrinsic(&input)?;
    let h: HostInput = decode(trusted_host, "host")?;
    if h.schema_version != 1 || h.kind != "host" {
        return Err(invalid("host", "trusted host kind/version mismatch"));
    }
    if input.host.id != h.name
        || input.host.hardware_fingerprint != h.hardware_fingerprint
        || input.host.environment_fingerprint != h.environment_fingerprint
    {
        return Err(invalid(
            "candidate.host",
            "host identity projection mismatch",
        ));
    }
    let recipe = &input.effective_recipe;
    let raw = h
        .runtime_profiles
        .get(&recipe.runtime_profile)
        .ok_or_else(|| {
            invalid(
                "effective_recipe.runtime_profile",
                "unknown runtime profile",
            )
        })?;
    for profile in h.runtime_profiles.values() {
        if !matches!(&profile.qualification_id, MissingAwareQualification::Present(Some(value)) if !value.is_empty())
        {
            // Missing is valid; a present reference must be a nonempty string.
            if !matches!(profile.qualification_id, MissingAwareQualification::Missing) {
                return Err(invalid(
                    "runtime_profiles.qualification_id",
                    "present qualification reference must be a nonempty string",
                ));
            }
        }
    }
    if raw.revision != recipe.runtime_profile_revision
        || recipe.resolved_profile.revision != raw.revision
    {
        return Err(invalid(
            "effective_recipe.runtime_profile_revision",
            "profile revision mismatch",
        ));
    }
    let profile = core::normalize_profile(raw, recipe.runtime_profile_revision, recipe.residency)?;
    if raw
        .security
        .credential_ref
        .as_ref()
        .is_some_and(String::is_empty)
        || raw
            .security
            .admin_credential_ref
            .as_ref()
            .is_some_and(String::is_empty)
    {
        return Err(invalid(
            "runtime_profiles.security",
            "invalid credential reference structure",
        ));
    }
    let expected = CandidateProfile {
        engine: profile.engine,
        revision: profile.revision,
        executable: profile.executable.clone(),
        build_fingerprint: profile.build_fingerprint.clone(),
        args: profile.args.clone(),
        launch_settings: decode(
            &serde_json::to_value(&profile.launch_settings)
                .map_err(|_| invalid("profile", "launch encoding failed"))?,
            "profile.launch_settings",
        )?,
        env: profile.env.clone(),
        experimental_controls: profile.security.experimental_controls,
        runtime_auth: profile.security.credential_ref.is_some(),
        admin_auth: profile.security.admin_credential_ref.is_some(),
        log_policy: CandidateLogPolicy {
            max_file_bytes: profile.log_policy.max_file_bytes,
            retained_files: profile.log_policy.retained_files,
        },
    };
    if recipe.resolved_profile != expected {
        return Err(invalid(
            "effective_recipe.resolved_profile",
            "profile projection mismatch",
        ));
    }
    if recipe.host_devices != h.resource_policy.devices
        || recipe.host_device_sharing != h.resource_policy.device_sharing
    {
        return Err(invalid(
            "effective_recipe.host_devices",
            "host topology projection mismatch",
        ));
    }
    let host = core::normalize_host(h)?;
    let normalized_recipe = recipe.normalized();
    core::validate_recipe(&normalized_recipe, &host)?;
    let reviewed = reviewed(&input);
    let (reviewed_json, manifest_digest) = encode_reviewed(&input)?;
    let recipe_fingerprint = core::qualification_fingerprint(&normalized_recipe, &profile, &host)?;
    let credential_refs = CandidateCredentialRefs {
        runtime: profile.security.credential_ref.clone(),
        admin: profile.security.admin_credential_ref.clone(),
    };
    let descriptor = DescriptorBudget {
        version: 1,
        reviewed_manifest: &reviewed,
        manifest_digest: &manifest_digest,
        recipe_fingerprint: &recipe_fingerprint,
        credential_refs: DescriptorCredentialRefs {
            runtime: credential_refs.runtime(),
            admin: credential_refs.admin(),
        },
        total_case_request_budget: total,
    };
    let descriptor_size = serde_json::to_vec(&descriptor)
        .map_err(|_| invalid("candidate", "normalized descriptor encoding failed"))?
        .len();
    if descriptor_size > MAX_ENCODED {
        return Err(invalid("candidate", "normalized descriptor exceeds 1MiB"));
    }
    let mut effective_recipe = input.effective_recipe;
    effective_recipe.resolved_profile = expected;
    let output = NormalizedCandidateManifest {
        host: input.host,
        effective_recipe,
        limits: input.limits,
        cases: input.cases,
        reviewed_json,
        manifest_digest,
        recipe_fingerprint,
        credential_refs,
        total_case_request_budget: total,
    };
    Ok(output)
}

fn reviewed(input: &CandidateInput) -> Reviewed<'_> {
    Reviewed {
        schema_version: 1,
        kind: "candidate_recipe",
        host: &input.host,
        effective_recipe: &input.effective_recipe,
        limits: &input.limits,
        evaluator_suite: "recipe_v1",
        cases: &input.cases,
    }
}

fn encode_reviewed(input: &CandidateInput) -> Result<(Vec<u8>, String), ConfigError> {
    let reviewed_json = canonical_json(
        &serde_json::to_value(reviewed(input))
            .map_err(|_| invalid("candidate", "reviewed encoding failed"))?,
    )?;
    if reviewed_json.len() > MAX_ENCODED {
        return Err(invalid("candidate", "reviewed encoding exceeds 1MiB"));
    }
    let mut digest = Sha256::new();
    digest.update(DIGEST_DOMAIN);
    digest.update(&reviewed_json);
    Ok((reviewed_json, hex::encode(digest.finalize())))
}

fn validate_candidate_intrinsic(input: &CandidateInput) -> Result<u32, ConfigError> {
    if input.schema_version != 1
        || input.kind != "candidate_recipe"
        || input.evaluator_suite != "recipe_v1"
    {
        return Err(invalid(
            "candidate",
            "schema version 1, candidate kind, and recipe_v1 suite required",
        ));
    }
    validate_selector(&input.host.id, "host.id")?;
    validate_text(
        &input.host.hardware_fingerprint,
        "host.hardware_fingerprint",
        4096,
    )?;
    validate_text(
        &input.host.environment_fingerprint,
        "host.environment_fingerprint",
        4096,
    )?;
    let recipe = &input.effective_recipe;
    validate_candidate_primitives(recipe)?;
    validate_selector(&recipe.runtime_profile, "effective_recipe.runtime_profile")?;
    validate_text(&recipe.recipe, "effective_recipe.recipe", 4096)?;
    for (path, value) in [
        ("effective_recipe.model.path", &recipe.model.path),
        (
            "effective_recipe.model.content_fingerprint",
            &recipe.model.content_fingerprint,
        ),
        ("effective_recipe.model.revision", &recipe.model.revision),
    ] {
        validate_text(value, path, 4096)?;
    }
    if recipe.runtime_profile_revision == 0 || recipe.resolved_profile.revision == 0 {
        return Err(invalid(
            "effective_recipe.runtime_profile_revision",
            "must be positive",
        ));
    }
    if recipe.runtime_profile_revision != recipe.resolved_profile.revision {
        return Err(invalid(
            "effective_recipe.runtime_profile_revision",
            "profile revision mismatch",
        ));
    }
    let launch_engine = match recipe.resolved_profile.launch_settings {
        CandidateLaunch::Vllm { .. } => Engine::Vllm,
        CandidateLaunch::Sglang { .. } => Engine::Sglang,
        CandidateLaunch::Fake => Engine::Fake,
    };
    if recipe.resolved_profile.engine != launch_engine {
        return Err(invalid(
            "effective_recipe.resolved_profile.launch_settings",
            "engine mismatch",
        ));
    }
    validate_limits(&input.limits)?;
    if recipe.request_deadline_ms > input.limits.max_run_duration_ms {
        return Err(invalid(
            "effective_recipe.request_deadline_ms",
            "deadline exceeds manifest limit",
        ));
    }
    core::validate_recipe_intrinsic(&recipe.normalized())?;
    validate_cases(&input.cases, recipe.residency, input.limits.max_requests)
}

fn validate_text(v: &str, path: &str, max: usize) -> Result<(), ConfigError> {
    if v.is_empty() || v.len() > max {
        Err(invalid(path, "invalid bounded text"))
    } else {
        Ok(())
    }
}

fn validate_selector(v: &str, path: &str) -> Result<(), ConfigError> {
    validate_text(v, path, 256)
}

fn validate_case_id(v: &str) -> Result<(), ConfigError> {
    validate_text(v, "cases.id", 64)?;
    if !v
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(invalid("cases.id", "invalid case identifier"));
    }
    Ok(())
}

fn validate_candidate_primitives(recipe: &CandidateRecipe) -> Result<(), ConfigError> {
    let p = &recipe.resolved_profile;
    for value in [&p.executable, &p.build_fingerprint] {
        validate_text(value, "effective_recipe.resolved_profile", 4096)?;
    }
    for arg in &p.args {
        validate_text(arg, "effective_recipe.resolved_profile.args", 4096)?;
    }
    for (key, value) in &p.env {
        validate_selector(key, "effective_recipe.resolved_profile.env.key")?;
        validate_text(value, "effective_recipe.resolved_profile.env.value", 4096)?;
    }
    if !Path::new(&p.executable).is_absolute() {
        return Err(invalid(
            "effective_recipe.resolved_profile.executable",
            "must be absolute",
        ));
    }
    nonnegative_bytes(
        p.log_policy.max_file_bytes,
        "effective_recipe.resolved_profile.log_policy.max_file_bytes",
    )?;
    match &p.launch_settings {
        CandidateLaunch::Vllm {
            kv_cache_dtype,
            cpu_offload_bytes,
            requested_budget,
            ..
        } => {
            validate_text(
                kv_cache_dtype,
                "effective_recipe.resolved_profile.launch_settings.kv_cache_dtype",
                4096,
            )?;
            nonnegative_bytes(
                *cpu_offload_bytes,
                "effective_recipe.resolved_profile.launch_settings.cpu_offload_bytes",
            )?;
            nonnegative_bytes(
                requested_budget.kv_cache_bytes,
                "effective_recipe.resolved_profile.launch_settings.requested_budget.kv_cache_bytes",
            )?;
            nonnegative_bytes(
                requested_budget.swap_space_bytes,
                "effective_recipe.resolved_profile.launch_settings.requested_budget.swap_space_bytes",
            )?;
        }
        CandidateLaunch::Sglang {
            recipe,
            model_dtype,
            weight_restore,
            requested_budget,
            ..
        } => {
            for value in [recipe, model_dtype, weight_restore] {
                validate_text(
                    value,
                    "effective_recipe.resolved_profile.launch_settings",
                    4096,
                )?;
            }
            nonnegative_bytes(
                requested_budget.kv_cache_bytes,
                "effective_recipe.resolved_profile.launch_settings.requested_budget.kv_cache_bytes",
            )?;
        }
        CandidateLaunch::Fake => {}
    }
    for (key, device) in &recipe.host_devices {
        validate_selector(key, "effective_recipe.host_devices.key")?;
        validate_selector(&device.domain, "effective_recipe.host_devices.domain")?;
    }
    for device in &recipe.devices {
        validate_selector(&device.id, "effective_recipe.devices.id")?;
    }
    for phase in [
        &recipe.resources.cold,
        &recipe.resources.ready,
        &recipe.resources.parking,
        &recipe.resources.parked,
        &recipe.resources.wake,
    ] {
        for allocation in &phase.allocations {
            validate_selector(
                &allocation.domain,
                "effective_recipe.resources.allocations.domain",
            )?;
            nonnegative_bytes(
                allocation.bytes,
                "effective_recipe.resources.allocations.bytes",
            )?;
            nonnegative_bytes(
                allocation.host_kv_bytes,
                "effective_recipe.resources.allocations.host_kv_bytes",
            )?;
        }
        for device in &phase.devices {
            validate_selector(&device.id, "effective_recipe.resources.devices.id")?;
        }
    }
    Ok(())
}

fn nonnegative_bytes(value: i64, path: &str) -> Result<(), ConfigError> {
    if value < 0 {
        Err(invalid(path, "negative byte quantity"))
    } else {
        Ok(())
    }
}

fn validate_limits(v: &CandidateLimits) -> Result<(), ConfigError> {
    if !(1..=86_400_000).contains(&v.max_run_duration_ms)
        || !(1..=3_600_000).contains(&v.max_cleanup_duration_ms)
        || !(1..=4096).contains(&v.max_requests)
        || !(1..=1_048_576).contains(&v.max_request_body_bytes)
        || !(1..=131_072).contains(&v.max_input_tokens_per_request)
        || !(1..=16_384).contains(&v.max_output_tokens_per_request)
    {
        Err(invalid("limits", "limits exceed bounded positive ranges"))
    } else {
        Ok(())
    }
}

fn validate_cases(
    cases: &[CandidateCase],
    residency: Residency,
    max: u32,
) -> Result<u32, ConfigError> {
    if !(1..=128).contains(&cases.len()) {
        return Err(invalid("cases", "case count must be 1..128"));
    }
    let mut ids = BTreeSet::new();
    let mut pairs = BTreeSet::new();
    let mut total = 0u32;
    let mut marker: Option<(&str, u32)> = None;
    // Work depends only on the bounded case count, never on an untrusted cycle.
    if !cases.len().is_multiple_of(5)
        || (residency == Residency::Warm && cases.len() < 10)
        || (residency == Residency::RestartOnly && cases.len() != 5)
    {
        return Err(invalid("cases", "residency cycle contract mismatch"));
    }
    for (index, case) in cases.iter().enumerate() {
        let cycle = (index / 5) as u32;
        let kinds = if cycle == 0 {
            [
                CandidateCaseKind::ColdInitialize,
                CandidateCaseKind::ReadyProbe,
                CandidateCaseKind::MarkerNonstreaming,
                CandidateCaseKind::MarkerStreaming,
                CandidateCaseKind::Security,
            ]
        } else {
            [
                CandidateCaseKind::Park,
                CandidateCaseKind::Restore,
                CandidateCaseKind::ReadyProbe,
                CandidateCaseKind::MarkerNonstreaming,
                CandidateCaseKind::MarkerStreaming,
            ]
        };
        if (case.cycle, case.kind) != (cycle, kinds[index % 5]) {
            return Err(invalid("cases", "ordered recipe_v1 suite mismatch"));
        }
    }
    for c in cases {
        validate_case_id(&c.id)?;
        if !ids.insert(&c.id) || !pairs.insert((c.cycle, c.kind)) {
            return Err(invalid("cases", "duplicate case id or cycle/kind"));
        }
        let is_marker = matches!(
            c.kind,
            CandidateCaseKind::MarkerNonstreaming | CandidateCaseKind::MarkerStreaming
        );
        if is_marker {
            let d = c
                .corpus_digest
                .as_deref()
                .ok_or_else(|| invalid("cases.corpus_digest", "marker digest required"))?;
            if d.len() != 64
                || !d
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(invalid(
                    "cases.corpus_digest",
                    "digest must be lowercase SHA-256 hex",
                ));
            }
            if !(1..=4096).contains(&c.count) || c.request_budget < c.count {
                return Err(invalid("cases", "invalid marker count or budget"));
            }
            if let Some(m) = marker {
                if m != (d, c.count) {
                    return Err(invalid("cases", "marker corpus/count drift"));
                }
            } else {
                marker = Some((d, c.count));
            }
        } else {
            if c.corpus_digest.is_some() || c.count != 1 {
                return Err(invalid("cases", "non-marker corpus/count invalid"));
            }
            if matches!(
                c.kind,
                CandidateCaseKind::ReadyProbe | CandidateCaseKind::Security
            ) && c.request_budget == 0
            {
                return Err(invalid(
                    "cases.request_budget",
                    "probe budget must be positive",
                ));
            }
        }
        if c.request_budget > 4096 {
            return Err(invalid("cases.request_budget", "case budget exceeds 4096"));
        }
        total = total
            .checked_add(c.request_budget)
            .ok_or_else(|| invalid("cases.request_budget", "total budget overflow"))?;
    }
    if total > max {
        return Err(invalid(
            "cases.request_budget",
            "total budget exceeds max_requests",
        ));
    }
    Ok(total)
}

impl CandidateRecipe {
    fn normalized(&self) -> core::NormalizedRecipe {
        let phase = |p: &CandidatePhase| PhaseFootprint {
            allocations: p
                .allocations
                .iter()
                .map(|a| Allocation {
                    domain: a.domain.clone(),
                    bytes: a.bytes,
                    host_kv_bytes: a.host_kv_bytes,
                })
                .collect(),
            devices: p.devices.clone(),
        };
        core::NormalizedRecipe {
            model: self.model.clone(),
            recipe: self.recipe.clone(),
            residency: self.residency,
            recovery: self.recovery,
            devices: self.devices.clone(),
            resources: RecipeFootprints {
                cold: phase(&self.resources.cold),
                ready: phase(&self.resources.ready),
                parking: phase(&self.resources.parking),
                parked: phase(&self.resources.parked),
                wake: phase(&self.resources.wake),
            },
            request_deadline_ms: self.request_deadline_ms,
        }
    }
}

fn canonical_json(value: &Value) -> Result<Vec<u8>, ConfigError> {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, v)| (k.clone(), sort(v)))
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            v => v.clone(),
        }
    }
    serde_json::to_vec(&sort(value)).map_err(|_| invalid("candidate", "canonical encoding failed"))
}

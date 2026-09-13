//! Strict, informational normalization of reviewed candidate recipes.

use super::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_ENCODED: usize = 1 << 20;
const DIGEST_DOMAIN: &[u8] = b"mllm.candidate-manifest.v1\0";

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    corpus_digest: Option<String>,
}
impl CandidateCase {
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
enum CandidateCaseKind {
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
struct CandidateLogPolicy {
    max_file_bytes: i64,
    retained_files: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "engine", rename_all = "lowercase", deny_unknown_fields)]
enum CandidateLaunch {
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
struct CandidateVllmBudget {
    kv_cache_bytes: i64,
    swap_space_bytes: i64,
    gpu_utilization_pct: u8,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateSglangBudget {
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
struct CandidatePhase {
    allocations: Vec<CandidateAllocation>,
    devices: Vec<DeviceClaim>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateAllocation {
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

/// Normalize a pre-parsed candidate. Callers must use the text API when duplicate-key
/// rejection is required because `serde_json::Value` cannot retain duplicate keys.
pub fn normalize_candidate_manifest(
    value: &Value,
    trusted_host: &Value,
) -> Result<NormalizedCandidateManifest, ConfigError> {
    check_size(value, "candidate")?;
    let input: CandidateInput = decode(value, "candidate")?;
    normalize(input, trusted_host)
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

fn normalize(
    input: CandidateInput,
    trusted_host: &Value,
) -> Result<NormalizedCandidateManifest, ConfigError> {
    if input.schema_version != 1
        || input.kind != "candidate_recipe"
        || input.evaluator_suite != "recipe_v1"
    {
        return Err(invalid(
            "candidate",
            "schema version 1, candidate kind, and recipe_v1 suite required",
        ));
    }
    validate_text(&input.host.id, "host.id", 256, false)?;
    validate_text(
        &input.host.hardware_fingerprint,
        "host.hardware_fingerprint",
        4096,
        true,
    )?;
    validate_text(
        &input.host.environment_fingerprint,
        "host.environment_fingerprint",
        4096,
        true,
    )?;
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
    validate_text(
        &recipe.runtime_profile,
        "effective_recipe.runtime_profile",
        256,
        false,
    )?;
    validate_text(&recipe.recipe, "effective_recipe.recipe", 4096, true)?;
    for (p, v) in [
        ("effective_recipe.model.path", &recipe.model.path),
        (
            "effective_recipe.model.content_fingerprint",
            &recipe.model.content_fingerprint,
        ),
        ("effective_recipe.model.revision", &recipe.model.revision),
    ] {
        validate_text(v, p, 4096, true)?;
    }
    if !Path::new(&recipe.model.path).is_absolute() {
        return Err(invalid("effective_recipe.model.path", "must be absolute"));
    }
    if recipe.runtime_profile_revision == 0 {
        return Err(invalid(
            "effective_recipe.runtime_profile_revision",
            "must be positive",
        ));
    }
    let raw = h
        .runtime_profiles
        .get(&recipe.runtime_profile)
        .ok_or_else(|| {
            invalid(
                "effective_recipe.runtime_profile",
                "unknown runtime profile",
            )
        })?;
    match &raw.qualification_id {
        MissingAwareQualification::Missing | MissingAwareQualification::Present(Some(_)) => {}
        MissingAwareQualification::Present(None) => {
            return Err(invalid(
                "runtime_profiles.qualification_id",
                "present qualification reference must be a nonempty string",
            ))
        }
    }
    if matches!(&raw.qualification_id, MissingAwareQualification::Present(Some(value)) if value.is_empty())
    {
        return Err(invalid(
            "runtime_profiles.qualification_id",
            "present qualification reference must be a nonempty string",
        ));
    }
    if raw.revision != recipe.runtime_profile_revision
        || recipe.resolved_profile.revision != raw.revision
    {
        return Err(invalid(
            "effective_recipe.runtime_profile_revision",
            "profile revision mismatch",
        ));
    }
    let launch = normalize_launch(raw.launch_settings.clone(), raw.engine, recipe.residency)?;
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
        || matches!(raw.engine, Engine::Vllm | Engine::Sglang)
            && raw.security.credential_ref.is_none()
        || raw.engine == Engine::Sglang && raw.security.admin_credential_ref.is_none()
        || raw.security.admin_credential_ref.is_some()
            && raw.security.admin_credential_ref.as_ref() == raw.security.credential_ref.as_ref()
    {
        return Err(invalid(
            "runtime_profiles.security",
            "invalid credential reference structure",
        ));
    }
    validate_profile_args(raw.engine, &raw.args)
        .map_err(|e| invalid("runtime_profiles.args", e.to_string()))?;
    validate_profile_env(&raw.env).map_err(|_| {
        invalid(
            "runtime_profiles.env",
            "environment name is not allowlisted",
        )
    })?;
    let expected = CandidateProfile {
        engine: raw.engine,
        revision: raw.revision,
        executable: raw.executable.clone(),
        build_fingerprint: raw.build_fingerprint.clone(),
        args: raw.args.clone(),
        launch_settings: decode(
            &serde_json::to_value(&launch)
                .map_err(|_| invalid("profile", "launch encoding failed"))?,
            "profile.launch_settings",
        )?,
        env: raw.env.clone(),
        experimental_controls: raw.security.experimental_controls,
        runtime_auth: raw.security.credential_ref.is_some(),
        admin_auth: raw.security.admin_credential_ref.is_some(),
        log_policy: CandidateLogPolicy {
            max_file_bytes: parse_bytes(&raw.log_policy.max_file_bytes)?,
            retained_files: raw.log_policy.retained_files,
        },
    };
    if recipe.resolved_profile != expected {
        return Err(invalid(
            "effective_recipe.resolved_profile",
            "profile projection mismatch",
        ));
    }
    if !Path::new(&expected.executable).is_absolute() {
        return Err(invalid(
            "effective_recipe.resolved_profile.executable",
            "must be absolute",
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
    validate_recipe(recipe, &h)?;
    let queue_deadline = h
        .resource_policy
        .queue
        .as_ref()
        .and_then(|q| q.request_deadline.as_deref())
        .map(parse_duration_ms)
        .transpose()?
        .unwrap_or(DEFAULT_REQUEST_DEADLINE_MS);
    if recipe.request_deadline_ms <= 0
        || recipe.request_deadline_ms > queue_deadline
        || recipe.request_deadline_ms > input.limits.max_run_duration_ms
    {
        return Err(invalid(
            "effective_recipe.request_deadline_ms",
            "deadline exceeds manifest or host limit",
        ));
    }
    validate_limits(&input.limits)?;
    let total = validate_cases(&input.cases, recipe.residency, input.limits.max_requests)?;
    let reviewed = Reviewed {
        schema_version: 1,
        kind: "candidate_recipe",
        host: &input.host,
        effective_recipe: recipe,
        limits: &input.limits,
        evaluator_suite: "recipe_v1",
        cases: &input.cases,
    };
    let reviewed_json = canonical_json(
        &serde_json::to_value(reviewed)
            .map_err(|_| invalid("candidate", "reviewed encoding failed"))?,
    )?;
    if reviewed_json.len() > MAX_ENCODED {
        return Err(invalid("candidate", "reviewed encoding exceeds 1MiB"));
    }
    let mut digest = Sha256::new();
    digest.update(DIGEST_DOMAIN);
    digest.update(&reviewed_json);
    let manifest_digest = hex::encode(digest.finalize());
    let recipe_fingerprint = recipe_fingerprint(recipe, &expected, &input.host)?;
    let credential_refs = CandidateCredentialRefs {
        runtime: raw.security.credential_ref.clone(),
        admin: raw.security.admin_credential_ref.clone(),
    };
    let output = NormalizedCandidateManifest {
        host: input.host,
        effective_recipe: input.effective_recipe,
        limits: input.limits,
        cases: input.cases,
        reviewed_json,
        manifest_digest,
        recipe_fingerprint,
        credential_refs,
        total_case_request_budget: total,
    };
    let descriptor = output.reviewed_json.len()
        + output
            .credential_refs
            .runtime
            .as_ref()
            .map_or(0, String::len)
        + output.credential_refs.admin.as_ref().map_or(0, String::len);
    if descriptor > MAX_ENCODED {
        return Err(invalid("candidate", "normalized descriptor exceeds 1MiB"));
    }
    Ok(output)
}

fn validate_text(v: &str, path: &str, max: usize, any: bool) -> Result<(), ConfigError> {
    if v.is_empty()
        || v.len() > max
        || (!any
            && !v
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')))
    {
        Err(invalid(path, "invalid bounded identifier or text"))
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
    let max_cycle = cases.iter().map(|c| c.cycle).max().unwrap_or(0);
    if (residency == Residency::Warm && max_cycle == 0)
        || (residency == Residency::RestartOnly && max_cycle != 0)
    {
        return Err(invalid("cases", "residency cycle contract mismatch"));
    }
    let mut expected = vec![
        (0, CandidateCaseKind::ColdInitialize),
        (0, CandidateCaseKind::ReadyProbe),
        (0, CandidateCaseKind::MarkerNonstreaming),
        (0, CandidateCaseKind::MarkerStreaming),
        (0, CandidateCaseKind::Security),
    ];
    for cycle in 1..=max_cycle {
        for kind in [
            CandidateCaseKind::Park,
            CandidateCaseKind::Restore,
            CandidateCaseKind::ReadyProbe,
            CandidateCaseKind::MarkerNonstreaming,
            CandidateCaseKind::MarkerStreaming,
        ] {
            expected.push((cycle, kind));
        }
    }
    if cases.len() != expected.len()
        || cases
            .iter()
            .zip(&expected)
            .any(|(c, e)| (c.cycle, c.kind) != *e)
    {
        return Err(invalid("cases", "ordered recipe_v1 suite mismatch"));
    }
    for c in cases {
        validate_text(&c.id, "cases.id", 64, false)?;
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

fn validate_recipe(r: &CandidateRecipe, h: &HostInput) -> Result<(), ConfigError> {
    let convert = |p: &CandidatePhase, phase| domain::PhaseFootprint {
        phase,
        allocations: p
            .allocations
            .iter()
            .map(|a| domain::Allocation {
                domain: a.domain.clone(),
                bytes: a.bytes,
                host_kv_bytes: a.host_kv_bytes,
            })
            .collect(),
        devices: p
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
    domain::validate_recipe(&domain::RecipeFootprints {
        cold: convert(&r.resources.cold, domain::ResourcePhase::Cold),
        ready: convert(&r.resources.ready, domain::ResourcePhase::Ready),
        parking: convert(&r.resources.parking, domain::ResourcePhase::Parking),
        parked: convert(&r.resources.parked, domain::ResourcePhase::Parked),
        wake: convert(&r.resources.wake, domain::ResourcePhase::Wake),
    })
    .map_err(|e| invalid("effective_recipe.resources", e.to_string()))?;
    let selected: BTreeMap<_, _> = r.devices.iter().map(|d| (&d.id, d.sharing)).collect();
    if selected.len() != r.devices.len() {
        return Err(invalid(
            "effective_recipe.devices",
            "device IDs must be unique",
        ));
    }
    for d in &r.devices {
        let p = h
            .resource_policy
            .devices
            .get(&d.id)
            .ok_or_else(|| invalid("effective_recipe.devices", "unknown device"))?;
        if (h.resource_policy.device_sharing == Sharing::Exclusive
            || p.sharing == Sharing::Exclusive)
            && d.sharing == Sharing::Shared
        {
            return Err(invalid("effective_recipe.devices", "sharing exceeds host"));
        }
    }
    for p in [
        &r.resources.cold,
        &r.resources.ready,
        &r.resources.parking,
        &r.resources.parked,
        &r.resources.wake,
    ] {
        for a in &p.allocations {
            if !h.resource_policy.domains.contains_key(&a.domain) {
                return Err(invalid(
                    "effective_recipe.resources.allocations.domain",
                    "unknown domain",
                ));
            }
            if a.bytes < 0 || a.host_kv_bytes < 0 {
                return Err(invalid(
                    "effective_recipe.resources.allocations",
                    "negative bytes",
                ));
            }
        }
        for d in &p.devices {
            if selected.get(&d.id) != Some(&d.sharing) {
                return Err(invalid(
                    "effective_recipe.resources.devices",
                    "phase claim mismatch",
                ));
            }
        }
    }
    Ok(())
}

fn recipe_fingerprint(
    r: &CandidateRecipe,
    p: &CandidateProfile,
    h: &CandidateHost,
) -> Result<String, ConfigError> {
    #[derive(Serialize)]
    struct Q<'a> {
        model: &'a ModelIdentity,
        recipe: &'a str,
        residency: Residency,
        recovery: Recovery,
        devices: &'a [DeviceClaim],
        resources: &'a CandidateResources,
        host_devices: &'a BTreeMap<String, DevicePolicy>,
        device_sharing: Sharing,
        engine: Engine,
        revision: u64,
        executable: &'a str,
        build_fingerprint: &'a str,
        args: &'a [String],
        launch_settings: &'a CandidateLaunch,
        env: &'a BTreeMap<String, String>,
        experimental_controls: bool,
        runtime_auth: bool,
        admin_auth: bool,
        log_policy: &'a CandidateLogPolicy,
        hardware_fingerprint: &'a str,
        environment_fingerprint: &'a str,
    }
    let q = Q {
        model: &r.model,
        recipe: &r.recipe,
        residency: r.residency,
        recovery: r.recovery,
        devices: &r.devices,
        resources: &r.resources,
        host_devices: &r.host_devices,
        device_sharing: r.host_device_sharing,
        engine: p.engine,
        revision: p.revision,
        executable: &p.executable,
        build_fingerprint: &p.build_fingerprint,
        args: &p.args,
        launch_settings: &p.launch_settings,
        env: &p.env,
        experimental_controls: p.experimental_controls,
        runtime_auth: p.runtime_auth,
        admin_auth: p.admin_auth,
        log_policy: &p.log_policy,
        hardware_fingerprint: &h.hardware_fingerprint,
        environment_fingerprint: &h.environment_fingerprint,
    };
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(&q).map_err(|_| invalid("fingerprint", "encoding failed"))?,
    )))
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

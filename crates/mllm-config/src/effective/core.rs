//! Private normalization shared by ordinary deployments and candidate manifests.

use super::*;

pub(super) struct NormalizedProfile {
    pub(super) engine: Engine,
    pub(super) revision: u64,
    pub(super) executable: String,
    pub(super) build_fingerprint: String,
    pub(super) args: Vec<String>,
    pub(super) launch_settings: ProfileLaunchSettings,
    pub(super) env: BTreeMap<String, String>,
    pub(super) security: Security,
    pub(super) log_policy: LogPolicy,
}

pub(super) struct NormalizedRecipe {
    pub(super) model: ModelIdentity,
    pub(super) recipe: String,
    pub(super) residency: Residency,
    pub(super) recovery: Recovery,
    pub(super) devices: Vec<DeviceClaim>,
    pub(super) resources: RecipeFootprints,
    pub(super) request_deadline_ms: i64,
}

pub(super) fn normalize_profile(
    raw_profile: &RawProfile,
    revision: u64,
    residency: Residency,
) -> Result<NormalizedProfile, ConfigError> {
    if raw_profile.revision != revision {
        return Err(invalid(
            "runtime_profile_revision",
            "profile revision mismatch",
        ));
    }
    if !Path::new(&raw_profile.executable).is_absolute() {
        return Err(invalid("runtime_profiles.executable", "must be absolute"));
    }
    if raw_profile.build_fingerprint.is_empty()
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
    validate_profile_env(&raw_profile.env).map_err(|_| {
        invalid(
            "runtime_profiles.env",
            "environment name is not allowlisted",
        )
    })?;
    let launch_settings = normalize_launch(
        raw_profile.launch_settings.clone(),
        raw_profile.engine,
        residency,
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
    let profile = NormalizedProfile {
        engine: raw_profile.engine,
        revision: raw_profile.revision,
        executable: raw_profile.executable.clone(),
        build_fingerprint: raw_profile.build_fingerprint.clone(),
        args: raw_profile.args.clone(),
        launch_settings,
        env: raw_profile.env.clone(),
        security: raw_profile.security.clone(),
        log_policy: LogPolicy {
            max_file_bytes: parse_bytes(&raw_profile.log_policy.max_file_bytes)?,
            retained_files: raw_profile.log_policy.retained_files,
        },
    };
    Ok(profile)
}

pub(super) fn normalize_host(h: HostInput) -> Result<HostPolicy, ConfigError> {
    if h.schema_version != 1 || h.kind != "host" {
        return Err(invalid("host", "schema version 1 and host kind required"));
    }
    for (path, value) in [
        ("host.name", &h.name),
        ("hardware_fingerprint", &h.hardware_fingerprint),
        ("environment_fingerprint", &h.environment_fingerprint),
    ] {
        if value.is_empty() {
            return Err(invalid(path, "must not be empty"));
        }
    }
    let mut domains = BTreeMap::new();
    for (name, raw) in h.resource_policy.domains {
        let value = DomainPolicy {
            managed_limit: parse_bytes(&raw.managed_limit)?,
            free_reserve: parse_bytes(&raw.free_reserve)?,
            host_kv_limit: raw.host_kv_limit.as_deref().map(parse_bytes).transpose()?,
            parked_limit: raw.parked_limit.as_deref().map(parse_bytes).transpose()?,
            memory: raw.memory,
        };
        domains.insert(name, value);
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
    ResourceControls::from_host(&host).validate(&ResourceContext::from_host(&host))?;
    Ok(host)
}

fn domain_phase(
    public: &PhaseFootprint,
    expected: domain::ResourcePhase,
) -> domain::PhaseFootprint {
    domain::PhaseFootprint {
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
    }
}

pub(super) fn validate_recipe(d: &NormalizedRecipe, host: &HostPolicy) -> Result<(), ConfigError> {
    validate_recipe_intrinsic(d)?;
    let resources = &d.resources;
    for claim in &d.devices {
        let policy = host
            .devices
            .get(&claim.id)
            .ok_or_else(|| invalid("devices", "unknown device"))?;
        if host.device_sharing == Sharing::Exclusive && claim.sharing == Sharing::Shared
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
            if !host.domains.contains_key(&a.domain) {
                return Err(invalid("resources.allocations.domain", "unknown domain"));
            }
        }
    }
    if d.request_deadline_ms > host.queue.request_deadline_ms {
        return Err(invalid(
            "request_deadline",
            "deployment deadline may only shorten host limit",
        ));
    }
    Ok(())
}

pub(super) fn validate_recipe_intrinsic(d: &NormalizedRecipe) -> Result<(), ConfigError> {
    for (path, value) in [
        ("model.path", &d.model.path),
        ("model.content_fingerprint", &d.model.content_fingerprint),
        ("model.revision", &d.model.revision),
        ("recipe", &d.recipe),
    ] {
        if value.is_empty() {
            return Err(invalid(path, "must not be empty"));
        }
    }
    if !Path::new(&d.model.path).is_absolute() {
        return Err(invalid("model.path", "must be absolute"));
    }
    let resources = &d.resources;
    domain::validate_recipe(&domain::RecipeFootprints {
        cold: domain_phase(&resources.cold, domain::ResourcePhase::Cold),
        ready: domain_phase(&resources.ready, domain::ResourcePhase::Ready),
        parking: domain_phase(&resources.parking, domain::ResourcePhase::Parking),
        parked: domain_phase(&resources.parked, domain::ResourcePhase::Parked),
        wake: domain_phase(&resources.wake, domain::ResourcePhase::Wake),
    })
    .map_err(|e| invalid("resources", e.to_string()))?;
    let selected: BTreeMap<_, _> = d
        .devices
        .iter()
        .map(|x| (x.id.as_str(), x.sharing))
        .collect();
    if selected.len() != d.devices.len() {
        return Err(invalid("devices", "device IDs must be unique"));
    }
    for p in [
        &resources.cold,
        &resources.ready,
        &resources.parking,
        &resources.parked,
        &resources.wake,
    ] {
        for claim in &p.devices {
            if selected.get(claim.id.as_str()) != Some(&claim.sharing) {
                return Err(invalid(
                    "resources.devices",
                    "phase device claim must match selected deployment claim",
                ));
            }
        }
    }
    if d.request_deadline_ms <= 0 {
        return Err(invalid(
            "request_deadline",
            "deployment deadline may only shorten host limit",
        ));
    }
    Ok(())
}

pub(super) fn qualification_fingerprint(
    d: &NormalizedRecipe,
    profile: &NormalizedProfile,
    host: &HostPolicy,
) -> Result<String, ConfigError> {
    let resources = &d.resources;
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
        resources,
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
    Ok(qualification_fingerprint)
}

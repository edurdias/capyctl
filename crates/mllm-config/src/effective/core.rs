//! Private normalization shared by the ordinary deployment paths.

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
    // Spec §3: `--trust-remote-code` makes the engine execute Python that arrived
    // with the checkpoint. The flag stays on the approved list because there are
    // models that need it, but a profile may only pass it where the host has said
    // so in as many words.
    if !raw_profile.security.trust_remote_code
        && raw_profile
            .args
            .iter()
            .any(|argument| normalize_option_name(argument) == "--trust-remote-code")
    {
        return Err(invalid(
            "runtime_profiles.security.trust_remote_code",
            "`--trust-remote-code` executes code shipped with the checkpoint; it \
             requires security.trust_remote_code: true on this profile",
        ));
    }
    // Spec §3: a host may switch deep park off. A deployment that asks to park on
    // such a profile is refused here rather than launched and then found unable to
    // park, which would surface only under memory pressure.
    if raw_profile.security.deep_park == DeepPark::Disabled && residency.parks() {
        return Err(invalid(
            "runtime_profiles.security.deep_park",
            "a parking deployment cannot run on a profile that disables deep park; \
             set deep_park: enabled or residency: restart_only",
        ));
    }
    let launch_settings = normalize_launch(
        raw_profile.launch_settings.clone(),
        raw_profile.engine,
        residency,
    )?;
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

/// Resolve a deployment's `model` block into its identity.
///
/// Spec §7: `path` and `source` are two spellings of the same thing and exactly
/// one may appear. `store` is the host's model store, or `None` where no host is
/// in hand — a command fingerprint is computed from the document alone, so it
/// resolves nothing and every `resolved_path` is `None` there.
///
/// An absolute local path is used as written, including one that leaves the store.
/// That is deliberate: the operator writing the host file decides where weights
/// may live, and confining paths to the store would stop a host from serving a
/// checkpoint it already has elsewhere.
pub(super) fn normalize_model(
    raw: RawModel,
    store: Option<&Path>,
) -> Result<ModelIdentity, ConfigError> {
    let source = match (raw.path, raw.source) {
        (Some(_), Some(_)) => {
            return Err(invalid(
                "model",
                "state either `path` or `source`, not both",
            ))
        }
        (None, None) => return Err(invalid("model", "a model source is required")),
        (Some(path), None) => ModelSource::Local { path },
        (None, Some(source)) => source,
    };
    match &source {
        ModelSource::Local { path } => {
            if path.is_empty() {
                return Err(invalid("model.source.path", "must not be empty"));
            }
        }
        ModelSource::HuggingFace {
            repo,
            revision,
            locked_commit,
        } => {
            if repo.is_empty() {
                return Err(invalid("model.source.repo", "must not be empty"));
            }
            for (path, value) in [
                ("model.source.revision", revision),
                ("model.source.locked_commit", locked_commit),
            ] {
                if value.as_ref().is_some_and(String::is_empty) {
                    return Err(invalid(path, "must not be empty when stated"));
                }
            }
        }
        ModelSource::Http { url, sha256 } => {
            // Spec §7: weights fetched over plain HTTP could be replaced in flight,
            // and a digest is the only thing that makes the fetch reproducible, so
            // both are required rather than recommended.
            if !url.starts_with("https://") || url.len() <= "https://".len() {
                return Err(invalid("model.source.url", "must be an https:// URL"));
            }
            if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(invalid(
                    "model.source.sha256",
                    "must be 64 hexadecimal characters",
                ));
            }
        }
    }
    let resolved_path = match (&source, store) {
        (ModelSource::Local { path }, Some(store)) => {
            let candidate = Path::new(path);
            let resolved = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                store.join(candidate)
            };
            Some(
                resolved
                    .to_str()
                    .ok_or_else(|| invalid("model.source.path", "must be valid UTF-8"))?
                    .to_owned(),
            )
        }
        _ => None,
    };
    Ok(ModelIdentity {
        source,
        resolved_path,
        content_fingerprint: raw.content_fingerprint,
        revision: raw.revision,
    })
}

/// Spec §3: admission reserves the Ready footprint before the engine starts. A
/// requested KV cache larger than that reservation would hand the engine a grant
/// nothing accounted for, and the overrun would appear as an out-of-memory kill
/// well after the deployment was accepted.
pub(super) fn validate_requested_budget(
    settings: &ProfileLaunchSettings,
    resources: &RecipeFootprints,
) -> Result<(), ConfigError> {
    let requested = match settings {
        ProfileLaunchSettings::Vllm(s) => s.requested_budget.kv_cache_bytes,
        ProfileLaunchSettings::Sglang(s) => s.requested_budget.kv_cache_bytes,
        ProfileLaunchSettings::Fake(_) => return Ok(()),
    };
    // The Ready phase may name one allocation per domain; the KV cache is spread
    // across them, so the bound is their total.
    let ready = resources
        .ready
        .allocations
        .iter()
        .fold(0_i64, |total, a| total.saturating_add(a.bytes));
    if requested > ready {
        return Err(invalid(
            "runtime_profiles.launch_settings.requested_budget",
            "requested KV exceeds the Ready allocation admission accounts for",
        ));
    }
    Ok(())
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
    // Spec §7: the store anchors every relative model path, so it has to be a
    // place, not a fragment that means something different per working directory.
    let model_store = PathBuf::from(h.model_store.path);
    if !model_store.is_absolute() {
        return Err(invalid("host.model_store.path", "must be absolute"));
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
    let host = HostPolicy {
        name: h.name,
        hardware_fingerprint: h.hardware_fingerprint,
        environment_fingerprint: h.environment_fingerprint,
        model_store,
        domains,
        devices: h.resource_policy.devices,
        max_parked,
        observation_ttl_ms,
        device_sharing: h.resource_policy.device_sharing,
        endpoint_port_range: h.resource_policy.endpoint_port_range,
        planner_max_states,
        queue,
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
            let domain = host
                .domains
                .get(&a.domain)
                .ok_or_else(|| invalid("resources.allocations.domain", "unknown domain"))?;
            // SPEC §6.2's host-backed park retains a weight backup in host RAM. Where a
            // domain's device and host memory are one pool, that allocates from the
            // pool it is supposed to free, so the park succeeds and releases nothing.
            // The failure is otherwise silent, which is why it is refused here rather
            // than at first park. Every phase is checked, not just one, because this
            // check should not depend on another validator's guarantee (elsewhere in
            // this module) that every phase names the same domain set.
            if d.residency == Residency::HostBacked && domain.memory == DomainMemory::Unified {
                return Err(invalid(
                    "residency",
                    format!(
                        "host_backed retains weights in host memory, but domain \
                         '{}' declares device and host memory as one pool, so it \
                         would free nothing; use deep or restart_only",
                        a.domain
                    ),
                ));
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
        ("model.content_fingerprint", &d.model.content_fingerprint),
        ("model.revision", &d.model.revision),
        ("recipe", &d.recipe),
    ] {
        if value.is_empty() {
            return Err(invalid(path, "must not be empty"));
        }
    }
    // The model source itself is checked by `normalize_model`, which is the only
    // way a `ModelIdentity` is built; there is no absolute-path rule left here
    // because a relative local path is legal and resolves against the host store.
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

pub(super) fn recipe_fingerprint(
    d: &NormalizedRecipe,
    profile: &NormalizedProfile,
    host: &HostPolicy,
) -> Result<String, ConfigError> {
    let resources = &d.resources;
    #[derive(Serialize)]
    struct Recipe<'a> {
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
        deep_park: DeepPark,
        trust_remote_code: bool,
        runtime_auth: bool,
        admin_auth: bool,
        log_policy: &'a LogPolicy,
        hardware_fingerprint: &'a str,
        environment_fingerprint: &'a str,
    }
    let material = Recipe {
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
        deep_park: profile.security.deep_park,
        trust_remote_code: profile.security.trust_remote_code,
        runtime_auth: profile.security.credential_ref.is_some(),
        admin_auth: profile.security.admin_credential_ref.is_some(),
        log_policy: &profile.log_policy,
        hardware_fingerprint: &host.hardware_fingerprint,
        environment_fingerprint: &host.environment_fingerprint,
    };
    let recipe_fingerprint = hex::encode(Sha256::digest(
        serde_json::to_vec(&material).map_err(|e| invalid("fingerprint", e.to_string()))?,
    ));
    Ok(recipe_fingerprint)
}

//! Private normalization shared by the ordinary deployment paths.

use super::*;

pub(super) struct NormalizedProfile {
    pub(super) engine: Engine,
    pub(super) revision: u64,
    pub(super) executable: String,
    pub(super) build_fingerprint: String,
    pub(super) args: Vec<String>,
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
    // ADR 0014 §1, §3: host-fixed arguments are the installation's own; they
    // are free of the approved-flag list but never of the reserved one.
    let sleep_mode = raw_profile.security.deep_park.is_enabled() && residency.parks();
    validate_profile_args(raw_profile.engine, &raw_profile.args, sleep_mode)
        .map_err(|e| invalid("runtime_profiles.args", e.to_string()))?;
    validate_profile_env(&raw_profile.env).map_err(|_| {
        invalid(
            "runtime_profiles.env",
            "environment name is not allowlisted",
        )
    })?;
    // Spec §3: `--trust-remote-code` makes the engine execute Python that arrived
    // with the checkpoint. There are models that need it, but a profile may only
    // pass it where the host has said so in as many words.
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
    // SPEC §9.1 / T21 / ADR 0012: deep park is on unless the host opts out. A
    // deployment that asks to park on an opted-out profile is refused here rather
    // than launched and then found unable to park, which would surface only under
    // memory pressure.
    if raw_profile.security.deep_park == DeepPark::Disabled && residency.parks() {
        return Err(invalid(
            "runtime_profiles.security.deep_park",
            "a parking deployment cannot run on a profile that opts out of deep park \
             (deep_park: disabled); use residency: restart_only or remove the opt-out",
        ));
    }
    for path in &raw_profile.security.approved_paths {
        if !Path::new(path).is_absolute()
            || Path::new(path).components().any(|c| {
                !matches!(
                    c,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
        {
            return Err(invalid(
                "runtime_profiles.security.approved_paths",
                "every approved path must be an absolute, normalized directory",
            ));
        }
    }
    if raw_profile
        .security
        .approved_options
        .iter()
        .any(|name| !name.starts_with("--") || name.len() <= 2 || name.contains('='))
    {
        return Err(invalid(
            "runtime_profiles.security.approved_options",
            "every approved option is a long option name such as `--tool-parser-plugin`",
        ));
    }
    let profile = NormalizedProfile {
        engine: raw_profile.engine,
        revision: raw_profile.revision,
        executable: raw_profile.executable.clone(),
        build_fingerprint: raw_profile.build_fingerprint.clone(),
        args: raw_profile.args.clone(),
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
    // SPEC §13.3, ADR 0008: pinned revisions and digests, HTTPS, secret refs.
    source.validate()?;
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
        // ADR 0008: a remote source resolves to its fixed directory in the
        // store; the host materializes it there before the first placement.
        (remote, Some(store)) => remote
            .store_key()
            .map(|key| {
                store
                    .join(key)
                    .to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("host.model_store.path", "must be valid UTF-8"))
            })
            .transpose()?,
        (_, None) => None,
    };
    Ok(ModelIdentity {
        source,
        resolved_path,
        content_fingerprint: raw.content_fingerprint,
        revision: raw.revision,
    })
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
    // The inventory digest is the host's published claim about its own NVIDIA
    // device inventory (sglang_device's `mllm-nvidia-inventory-v1` material). It
    // is versioned evidence the native placement gate asserts against, never a
    // reinterpretation of the opaque hardware fingerprint, so it must be a
    // lowercase hex digest exactly as the collector computes it.
    if let Some(digest) = h.device_inventory_digest.as_ref() {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(invalid(
                "host.device_inventory_digest",
                "must be 64 lowercase hexadecimal characters",
            ));
        }
    }
    // Spec §7: the store anchors every relative model path, so it has to be a
    // place, not a fragment that means something different per working directory.
    let model_store = PathBuf::from(h.model_store.path);
    if !model_store.is_absolute() {
        return Err(invalid("host.model_store.path", "must be absolute"));
    }
    // ADR 0008: remote sources are denied unless this host opts in.
    let model_sources = crate::model_source::ModelSourcePolicy::from_raw(h.model_sources)?;
    if let Some(labels) = &h.resource_policy.labels {
        crate::instances::validate_labels(labels)?;
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
        stream_idle_timeout: None,
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
        stream_idle_ms: raw_queue
            .stream_idle_timeout
            .as_deref()
            .map(parse_duration_ms)
            .transpose()?
            .unwrap_or(DEFAULT_STREAM_IDLE_MS),
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
    // A published physical UUID is placement evidence the launcher sets the
    // child's CUDA namespace from, so it must be the exact shape the inventory
    // collector validates (`runtime/sglang_device.py`), not any opaque token.
    for (name, device) in &h.resource_policy.devices {
        if let Some(uuid) = device.physical_gpu_uuid.as_ref() {
            if !is_physical_gpu_uuid(uuid) {
                return Err(invalid(
                    format!("resource_policy.devices.{name}.physical_gpu_uuid"),
                    "must be a GPU- prefixed lowercase physical UUID",
                ));
            }
        }
    }
    let host = HostPolicy {
        name: h.name,
        hardware_fingerprint: h.hardware_fingerprint,
        environment_fingerprint: h.environment_fingerprint,
        device_inventory_digest: h.device_inventory_digest,
        model_store,
        domains,
        devices: h.resource_policy.devices,
        max_parked,
        observation_ttl_ms,
        device_sharing: h.resource_policy.device_sharing,
        endpoint_port_range: h.resource_policy.endpoint_port_range,
        planner_max_states,
        queue,
        model_sources,
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
    validate_identity_intrinsic(&d.model, &d.recipe, &d.devices, d.request_deadline_ms)?;
    validate_resources_intrinsic(&d.resources, &d.devices)
}

/// The intrinsic rules that do not depend on resource phases. ADR 0014 §5: a
/// deployment may omit `resources:`, and its command identity is then checked
/// without them.
pub(super) fn validate_identity_intrinsic(
    model: &ModelIdentity,
    recipe: &str,
    devices: &[DeviceClaim],
    request_deadline_ms: i64,
) -> Result<(), ConfigError> {
    for (path, value) in [
        (
            "model.content_fingerprint",
            model.content_fingerprint.as_str(),
        ),
        ("model.revision", model.revision.as_str()),
        ("recipe", recipe),
    ] {
        if value.is_empty() {
            return Err(invalid(path, "must not be empty"));
        }
    }
    // The model source itself is checked by `normalize_model`, which is the only
    // way a `ModelIdentity` is built; there is no absolute-path rule left here
    // because a relative local path is legal and resolves against the host store.
    let selected: BTreeMap<_, _> = devices.iter().map(|x| (x.id.as_str(), x.sharing)).collect();
    if selected.len() != devices.len() {
        return Err(invalid("devices", "device IDs must be unique"));
    }
    if request_deadline_ms <= 0 {
        return Err(invalid(
            "request_deadline",
            "deployment deadline may only shorten host limit",
        ));
    }
    Ok(())
}

pub(super) fn validate_resources_intrinsic(
    resources: &RecipeFootprints,
    devices: &[DeviceClaim],
) -> Result<(), ConfigError> {
    domain::validate_recipe(&domain::RecipeFootprints {
        cold: domain_phase(&resources.cold, domain::ResourcePhase::Cold),
        ready: domain_phase(&resources.ready, domain::ResourcePhase::Ready),
        parking: domain_phase(&resources.parking, domain::ResourcePhase::Parking),
        parked: domain_phase(&resources.parked, domain::ResourcePhase::Parked),
        wake: domain_phase(&resources.wake, domain::ResourcePhase::Wake),
    })
    .map_err(|e| invalid("resources", e.to_string()))?;
    let selected: BTreeMap<_, _> = devices.iter().map(|x| (x.id.as_str(), x.sharing)).collect();
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
    Ok(())
}

pub(super) fn recipe_fingerprint(
    d: &NormalizedRecipe,
    profile: &NormalizedProfile,
    engine_config: &LaunchSettings,
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
        engine_config: &'a LaunchSettings,
        env: &'a BTreeMap<String, String>,
        deep_park: DeepPark,
        trust_remote_code: bool,
        extra_args_policy: ExtraArgsPolicy,
        approved_options: &'a [String],
        approved_paths: &'a [String],
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
        engine_config,
        env: &profile.env,
        deep_park: profile.security.deep_park,
        trust_remote_code: profile.security.trust_remote_code,
        extra_args_policy: profile.security.extra_args,
        approved_options: &profile.security.approved_options,
        approved_paths: &profile.security.approved_paths,
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

/// The physical UUID shape `runtime/sglang_device.py` validates
/// (`GPU-` + 8-4-4-4 lowercase hex, 32 hex digits in all). Both sides refuse
/// exactly the same inputs, so a policy UUID the collector would not have
/// observed never reaches the launcher.
pub(super) fn is_physical_gpu_uuid(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("GPU-") else {
        return false;
    };
    rest.len() == 36
        && rest.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
        })
}

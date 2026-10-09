//! SPEC §§4.2,7,13: peer-bound preparation, independently from model readiness.
use crate::ownership::SharedCoordinatorState;
use capyctl_protocol::pb::ReportInventory;
#[derive(Debug, Default, thiserror::Error)]
#[error("host preparation publication refused{}", .reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default())]
pub struct PublicationError {
    /// An operator-safe reason, when the refusal has one the host can act on
    /// (ADR 0019: a hand-written policy whose shape changed).
    pub reason: Option<String>,
}

/// SPEC §4.2: profile eligibility, derived only from evidence the controller
/// accepted, never from a claim. A host is eligible for placement when its
/// inventory carries an approved preparation (the one `publish` accepted) and at
/// least one reported runtime profile resolves in that approved document with
/// the same build fingerprint. A profile the host reports `disabled` or
/// `unsupported` is negative evidence and never counts. ADR 0011: a reported
/// `qualified` grants nothing more than `unknown`; qualification is not an
/// input. Connectivity (a live, reconciled session) is the caller's half, and
/// so is a pending drain (owner decision 4, `Store::host_drain_pending`), which
/// `AgentSessions::eligible_hosts` and the session view apply on top of this.
pub fn eligible(inventory: &ReportInventory) -> bool {
    if inventory.approved_host_config_json.is_empty() {
        return false;
    }
    let Ok(config) =
        capyctl_config::remote_roles::HostConfig::parse(&inventory.approved_host_config_json)
    else {
        return false;
    };
    inventory.profiles.iter().any(|profile| {
        !matches!(profile.eligibility.as_str(), "disabled" | "unsupported")
            && config.profiles.get(&profile.name).is_some_and(|approved| {
                // The same derivation the host uses when it reports the profile.
                approved["build_fingerprint"].as_str().unwrap_or("unknown")
                    == profile.build_fingerprint
            })
    })
}
/// ADR 0019: whether the approved policy `inventory` carries declares a
/// device memory domain. A document without a resolvable policy declares none.
pub fn declares_device_domains(inventory: &ReportInventory) -> bool {
    capyctl_config::remote_roles::HostConfig::parse(&inventory.approved_host_config_json)
        .ok()
        .filter(|config| config.document.get("resource_policy").is_some())
        .and_then(|config| {
            capyctl_config::remote_resources::local_host_document(&config.document).ok()
        })
        .and_then(|local| capyctl_config::effective::normalize_host_policy(&local).ok())
        .is_some_and(|policy| {
            policy
                .domains
                .values()
                .any(|d| d.memory == capyctl_config::effective::DomainMemory::Device)
        })
}
/// Publishes the host's approved preparation and resource policy. Returns the
/// memory headroom warnings for the host's domains (SPEC §7.2, see
/// `Store::memory_headroom_warnings`), already written to the role log.
pub fn publish(
    state: &SharedCoordinatorState,
    host_id: &str,
    inventory: &ReportInventory,
) -> Result<Vec<String>, PublicationError> {
    if inventory.approved_host_config_json.is_empty() {
        // An unprepared connected host grants no executable/profile authority.
        return if inventory.profiles.is_empty() {
            Ok(Vec::new())
        } else {
            Err(PublicationError::default())
        };
    }
    let config =
        capyctl_config::remote_roles::HostConfig::parse(&inventory.approved_host_config_json)
            .map_err(|_| PublicationError::default())?;
    if capyctl_config::remote_resources::policy_fingerprint(&config.document)
        != inventory.policy_fingerprint
    {
        return Err(PublicationError::default());
    }
    let now = capyctl_protocol::now_unix_ms();
    let publication = capyctl_store::host_publication::HostPublication {
        host_id: host_id.into(),
        config_json: config.document.to_string(),
        boot_id: inventory.host_boot_id.clone(),
        fingerprint: inventory.policy_fingerprint.clone(),
        received_at_ms: now,
    };
    let state = state.lock().map_err(|_| PublicationError::default())?;
    let mut warnings = Vec::new();
    if config.document.get("resource_policy").is_some() {
        let local = capyctl_config::remote_resources::local_host_document(&config.document)
            .map_err(|_| PublicationError::default())?;
        let policy = capyctl_config::effective::normalize_host_policy(&local)
            .map_err(|_| PublicationError::default())?;
        let mut observations = Vec::new();
        for (domain, declared) in &policy.domains {
            let mut matches = inventory.domains.iter().filter(|d| &d.domain_id == domain);
            let observation = matches.next().ok_or_else(PublicationError::default)?;
            // ADR 0019: a `device` observation is the GPU of a device domain
            // the approved policy declares, and names that domain's device.
            let device_kind = observation.kind == "device";
            if device_kind
                && (declared.memory != capyctl_config::effective::DomainMemory::Device
                    || declared.device.as_deref() != Some(observation.device_id.as_str()))
            {
                return Err(PublicationError::default());
            }
            if matches.next().is_some()
                || observation.capacity_bytes <= 0
                || observation.available_bytes < 0
                || observation.available_bytes > observation.capacity_bytes
                || observation.observed_at_unix_ms < 0
                || observation.observed_at_unix_ms > now + 500
                || now - observation.observed_at_unix_ms > policy.observation_ttl_ms
            {
                return Err(PublicationError::default());
            }
            observations.push(capyctl_domain::resources::MemoryObservation {
                domain: domain.clone(),
                capacity_bytes: observation.capacity_bytes,
                available_bytes: observation.available_bytes,
                // SPEC §7: freshness is judged on the controller clock. A host
                // clock that leads within the bound above is not a future sample;
                // it is received now, and the ledger never sees a future time.
                sampled_at_ms: observation.observed_at_unix_ms.min(now),
            });
        }
        state
            .store()
            .import_remote_resource_policy(state.session(), host_id, &policy, &observations, now)
            .map_err(|error| match error {
                // ADR 0019: the host is told what differs and what to do.
                shape @ capyctl_store::resource_policy::ResourcePolicyError::ShapeChanged {
                    ..
                } => PublicationError {
                    reason: Some(shape.to_string()),
                },
                _ => PublicationError::default(),
            })?;
        // SPEC §7.2 (found live 2026-10-09): limits that fit the total may
        // still not fit the memory available now. Evidence for the operator
        // only: a warning that cannot be computed never refuses the host.
        let scoped: Vec<_> = observations
            .iter()
            .map(|o| capyctl_domain::resources::MemoryObservation {
                domain: capyctl_config::remote_resources::ledger_key(host_id, "domain", &o.domain),
                ..o.clone()
            })
            .collect();
        warnings = state
            .store()
            .memory_headroom_warnings(host_id, &scoped)
            .unwrap_or_default();
    }
    // Publish executable authority only after all resource observations validate.
    // A rejected policy import must not replace the previously approved snapshot.
    // SPEC §§3.1, 7.3: a host that keeps one journal claim per launch says
    // so; anything else (an older agent) keeps single-claim placement.
    // ADR 0013 §4 (P1): "per_instance" is a per-launch journal that also
    // fences per instance, so two instances of one deployment may share it.
    let claims = match inventory.launch_claims.as_str() {
        "per_launch" => Some(capyctl_store::host_publication::LaunchClaims::PerLaunch),
        "per_instance" => Some(capyctl_store::host_publication::LaunchClaims::PerInstance),
        _ => None,
    };
    state
        .store()
        .publish_host_configuration_with_launch_claims(&publication, claims)
        .map_err(|_| PublicationError::default())?;
    for warning in &warnings {
        capyctl_domain::role_log::notice(
            capyctl_domain::role_log::Level::Warning,
            &format!("Host {host_id}: {warning}"),
        );
    }
    Ok(warnings)
}

/// ADR 0018 §3: a live re-publication, validated like a startup publication
/// (`HostConfig::parse`, fingerprint), with every added profile checked by
/// the rules resolution applies, then stored only if runtime profiles alone
/// changed and every dropped profile's retirement was confirmed. `Err` is
/// the operator-safe reason; the previous approved document stays.
pub fn republish(
    state: &SharedCoordinatorState,
    host_id: &str,
    inventory: &ReportInventory,
    previous: &ReportInventory,
) -> Result<(), String> {
    let config =
        capyctl_config::remote_roles::HostConfig::parse(&inventory.approved_host_config_json)
            .map_err(|e| format!("the document is not a valid host document: {}", e.detail))?;
    if capyctl_config::remote_resources::policy_fingerprint(&config.document)
        != inventory.policy_fingerprint
    {
        return Err("the document does not match its fingerprint".into());
    }
    let old = capyctl_config::remote_roles::HostConfig::parse(&previous.approved_host_config_json)
        .map_err(|_| "the approved document could not be read".to_owned())?;
    for added in capyctl_config::registration::added_profiles(&old.document, &config.document) {
        capyctl_config::registration::check_profile(
            &added,
            &config.document["runtime_profiles"][&added],
        )
        .map_err(|e| format!("profile {added}: {}", e.detail))?;
    }
    let publication = capyctl_store::host_publication::HostPublication {
        host_id: host_id.into(),
        config_json: config.document.to_string(),
        boot_id: inventory.host_boot_id.clone(),
        fingerprint: inventory.policy_fingerprint.clone(),
        received_at_ms: capyctl_protocol::now_unix_ms(),
    };
    let state = state
        .lock()
        .map_err(|_| "the server could not record the publication".to_owned())?;
    state
        .store()
        .republish_host_configuration(&publication, &previous.policy_fingerprint)
        .map_err(|refusal| refusal.reason())
}

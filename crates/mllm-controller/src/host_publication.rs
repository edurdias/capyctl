//! SPEC §§4.2,7,13: peer-bound preparation, independently from model readiness.
use crate::ownership::SharedCoordinatorState;
use mllm_protocol::pb::ReportInventory;
#[derive(Debug, thiserror::Error)]
#[error("host preparation publication refused")]
pub struct PublicationError;

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
        mllm_config::remote_roles::HostConfig::parse(&inventory.approved_host_config_json)
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
pub fn publish(
    state: &SharedCoordinatorState,
    host_id: &str,
    inventory: &ReportInventory,
) -> Result<(), PublicationError> {
    if inventory.approved_host_config_json.is_empty() {
        // An unprepared connected host grants no executable/profile authority.
        return if inventory.profiles.is_empty() {
            Ok(())
        } else {
            Err(PublicationError)
        };
    }
    let config = mllm_config::remote_roles::HostConfig::parse(&inventory.approved_host_config_json)
        .map_err(|_| PublicationError)?;
    if mllm_config::remote_resources::policy_fingerprint(&config.document)
        != inventory.policy_fingerprint
    {
        return Err(PublicationError);
    }
    let now = mllm_protocol::now_unix_ms();
    let publication = mllm_store::host_publication::HostPublication {
        host_id: host_id.into(),
        config_json: config.document.to_string(),
        boot_id: inventory.host_boot_id.clone(),
        fingerprint: inventory.policy_fingerprint.clone(),
        received_at_ms: now,
    };
    let state = state.lock().map_err(|_| PublicationError)?;
    if config.document.get("resource_policy").is_some() {
        let local = mllm_config::remote_resources::local_host_document(&config.document)
            .map_err(|_| PublicationError)?;
        let policy =
            mllm_config::effective::normalize_host_policy(&local).map_err(|_| PublicationError)?;
        let mut observations = Vec::new();
        for domain in policy.domains.keys() {
            let mut matches = inventory.domains.iter().filter(|d| &d.domain_id == domain);
            let observation = matches.next().ok_or(PublicationError)?;
            if matches.next().is_some()
                || observation.capacity_bytes <= 0
                || observation.available_bytes < 0
                || observation.available_bytes > observation.capacity_bytes
                || observation.observed_at_unix_ms < 0
                || observation.observed_at_unix_ms > now + 500
                || now - observation.observed_at_unix_ms > policy.observation_ttl_ms
            {
                return Err(PublicationError);
            }
            observations.push(mllm_domain::resources::MemoryObservation {
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
            .map_err(|_| PublicationError)?;
    }
    // Publish executable authority only after all resource observations validate.
    // A rejected policy import must not replace the previously approved snapshot.
    // SPEC §§3.1, 7.3: a host that keeps one journal claim per launch says
    // so; anything else (an older agent) keeps single-claim placement.
    // ADR 0013 §4 (P1): "per_instance" is a per-launch journal that also
    // fences per instance, so two instances of one deployment may share it.
    let claims = match inventory.launch_claims.as_str() {
        "per_launch" => Some(mllm_store::host_publication::LaunchClaims::PerLaunch),
        "per_instance" => Some(mllm_store::host_publication::LaunchClaims::PerInstance),
        _ => None,
    };
    state
        .store()
        .publish_host_configuration_with_launch_claims(&publication, claims)
        .map_err(|_| PublicationError)?;
    Ok(())
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
    let config = mllm_config::remote_roles::HostConfig::parse(&inventory.approved_host_config_json)
        .map_err(|e| format!("the document is not a valid host document: {}", e.detail))?;
    if mllm_config::remote_resources::policy_fingerprint(&config.document)
        != inventory.policy_fingerprint
    {
        return Err("the document does not match its fingerprint".into());
    }
    let old = mllm_config::remote_roles::HostConfig::parse(&previous.approved_host_config_json)
        .map_err(|_| "the approved document could not be read".to_owned())?;
    for added in mllm_config::registration::added_profiles(&old.document, &config.document) {
        mllm_config::registration::check_profile(
            &added,
            &config.document["runtime_profiles"][&added],
        )
        .map_err(|e| format!("profile {added}: {}", e.detail))?;
    }
    let publication = mllm_store::host_publication::HostPublication {
        host_id: host_id.into(),
        config_json: config.document.to_string(),
        boot_id: inventory.host_boot_id.clone(),
        fingerprint: inventory.policy_fingerprint.clone(),
        received_at_ms: mllm_protocol::now_unix_ms(),
    };
    let state = state
        .lock()
        .map_err(|_| "the server could not record the publication".to_owned())?;
    state
        .store()
        .republish_host_configuration(&publication, &previous.policy_fingerprint)
        .map_err(|refusal| refusal.reason())
}

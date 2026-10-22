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
    // ADR 0033 §§9, 10 (lifecycle design §9): a host that publishes
    // `lmcache: off` or `retention: zero` has every instance there whose
    // launch runs a server stopped with it, as a standalone role start does;
    // a Ready one is owed a start without LMCache (the coordinator's relaunch
    // pass). Its stops are accepted here and driven by the coordinator; one
    // refused now is asked again at the host's next publication.
    if let Some(reason) = lmcache_retirement(&config.document) {
        match state
            .store()
            .retire_lmcache_on_host(state.session(), host_id, reason, now)
        {
            Ok(stops) if !stops.is_empty() => capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Warning,
                &format!(
                    "Host {host_id}: its settings say `{}`; {} instance{} with an LMCache server \
                     stop{} with it",
                    match reason {
                        capyctl_config::lmcache::LmcacheOffReason::HostOff => "lmcache: off",
                        _ => "retention: zero",
                    },
                    stops.len(),
                    if stops.len() == 1 { "" } else { "s" },
                    if stops.len() == 1 { "s" } else { "" },
                ),
            ),
            Ok(_) => {}
            // Evidence for the operator only: the publication stands, and the
            // host's servers are refused re-proof under these settings.
            Err(error) => capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Warning,
                &format!("Host {host_id}: its LMCache servers could not be stopped yet: {error}"),
            ),
        }
    }
    for warning in &warnings {
        capyctl_domain::role_log::notice(
            capyctl_domain::role_log::Level::Warning,
            &format!("Host {host_id}: {warning}"),
        );
    }
    Ok(warnings)
}

/// ADR 0033 §§9, 10: why a host's published settings retire its LMCache
/// servers, `host_off` before `retention_zero` as a standalone role names
/// it; `None` when they allow LMCache.
pub fn lmcache_retirement(
    document: &serde_json::Value,
) -> Option<capyctl_config::lmcache::LmcacheOffReason> {
    use capyctl_config::lmcache::{HostLmcache, LmcacheOffReason, Retention};
    match capyctl_config::lmcache::host_settings(document) {
        (HostLmcache::Off, _) => Some(LmcacheOffReason::HostOff),
        (_, Retention::Zero) => Some(LmcacheOffReason::RetentionZero),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_domain::completion::{
        CompletionEvidence, Milestone, OwnedLaunchReceipt, ProcessIdentity,
    };
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    /// The enrolled hosts' names, which placement names them by.
    const NAMES: [&str; 2] = ["spark-a", "spark-b"];

    /// Two enrolled hosts that declared `lmcache`, each publishing the vLLM
    /// golden host on vLLM 0.32.0 with LMCache 0.5.6 registered, and one
    /// `restart_only` deployment with an instance Ready on each, its launch
    /// running an LMCache server. CPU only: every launch is scripted evidence.
    struct Hosts {
        _dir: tempfile::TempDir,
        state: SharedCoordinatorState,
        sql: rusqlite::Connection,
        hosts: [String; 2],
        deployment: String,
    }

    fn document(settings: Value) -> Value {
        let golden: Value = serde_json::from_str(include_str!(
            "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let mut host = golden["input"]["host"].clone();
        let profile = &mut host["runtime_profiles"]["local"];
        profile["build_fingerprint"] = json!("0.32.0");
        profile["lmcache"] =
            json!({"version": "0.5.6", "digest": format!("sha256:{}", "0".repeat(64))});
        host["resource_policy"]["domains"]["unified"]["host_kv_limit"] = json!("8GiB");
        host["state_dir"] = json!("/home/operator/.local/state/capyctl");
        host["identity_dir"] = json!("/home/operator/.local/state/capyctl/identity");
        host["ingress"] = json!({
            "transport": "trusted_private_link",
            "address": "http://100.64.0.2:9443",
            "bind": "100.64.0.2:9443"
        });
        for (key, value) in settings.as_object().unwrap() {
            host[key] = value.clone();
        }
        host
    }

    fn identity(role: &str, pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            role: role.into(),
            pid,
            boot_id: "boot".into(),
            start_ticks: u64::from(pid) * 10,
        }
    }

    impl Hosts {
        fn new() -> Self {
            let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
            std::fs::set_permissions(
                dir.path(),
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
            )
            .unwrap();
            let state = Arc::new(Mutex::new(
                crate::OwnedCoordinatorState::open(dir.path()).unwrap(),
            ));
            let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
            let hosts = NAMES.map(|name| {
                let owner = state.lock().unwrap();
                let store = owner.store();
                // Hex digests distinct per host (its name's last letter) and kind.
                let digest = |kind: char| format!("{kind}{}", name[6..].repeat(63));
                let invitation = digest('1');
                store
                    .create_host_invitation(&invitation, name, 100, 0)
                    .unwrap();
                let host = store
                    .redeem_host_invitation(
                        &capyctl_store::enrollment::Redemption {
                            invitation_digest: invitation,
                            transaction_id: format!("tx-{name}"),
                            host_name: name.into(),
                            key_digest: digest('2'),
                            csr_digest: "d".repeat(64),
                        },
                        1,
                        |id| {
                            Ok(capyctl_store::enrollment::CertificateRecord {
                                host_id: id.into(),
                                fingerprint: digest('3'),
                                certificate_pem: "certificate".into(),
                                expires_unix: 500,
                            })
                        },
                    )
                    .unwrap()
                    .host_id;
                store
                    .record_host_version(
                        &host,
                        &capyctl_store::host_versions::HostVersion {
                            binary_version: capyctl_protocol::version::BINARY_VERSION.into(),
                            compatibility: "supported".into(),
                            reason: String::new(),
                            capabilities: ["engine_groups", "lmcache", "retention"]
                                .map(String::from)
                                .to_vec(),
                            recorded_at_ms: 1,
                        },
                    )
                    .unwrap();
                host
            });
            let mut lab = Self {
                _dir: dir,
                state,
                sql,
                hosts,
                deployment: String::new(),
            };
            for host in lab.hosts.clone() {
                lab.publish(&host, json!({}));
            }
            lab.deployment = lab.deploy_ready();
            lab
        }

        /// `host` publishes the golden document with `settings` at its root.
        fn publish(&self, host: &str, settings: Value) {
            let document = document(settings);
            let config =
                capyctl_config::remote_roles::HostConfig::parse(&document.to_string()).unwrap();
            let now = capyctl_protocol::now_unix_ms();
            let inventory = ReportInventory {
                domains: vec![capyctl_protocol::pb::DomainObservation {
                    domain_id: "unified".into(),
                    kind: "system".into(),
                    observed_bytes: 100 << 30,
                    observed_at_unix: now / 1000,
                    capacity_bytes: 128 << 30,
                    available_bytes: 100 << 30,
                    observed_at_unix_ms: now,
                    ..Default::default()
                }],
                approved_host_config_json: config.document.to_string(),
                host_boot_id: "boot".into(),
                policy_fingerprint: capyctl_config::remote_resources::policy_fingerprint(
                    &config.document,
                ),
                ..Default::default()
            };
            publish(&self.state, host, &inventory).unwrap();
        }

        /// One instance on each host, each Ready with its LMCache server.
        fn deploy_ready(&self) -> String {
            use capyctl_scheduler::residency::AdmissionContext;
            let owner = self.state.lock().unwrap();
            let store = owner.store();
            let now = capyctl_protocol::now_unix_ms();
            let targets: Vec<_> = self
                .hosts
                .iter()
                .zip(NAMES)
                .map(|(host, name)| {
                    let trusted = capyctl_config::remote_resources::scope_host_document(
                        host,
                        &document(json!({})),
                    )
                    .unwrap();
                    let policy = store.resource_policy(host).unwrap().unwrap();
                    capyctl_store::managed_configuration::HostTarget {
                        host_id: host.clone(),
                        host_name: name.into(),
                        trusted_host: capyctl_config::effective::compose_current_resource_controls(
                            &trusted,
                            &policy.context,
                            &policy.controls,
                        )
                        .unwrap(),
                        scoped: true,
                    }
                })
                .collect();
            let golden: Value = serde_json::from_str(include_str!(
                "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
            ))
            .unwrap();
            let mut config = golden["input"]["deployment"].clone();
            config.as_object_mut().unwrap().remove("host");
            config["devices"] = json!([{ "sharing": "shared" }]);
            for phase in ["cold", "ready", "parking", "wake"] {
                config["resources"][phase]["devices"] = json!([{ "sharing": "shared" }]);
            }
            config["placement"] = json!({ "hosts": NAMES });
            config["instances"] = json!(2);
            config["residency"] = json!("restart_only");
            config["lmcache_pool"] = json!("1GiB");
            let receipt = store
                .create_managed_configuration_on_hosts(
                    owner.session(),
                    "owner",
                    "deploy",
                    &json!({ "config": config }).to_string(),
                    &targets,
                    &[],
                    now,
                )
                .unwrap();
            let id = receipt.deployment_id;
            store
                .accept_scoped_start_command(
                    owner.session(),
                    "owner",
                    &id,
                    capyctl_store::ordinary_lifecycle::placement::StartScope::All,
                    receipt.revision,
                    "start",
                    now,
                    now + 120_000,
                    None,
                )
                .unwrap();
            let planned: Vec<(String, String)> = self
                .sql
                .prepare(
                    "SELECT s.id,i.host_id FROM lifecycle_steps s
                       JOIN runtime_bindings b ON b.id=s.binding_id
                       JOIN deployment_instances i ON i.deployment_id=b.deployment_id
                        AND i.instance_index=b.instance_index
                      WHERE s.deployment_id=?1 AND s.state='planned' ORDER BY i.host_id",
                )
                .unwrap()
                .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(
                planned.iter().map(|(_, h)| h.clone()).collect::<Vec<_>>(),
                {
                    let mut hosts = self.hosts.to_vec();
                    hosts.sort();
                    hosts
                }
            );
            for (n, (step, host)) in planned.into_iter().enumerate() {
                let policy = store.resource_policy(&host).unwrap().unwrap();
                let limits: Vec<_> = policy
                    .controls
                    .domains
                    .iter()
                    .map(|(domain, d)| capyctl_domain::resources::MemoryLimit {
                        domain: domain.clone(),
                        managed_bytes: d.managed_limit,
                        free_reserve_bytes: d.free_reserve,
                        reserve_absorbs_unmanaged: false,
                        host_kv_bytes: d.host_kv_limit,
                        parked_bytes: d.parked_limit,
                    })
                    .collect();
                let observations = vec![capyctl_domain::resources::MemoryObservation {
                    domain: capyctl_config::remote_resources::ledger_key(
                        &host, "domain", "unified",
                    ),
                    capacity_bytes: 128 << 30,
                    available_bytes: 100 << 30,
                    sampled_at_ms: now,
                }];
                let ttl = policy.controls.observation_ttl_ms;
                let (_, context) = store
                    .arm_initialize_with_context(
                        owner.session(),
                        &step,
                        AdmissionContext::new(
                            &observations,
                            &limits,
                            now,
                            ttl,
                            policy.controls.max_parked as usize,
                        ),
                    )
                    .unwrap();
                let context = context.unwrap();
                let grant = store.launch_lmcache_grant(&context.binding_id).unwrap();
                assert!(grant.is_some(), "LMCache is planned on {host}");
                let base = 100 + 10 * n as u32;
                let server = identity("lmcache", base + 5);
                let fence = capyctl_store::lifecycle::DeploymentFence {
                    deployment_id: context.token.deployment_id.clone(),
                    revision: context.token.revision,
                    generation: context.token.generation,
                };
                store
                    .record_launch_identity(owner.session(), &fence, &context.binding_id, &server)
                    .unwrap();
                store
                    .record_launch_lmcache(
                        owner.session(),
                        &context.binding_id,
                        &capyctl_store::ordinary_lifecycle::lmcache::LmcacheOutcome::On,
                        now,
                    )
                    .unwrap();
                let group = vec![
                    identity("api", base),
                    identity("worker-0", base + 1),
                    server,
                ];
                store
                    .record_owned_launch(
                        owner.session(),
                        &step,
                        &OwnedLaunchReceipt {
                            binding_id: context.binding_id.clone(),
                            incarnation: context.incarnation.clone(),
                            identities: group.clone(),
                            observed_at_ms: now,
                            receipt: "scripted host native model probe".into(),
                        },
                        now,
                    )
                    .unwrap();
                store
                    .complete_step(
                        owner.session(),
                        &step,
                        &CompletionEvidence {
                            token: context.token,
                            identities: group,
                            observed_at_ms: now,
                            control_receipt: Some("scripted host native model probe".into()),
                            milestones: vec![
                                Milestone::AllocationsRestored,
                                Milestone::WeightsUsable,
                                Milestone::CacheValid,
                                Milestone::ModelUsable,
                            ],
                        },
                        now,
                        ttl,
                    )
                    .unwrap();
            }
            id
        }

        /// The stops a retirement accepted for the instance on `host`.
        fn retirement_stops(&self, host: &str) -> i64 {
            self.sql
                .query_row(
                    "SELECT COUNT(DISTINCT o.id) FROM operations o
                       JOIN command_receipts r ON r.operation_id=o.id
                       JOIN lifecycle_steps s ON s.operation_id=o.id
                       JOIN runtime_bindings b ON b.id=s.binding_id
                       JOIN deployment_instances i ON i.deployment_id=b.deployment_id
                        AND i.instance_index=b.instance_index
                      WHERE o.deployment_id=?1 AND i.host_id=?2 AND o.kind='ordinary_cleanup'
                        AND r.principal_id=?3",
                    rusqlite::params![
                        self.deployment,
                        host,
                        capyctl_store::ordinary_lifecycle::lmcache::RETIRE_PRINCIPAL
                    ],
                    |r| r.get(0),
                )
                .unwrap()
        }

        /// The reasons the relaunch pass owes the instance on `host` a start
        /// without LMCache.
        fn owed(&self, host: &str) -> Vec<String> {
            self.sql
                .prepare(
                    "SELECT m.reason FROM lmcache_relaunches m
                       JOIN deployment_instances i ON i.deployment_id=m.deployment_id
                        AND i.instance_index=m.instance_index
                      WHERE m.deployment_id=?1 AND i.host_id=?2",
                )
                .unwrap()
                .query_map([&self.deployment, &host.to_owned()], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        }

        /// The `lmcache_off` reasons journaled for the launch on `host`.
        fn off_events(&self, host: &str) -> Vec<String> {
            self.sql
                .prepare(
                    "SELECT json_extract(e.payload_json,'$.reason') FROM management_events e
                       JOIN launch_lmcache l ON l.binding_id=json_extract(e.payload_json,'$.binding_id')
                      WHERE e.kind='lmcache_off' AND e.deployment_id=?1 AND l.host_id=?2
                      ORDER BY e.sequence",
                )
                .unwrap()
                .query_map([&self.deployment, &host.to_owned()], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        }

        fn dispatch_open(&self, host: &str) -> bool {
            self.sql
                .query_row(
                    "SELECT dispatch_enabled=1 FROM deployment_instances
                      WHERE deployment_id=?1 AND host_id=?2",
                    [&self.deployment, &host.to_owned()],
                    |r| r.get(0),
                )
                .unwrap()
        }

        /// The step of the stop accepted for the instance on `host`.
        fn stop_step(&self, host: &str) -> String {
            self.sql
                .query_row(
                    "SELECT s.id FROM lifecycle_steps s
                       JOIN operations o ON o.id=s.operation_id AND o.kind='ordinary_cleanup'
                       JOIN runtime_bindings b ON b.id=s.binding_id
                       JOIN deployment_instances i ON i.deployment_id=b.deployment_id
                        AND i.instance_index=b.instance_index
                      WHERE s.deployment_id=?1 AND i.host_id=?2",
                    [&self.deployment, &host.to_owned()],
                    |r| r.get(0),
                )
                .unwrap()
        }
    }

    /// ADR 0033 §§9, 10 (lifecycle design §9): a host publishing
    /// `lmcache: off` has the Ready instance whose launch runs a server there
    /// stopped by the retirement principal and owed a start without LMCache
    /// (`host_off`, journaled once); the instance on the other host is
    /// untouched. Publishing the same settings again stops nothing more. A
    /// document that allows LMCache retires nothing.
    // T45 T10 T34
    #[test]
    fn publishing_lmcache_off_retires_that_hosts_servers_only() {
        let lab = Hosts::new();
        let [a, b] = lab.hosts.clone();
        lab.publish(&b, json!({"lmcache": "auto", "retention": "standard"}));
        for host in [&a, &b] {
            assert_eq!(lab.retirement_stops(host), 0);
            assert!(lab.owed(host).is_empty());
            assert!(lab.off_events(host).is_empty());
        }

        lab.publish(&a, json!({"lmcache": "off"}));
        assert_eq!(lab.retirement_stops(&a), 1);
        assert_eq!(lab.owed(&a), ["host_off"]);
        assert_eq!(lab.off_events(&a), ["host_off"]);
        assert!(
            !lab.dispatch_open(&a),
            "a stopping instance takes no traffic"
        );
        assert_eq!(lab.retirement_stops(&b), 0);
        assert!(lab.owed(&b).is_empty());
        assert!(lab.off_events(&b).is_empty());
        assert!(lab.dispatch_open(&b));

        // The host's next publication asks again: nothing more is stopped.
        lab.publish(&a, json!({"lmcache": "off"}));
        assert_eq!(lab.retirement_stops(&a), 1);
        assert_eq!(lab.owed(&a), ["host_off"]);
        assert_eq!(lab.off_events(&a), ["host_off"]);
        // Nothing is relaunched before the stop completed.
        let owner = lab.state.lock().unwrap();
        let store = owner.store();
        let now = capyctl_protocol::now_unix_ms();
        assert!(store
            .relaunch_retired_lmcache(owner.session(), now, None)
            .unwrap()
            .is_empty());
        // The stop completes on the host's gone evidence; the relaunch pass
        // then starts the instance again there, planned without LMCache for
        // the host's reason, and later publications retire nothing more.
        let step = lab.stop_step(&a);
        let (_, cleanup) = store
            .arm_ordinary_cleanup_with_context(owner.session(), &step, now)
            .unwrap();
        let cleanup = cleanup.unwrap();
        let ttl = store
            .resource_policy(&a)
            .unwrap()
            .unwrap()
            .controls
            .observation_ttl_ms;
        store
            .complete_cleanup(
                owner.session(),
                &step,
                &capyctl_domain::completion::CleanupEvidence {
                    binding_id: cleanup.binding_id,
                    incarnation: cleanup.incarnation,
                    identities: cleanup.identities,
                    observed_at_ms: now,
                    receipt: "scripted host observed the owned group gone".into(),
                },
                now,
                ttl,
            )
            .unwrap();
        let starts = store
            .relaunch_retired_lmcache(owner.session(), now, None)
            .unwrap();
        assert_eq!(starts.len(), 1);
        let plan = store
            .launch_lmcache(&starts[0].binding_id)
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                &plan.planned,
                capyctl_store::ordinary_lifecycle::lmcache::PlannedLmcache::Off(off)
                    if off.reason == capyctl_config::lmcache::LmcacheOffReason::HostOff
            ),
            "{plan:?}"
        );
        let relaunched_on: String = lab
            .sql
            .query_row(
                "SELECT host_id FROM launch_lmcache WHERE binding_id=?1",
                [&starts[0].binding_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(relaunched_on, a);
        drop(owner);
        lab.publish(&a, json!({"lmcache": "off"}));
        assert_eq!(lab.retirement_stops(&a), 1);
    }

    /// ADR 0033 §§9, 10: `retention: zero` retires a host's servers as
    /// `lmcache: off` does, under its own reason; a document stating both
    /// names `host_off`, as a standalone role does.
    // T45 T10
    #[test]
    fn retention_zero_retires_too_and_host_off_names_both() {
        let lab = Hosts::new();
        let [a, b] = lab.hosts.clone();
        lab.publish(&a, json!({"retention": "zero"}));
        lab.publish(&b, json!({"lmcache": "off", "retention": "zero"}));
        assert_eq!(lab.retirement_stops(&a), 1);
        assert_eq!(lab.owed(&a), ["retention_zero"]);
        assert_eq!(lab.off_events(&a), ["retention_zero"]);
        assert_eq!(lab.retirement_stops(&b), 1);
        assert_eq!(lab.owed(&b), ["host_off"]);
        assert_eq!(lab.off_events(&b), ["host_off"]);
    }

    /// ADR 0033 §§6, 12 (lifecycle design §7): what a host's load report
    /// measured of its own launch's LMCache server is recorded for the
    /// revision on that host, max-merged; a sample naming another host's
    /// launch records nothing and never keeps that host's own report out.
    // T45 T14
    #[test]
    fn a_hosts_lmcache_samples_measure_its_own_launch_only() {
        const GIB: u64 = 1 << 30;
        const MIB: u64 = 1 << 20;
        let lab = Hosts::new();
        let [a, b] = lab.hosts.clone();
        let launch = |host: &str| -> (String, i64) {
            lab.sql
                .query_row(
                    "SELECT s.id,l.generation FROM launch_lmcache l
                       JOIN lifecycle_steps s ON s.binding_id=l.binding_id
                       JOIN operations o ON o.id=s.operation_id AND o.kind='initialize'
                      WHERE l.deployment_id=?1 AND l.host_id=?2",
                    [&lab.deployment, &host.to_owned()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        };
        let measured = || -> Vec<(String, i64, i64)> {
            lab.sql
                .prepare(
                    "SELECT host_id,overhead_bytes,device_bytes FROM lmcache_measurements
                      WHERE deployment_id=?1 ORDER BY host_id",
                )
                .unwrap()
                .query_map([&lab.deployment], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let sample = |(step, generation): &(String, i64), rss: u64, device: Option<u64>| {
            capyctl_protocol::reports::LmcacheSample {
                deployment_id: lab.deployment.clone(),
                generation: *generation,
                owned_handle: step.clone(),
                sampled_at_ms: capyctl_protocol::now_unix_ms(),
                scrape_ok: true,
                used_bytes: None,
                usage_ppm: None,
                inflight_bytes: None,
                alloc_failures: None,
                lookups: None,
                hits: None,
                l2_bytes: None,
                rss_bytes: Some(rss),
                device_bytes: device,
            }
        };
        let (on_a, on_b) = (launch(&a), launch(&b));
        let authority = crate::enrollment::EnrollmentAuthority::new(
            lab.state.clone(),
            capyctl_agent::identity::CertificateAuthority::generate(100).unwrap(),
        );
        authority.record_lmcache_samples(&a, &[sample(&on_b, GIB + 900 * MIB, Some(400 * MIB))]);
        assert!(measured().is_empty());
        authority.record_lmcache_samples(&b, &[sample(&on_b, GIB + 600 * MIB, Some(300 * MIB))]);
        let mi = |bytes: u64| bytes as i64;
        assert_eq!(measured(), [(b.clone(), mi(600 * MIB), mi(300 * MIB))]);
        // Smaller does not lower it; a larger figure grows on its own.
        authority.record_lmcache_samples(&b, &[sample(&on_b, GIB + 500 * MIB, Some(200 * MIB))]);
        authority.record_lmcache_samples(&b, &[sample(&on_b, GIB + 800 * MIB, None)]);
        authority.record_lmcache_samples(&a, &[sample(&on_a, GIB + 100 * MIB, None)]);
        let mut expected = vec![
            (a.clone(), mi(100 * MIB), 0),
            (b.clone(), mi(800 * MIB), mi(300 * MIB)),
        ];
        expected.sort();
        assert_eq!(measured(), expected);
    }
}

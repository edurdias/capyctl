//! Stopped-only configuration acceptance. No qualification or runtime authority.
use crate::dispatch::{check_session, CoordinatorSession};
use crate::events::{append_event, EventMetadata, EventOperationId};
use crate::resource_policy::read_selected_policy;
use capyctl_config::effective::{deployment_command_fingerprint, resolve_effective};
use capyctl_config::resource_controls::{ResourceContext, ResourceControls};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue, Value};
use sha2::{Digest, Sha256};

const MAX_BYTES: usize = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum ManagedConfigurationError {
    #[error("invalid stopped configuration command")]
    Invalid,
    /// SPEC §15.3: the configuration itself was refused, with the same named
    /// reason `validate config` reports (code, path and detail; details name
    /// options, never values).
    #[error("invalid configuration: {0}")]
    Rejected(capyctl_config::ConfigError),
    #[error("stale coordinator session")]
    StaleSession,
    #[error("idempotency conflict")]
    IdempotencyConflict,
    #[error("configuration revision conflict")]
    RevisionConflict,
    #[error("route or deployment name conflict")]
    RouteConflict,
    #[error("runtime retained")]
    RuntimeRetained,
    #[error("current resource policy required")]
    PolicyConflict,
    #[error("corrupt stored configuration")]
    CorruptStoredData,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}
type Result<T> = std::result::Result<T, ManagedConfigurationError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedConfigurationReceipt {
    pub version: u8,
    pub operation_id: String,
    pub deployment_id: String,
    pub revision: i64,
    pub generation: i64,
    pub resource_policy_revision: i64,
    pub accepted_at_ms: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReceiptV2 {
    version: u8,
    receipt: ManagedConfigurationReceipt,
    command_fingerprint: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateCommand<'a> {
    #[serde(borrow)]
    config: &'a RawValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceCommand<'a> {
    #[serde(borrow)]
    config: &'a RawValue,
    expected_revision: i64,
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

/// ADR 0013 §3: one allowed host a deployment is resolved against at deploy
/// time. The trusted host document carries the host's current persisted
/// resource controls; `scoped` says the deployment's device and domain names
/// must be scoped to this host's ledger keys (an enrolled remote host) before
/// it is resolved, as a single-host deploy always did.
#[derive(Debug, Clone)]
pub struct HostTarget {
    pub host_id: String,
    /// The name an allowed host set may use for it (an enrolled host's name;
    /// the embedded host's name is its id).
    pub host_name: String,
    pub trusted_host: Value,
    pub scoped: bool,
}

/// An allowed host that refused the deployment, with its closed diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRefusal {
    pub host_id: String,
    pub diagnostic: String,
}

/// One allowed host's resolution of the revision.
struct Resolved {
    host_id: String,
    host_name: String,
    /// The command as this host sees it (scoped to its ledger keys): what the
    /// command's identity is computed over.
    command: Value,
    /// The document the revision was resolved from on this host: devices
    /// assigned, deployment-level fields (count, placement) removed. A launch
    /// on the host is rendered from it.
    source: Value,
    effective: capyctl_config::effective::EffectiveDeployment,
    provisional: bool,
    /// ADR 0019 (discrete GPU design §7): on a multi-GPU host, the revision
    /// resolved once per GPU (device, source, resolution) when the deployment
    /// pins no device; `source` and `effective` are the first of them.
    devices: Vec<(
        String,
        Value,
        capyctl_config::effective::EffectiveDeployment,
    )>,
}

fn scoped_source(target: &HostTarget, config: &Value) -> Result<Value> {
    if target.scoped {
        capyctl_config::remote_resources::scope_deployment_document(&target.host_id, config)
            .map_err(|_| ManagedConfigurationError::Invalid)
    } else {
        Ok(config.clone())
    }
}

/// The closed category a refused resolution is reported with (never the raw
/// configuration or error text).
fn refusal_diagnostic(error: &ManagedConfigurationError) -> &'static str {
    match error {
        ManagedConfigurationError::PolicyConflict => "resource_policy_unavailable",
        _ => "does_not_resolve",
    }
}

/// ADR 0028 §2, §3: a group's named hosts in rank order, the head first, each
/// one an allowed target the server could resolve against and each declaring
/// the peer address its peers reach it on (read from the trusted host
/// document's groups block, never from the normalized policy). A named host
/// without a target cannot resolve, so the group is refused.
fn group_members<'t>(
    shape: &capyctl_config::topology::GroupShape,
    targets: &'t [HostTarget],
) -> Result<Vec<&'t HostTarget>> {
    let mut members: Vec<&HostTarget> = Vec::with_capacity(shape.hosts.len());
    for name in &shape.hosts {
        let target = targets
            .iter()
            .find(|t| t.host_id == *name || t.host_name == *name)
            .ok_or(ManagedConfigurationError::Invalid)?;
        if members.iter().any(|m| m.host_id == target.host_id) {
            return Err(ManagedConfigurationError::Invalid);
        }
        capyctl_config::topology::check_member_peer_address(name, &target.trusted_host)
            .map_err(ManagedConfigurationError::Rejected)?;
        members.push(target);
    }
    Ok(members)
}

/// ADR 0028 §2, spec §16: why one named host of a group did not resolve
/// (`capyctl_config::topology::member_resolution_error`).
fn group_member_error(host: &str, error: ManagedConfigurationError) -> ManagedConfigurationError {
    match error {
        ManagedConfigurationError::Rejected(error) => ManagedConfigurationError::Rejected(
            capyctl_config::topology::member_resolution_error(host, error),
        ),
        other => other,
    }
}

/// ADR 0028 §2: one build across the members and every member's resolution
/// admitted by the capability gate
/// (`capyctl_config::topology::check_group_members`). `resolved` is in rank
/// order, the head first.
fn check_group_members(resolved: &[Resolved]) -> Result<()> {
    if resolved.is_empty() {
        return Err(ManagedConfigurationError::Invalid);
    }
    let members: Vec<(&str, &capyctl_config::effective::EffectiveDeployment)> = resolved
        .iter()
        .map(|member| (member.host_id.as_str(), &member.effective))
        .collect();
    capyctl_config::topology::check_group_members(&members)
        .map_err(ManagedConfigurationError::Rejected)
}

impl crate::Store {
    /// Service-only trusted host configuration must contain the current persisted
    /// resource controls, not stale startup values. This never imports policy,
    /// validates qualification authority, reserves a runtime, or enables dispatch.
    #[allow(clippy::too_many_arguments)]
    pub fn create_stopped_managed_configuration(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        key: &str,
        request_json: &str,
        trusted_host: &Value,
        now_ms: i64,
    ) -> Result<ManagedConfigurationReceipt> {
        self.accept_stopped_configuration(
            session,
            principal,
            key,
            None,
            request_json,
            &[embedded_target(trusted_host)?],
            &[],
            now_ms,
        )
    }

    /// ADR 0013 §3: create a deployment resolved against every allowed host.
    /// The deploy succeeds when at least one host resolves; each host that
    /// refuses is recorded with its diagnostic and is never a candidate. No
    /// host is chosen and nothing is reserved.
    #[allow(clippy::too_many_arguments)]
    pub fn create_managed_configuration_on_hosts(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        key: &str,
        request_json: &str,
        targets: &[HostTarget],
        refused: &[HostRefusal],
        now_ms: i64,
    ) -> Result<ManagedConfigurationReceipt> {
        self.accept_stopped_configuration(
            session,
            principal,
            key,
            None,
            request_json,
            targets,
            refused,
            now_ms,
        )
    }

    /// ADR 0013 §3, §7: a revision of an existing deployment resolved against
    /// every allowed host. A count-only revision leaves running instances
    /// untouched; any other revision of a running deployment stops every
    /// instance and restarts it on the new revision (owner decision Q8).
    #[allow(clippy::too_many_arguments)]
    pub fn replace_managed_configuration_on_hosts(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        key: &str,
        deployment_id: &str,
        request_json: &str,
        targets: &[HostTarget],
        refused: &[HostRefusal],
        now_ms: i64,
    ) -> Result<ManagedConfigurationReceipt> {
        if ulid::Ulid::from_string(deployment_id).is_err() {
            return Err(ManagedConfigurationError::Invalid);
        }
        self.accept_stopped_configuration(
            session,
            principal,
            key,
            Some(deployment_id),
            request_json,
            targets,
            refused,
            now_ms,
        )
    }

    /// Revision-aware replacement after all owned effects have been cleaned up.
    /// Historical revisions/receipts remain immutable; this is never a hot update.
    #[allow(clippy::too_many_arguments)]
    pub fn replace_stopped_managed_configuration(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        key: &str,
        deployment_id: &str,
        request_json: &str,
        trusted_host: &Value,
        now_ms: i64,
    ) -> Result<ManagedConfigurationReceipt> {
        if ulid::Ulid::from_string(deployment_id).is_err() {
            return Err(ManagedConfigurationError::Invalid);
        }
        self.accept_stopped_configuration(
            session,
            principal,
            key,
            Some(deployment_id),
            request_json,
            &[embedded_target(trusted_host)?],
            &[],
            now_ms,
        )
    }

    /// Whether `key` already names an accepted configuration command of
    /// `principal` (a create when `target` is `None`, else a replacement of
    /// `target`). Such a retry is answered from its receipt (or refused as an
    /// idempotency conflict) by the store, never re-checked against what hosts
    /// publish now (ADR 0018 §7).
    pub fn has_configuration_receipt(
        &self,
        principal: &str,
        key: &str,
        target: Option<&str>,
    ) -> Result<bool> {
        let scope = configuration_scope(target);
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3)",
            params![principal, scope, key],
            |r| r.get(0),
        )?)
    }

    #[allow(clippy::too_many_arguments)]
    fn accept_stopped_configuration(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        key: &str,
        target: Option<&str>,
        request_json: &str,
        targets: &[HostTarget],
        refused: &[HostRefusal],
        now_ms: i64,
    ) -> Result<ManagedConfigurationReceipt> {
        if targets.is_empty() && refused.is_empty() {
            return Err(ManagedConfigurationError::Invalid);
        }
        if request_json.len() > MAX_BYTES
            || !valid_identifier(principal)
            || !valid_identifier(key)
            || now_ms < 0
        {
            return Err(ManagedConfigurationError::Invalid);
        }
        let (raw_config, expected_revision) = if target.is_some() {
            let command: ReplaceCommand<'_> = serde_json::from_str(request_json)
                .map_err(|_| ManagedConfigurationError::Invalid)?;
            if command.expected_revision < 1 {
                return Err(ManagedConfigurationError::Invalid);
            }
            (command.config, Some(command.expected_revision))
        } else {
            let command: CreateCommand<'_> = serde_json::from_str(request_json)
                .map_err(|_| ManagedConfigurationError::Invalid)?;
            (command.config, None)
        };
        let config =
            capyctl_config::parse_strict(capyctl_config::ConfigKind::Deployment, raw_config.get())
                .map_err(ManagedConfigurationError::Rejected)?;
        let scope = configuration_scope(target);
        let kind = if target.is_some() {
            "managed_configuration_replace"
        } else {
            "managed_configuration_create"
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session).map_err(|_| ManagedConfigurationError::StaleSession)?;
        if let Some(receipt) = replay(
            &tx,
            principal,
            &scope,
            key,
            kind,
            target,
            expected_revision,
            &config,
            targets,
        )? {
            return Ok(receipt);
        }
        // ADR 0013 §2: the deployment's instance count and placement constraints.
        let instance_spec = capyctl_config::instances::parse_instance_spec(&config)
            .map_err(ManagedConfigurationError::Rejected)?;
        // ADR 0013 §3: resolve against every allowed host, in host order so the
        // canonical (first resolving) host is deterministic.
        // A host outside the allowed set is not a candidate at all (ADR 0013
        // §2: `placement.hosts` or the `host` shorthand, by id or by name).
        let group = instance_spec.group.as_ref();
        let ordered: Vec<&HostTarget> = match group {
            None => {
                let mut ordered: Vec<&HostTarget> = targets
                    .iter()
                    .filter(|t| {
                        instance_spec.placement.allows(&t.host_id)
                            || instance_spec.placement.allows(&t.host_name)
                    })
                    .collect();
                ordered.sort_by(|a, b| a.host_id.cmp(&b.host_id));
                ordered
            }
            Some(shape) => group_members(shape, targets)?,
        };
        let mut refusals = refused.to_vec();
        let mut resolved: Vec<Resolved> = Vec::new();
        let mut single_error = None;
        for host in &ordered {
            let attempt = (|| {
                let mut command = scoped_source(host, &config)?;
                // ADR 0019: `devices: [{id: gpu1}]` takes this host's sharing.
                capyctl_config::deployment_defaults::fill_device_sharing(
                    &mut command,
                    &host.trusted_host,
                );
                // ADR 0019 (discrete GPU design §7): a deployment that pins
                // no device is resolved once per GPU of a discrete host, and
                // placement picks the GPU. The host's own resolution is the
                // lowest-index GPU's.
                let choices =
                    capyctl_config::instances::device_choices(&command, &host.trusted_host)
                        .map_err(ManagedConfigurationError::Rejected)?;
                let recipe = |document: &Value| -> Result<Value> {
                    // ADR 0013 §2: unnamed device claims take this host's
                    // devices, and the per-host recipe carries no
                    // deployment-level field.
                    let mut source =
                        capyctl_config::instances::assign_devices(document, &host.trusted_host)
                            .map_err(ManagedConfigurationError::Rejected)?;
                    if let Some(object) = source.as_object_mut() {
                        for field in ["instances", "placement", "host"] {
                            // ADR 0028 §2: a group member's recipe keeps
                            // its rank-ordered host list, so it resolves as
                            // the group it is; placement is outside the
                            // recipe fingerprint either way.
                            if field == "placement" && group.is_some() {
                                continue;
                            }
                            object.remove(field);
                        }
                    }
                    Ok(source)
                };
                let mut devices = Vec::new();
                let (source, effective, provisional) = if choices.len() > 1 {
                    // Final review I7 (design §7): each GPU is judged on its
                    // own. A GPU the deployment cannot fit is not a placement
                    // option for it; the host is refused only when no GPU
                    // resolves, and the host's own row is the first GPU that
                    // does.
                    let mut first_error = None;
                    let mut resolved_on = Vec::new();
                    for (device, choice) in &choices {
                        let attempt = recipe(choice).and_then(|source| {
                            resolve_for_acceptance(&source, &host.trusted_host)
                                .map(|resolution| (source, resolution))
                        });
                        match attempt {
                            Ok((source, (mut on_device, device_provisional))) => {
                                on_device.routes.sort();
                                resolved_on.push((
                                    device.clone(),
                                    source,
                                    on_device,
                                    device_provisional,
                                ));
                            }
                            Err(ManagedConfigurationError::Sql(error)) => {
                                return Err(ManagedConfigurationError::Sql(error))
                            }
                            Err(error) => {
                                first_error.get_or_insert(error);
                            }
                        }
                    }
                    let Some((_, source, effective, provisional)) = resolved_on.first().cloned()
                    else {
                        return Err(first_error.unwrap_or(ManagedConfigurationError::Invalid));
                    };
                    // One checkpoint, one set of facts: every GPU's
                    // resolution is provisional exactly when the host's is.
                    if resolved_on.iter().any(|(.., p)| *p != provisional) {
                        return Err(ManagedConfigurationError::Invalid);
                    }
                    devices = resolved_on
                        .into_iter()
                        .map(|(device, source, on_device, _)| (device, source, on_device))
                        .collect();
                    (source, effective, provisional)
                } else {
                    let source = recipe(choices.first().map_or(&command, |(_, first)| first))?;
                    let (mut effective, provisional) =
                        resolve_for_acceptance(&source, &host.trusted_host)?;
                    effective.routes.sort();
                    (source, effective, provisional)
                };
                let policy = read_selected_policy(&tx, &effective.host.name)
                    .map_err(|_| ManagedConfigurationError::PolicyConflict)?
                    .ok_or(ManagedConfigurationError::PolicyConflict)?;
                if policy.context != ResourceContext::from_host(&effective.host)
                    || policy.controls != ResourceControls::from_host(&effective.host)
                {
                    return Err(ManagedConfigurationError::PolicyConflict);
                }
                Ok((command, source, effective, provisional, devices))
            })();
            match attempt {
                Ok((command, source, effective, provisional, devices)) => resolved.push(Resolved {
                    host_id: host.host_id.clone(),
                    host_name: host.host_name.clone(),
                    command,
                    source,
                    effective,
                    provisional,
                    devices,
                }),
                Err(ManagedConfigurationError::Sql(error)) => {
                    return Err(ManagedConfigurationError::Sql(error))
                }
                // ADR 0028 §2: every named host of a group must resolve; one
                // that does not refuses the whole deploy with its reason.
                Err(error) if group.is_some() => {
                    return Err(group_member_error(&host.host_id, error))
                }
                Err(error) => {
                    refusals.push(HostRefusal {
                        host_id: host.host_id.clone(),
                        diagnostic: refusal_diagnostic(&error).into(),
                    });
                    single_error.get_or_insert(error);
                }
            }
        }
        if group.is_some() {
            check_group_members(&resolved)?;
        }
        let Some(canonical) = resolved.first() else {
            // A single allowed host keeps the answer it always gave; with
            // several, the first host's configuration reason is still named
            // (SPEC §15.3), never a bare refusal.
            return Err(match single_error {
                Some(error) if ordered.len() == 1 => error,
                Some(error @ ManagedConfigurationError::Rejected(_)) => error,
                _ => ManagedConfigurationError::Invalid,
            });
        };
        let effective = canonical.effective.clone();
        let provisional = canonical.provisional;
        let source = canonical.source.clone();
        let command = canonical.command.clone();
        let effective_json =
            serde_json::to_string(&effective).map_err(|_| ManagedConfigurationError::Invalid)?;
        if effective_json.len() > MAX_BYTES {
            return Err(ManagedConfigurationError::Invalid);
        }
        let command_fingerprint =
            deployment_command_fingerprint(&command, effective.request_deadline_ms)
                .map_err(|_| ManagedConfigurationError::Invalid)?;
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&json!({"version":2,"scope":scope,"expected_revision":expected_revision,"effective":effective,"command_fingerprint":command_fingerprint})).map_err(|_| ManagedConfigurationError::Invalid)?));
        let policy = read_selected_policy(&tx, &effective.host.name)
            .map_err(|_| ManagedConfigurationError::PolicyConflict)?
            .ok_or(ManagedConfigurationError::PolicyConflict)?;
        let (revision, generation, running) = if let Some(id) = target {
            replacement_fence(
                &tx,
                id,
                expected_revision.ok_or(ManagedConfigurationError::Invalid)?,
            )?
        } else {
            (1, 1, false)
        };
        ensure_routes(
            &tx,
            &effective.name,
            &effective.routes,
            target.unwrap_or(""),
        )?;
        let deployment = match target {
            Some(id) => {
                ulid::Ulid::from_string(id).map_err(|_| ManagedConfigurationError::Invalid)?
            }
            None => ulid::Ulid::new(),
        };
        let operation = ulid::Ulid::new();
        let receipt = ManagedConfigurationReceipt {
            version: 1,
            operation_id: operation.to_string(),
            deployment_id: deployment.to_string(),
            revision,
            generation,
            resource_policy_revision: policy.revision,
            accepted_at_ms: now_ms,
        };
        if target.is_some() {
            tx.execute("UPDATE deployments SET name=?2,revision=?3,current_generation=MAX(current_generation,?4),route_model_id=NULL,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1",params![receipt.deployment_id,effective.name,revision,generation])?;
            tx.execute(
                "DELETE FROM deployment_routes WHERE deployment_id=?1",
                [&receipt.deployment_id],
            )?;
        } else {
            tx.execute("INSERT INTO deployments(id,name,kind,desired_state,observed_state,admission_enabled,dispatch_enabled,suspended,current_generation,schema_version,revision) VALUES(?1,?2,'model','stopped','stopped',0,0,0,1,1,1)",params![receipt.deployment_id,effective.name])?;
        }
        for route in &effective.routes {
            tx.execute(
                "INSERT INTO deployment_routes(route,deployment_id) VALUES(?1,?2)",
                params![route, receipt.deployment_id],
            )?;
        }
        tx.execute("INSERT INTO effective_revisions(deployment_id,revision,effective_json,fingerprint) VALUES(?1,?2,?3,?4)",params![receipt.deployment_id,revision,effective_json,effective.recipe_fingerprint])?;
        // ADR 0013 §3, §7: the revision's instance count and placement, every
        // host it resolved or was refused on, and instance rows `0..N-1`.
        let mut hosts = Vec::with_capacity(resolved.len());
        for host in &resolved {
            let json = serde_json::to_string(&host.effective)
                .map_err(|_| ManagedConfigurationError::Invalid)?;
            if json.len() > MAX_BYTES {
                return Err(ManagedConfigurationError::Invalid);
            }
            let mut devices = Vec::with_capacity(host.devices.len());
            for (device, source, on_device) in &host.devices {
                let json = serde_json::to_string(on_device)
                    .map_err(|_| ManagedConfigurationError::Invalid)?;
                if json.len() > MAX_BYTES {
                    return Err(ManagedConfigurationError::Invalid);
                }
                devices.push(crate::instances::DeviceResolution {
                    device: device.clone(),
                    effective_json: json,
                    fingerprint: on_device.recipe_fingerprint.clone(),
                    source_json: source.to_string(),
                });
            }
            hosts.push(crate::instances::ResolvedHost {
                host_id: host.host_id.clone(),
                host_name: host.host_name.clone(),
                effective_json: json,
                fingerprint: host.effective.recipe_fingerprint.clone(),
                source_json: host.source.to_string(),
                devices,
            });
        }
        crate::instances::record_accepted_revision(
            &tx,
            &receipt.deployment_id,
            revision,
            &instance_spec,
            &hosts,
            &refusals,
            running.then_some(crate::instances::Replacement {
                now_ms,
                restart_until_ms: now_ms.saturating_add(effective.request_deadline_ms),
            }),
        )
        .map_err(|error| match error {
            crate::instances::InstanceError::Sql(error) => ManagedConfigurationError::Sql(error),
            _ => ManagedConfigurationError::Invalid,
        })?;
        // SPEC §13: retain the source alongside the frozen revision atomically.
        // Remote agents re-resolve local IDs, never execute server-supplied argv.
        tx.execute(
            "INSERT INTO managed_configuration_sources VALUES(?1,?2,?3)",
            params![receipt.deployment_id, revision, source.to_string()],
        )?;
        // ADR 0014 §7 (WE3): every accepted revision starts with its checkpoint
        // digest pending (`checkpoint_digest_pending`) until a host measures it.
        crate::checkpoint_digests::insert_accepted(
            &tx,
            &receipt.deployment_id,
            revision,
            &effective,
            provisional,
            now_ms,
        )?;
        // ADR 0008: a declared remote source starts pending on its host until
        // that host materializes it (`model_source_pending`).
        crate::model_sources::insert_accepted(
            &tx,
            &receipt.deployment_id,
            revision,
            &effective,
            now_ms,
        )?;
        persist_receipt(
            &tx,
            principal,
            &scope,
            key,
            &hash,
            &receipt,
            kind,
            &command_fingerprint,
        )?;
        append_event(
            &tx,
            &EventMetadata::ManagedConfigurationAccepted {
                operation_id: EventOperationId::generated(operation),
                deployment_id: EventOperationId::generated(deployment),
                revision,
                generation,
                session_epoch: session.epoch(),
            },
        )
        .map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
        tx.commit()?;
        Ok(receipt)
    }
}

/// ADR 0014 §5, §7: resolve a deployment for acceptance. A memory request or KV
/// cache derived from weights the checkpoint digest has not measured yet
/// (`NotMaterializable` under `engine_config.memory`) is accepted
/// `provisional`: frozen with zero weights, a bound nothing may reserve, and
/// re-resolved exactly once the digest records the weights. Activation of a
/// provisional revision waits for that (`checkpoint_digest_pending`).
fn resolve_for_acceptance(
    config: &Value,
    trusted_host: &Value,
) -> Result<(capyctl_config::effective::EffectiveDeployment, bool)> {
    match resolve_effective(config, trusted_host) {
        Ok(effective) => Ok((effective, false)),
        Err(error)
            if error.code == capyctl_config::ConfigErrorCode::NotMaterializable
                && error.path.starts_with("engine_config.memory") =>
        {
            capyctl_config::effective::resolve_effective_with_checkpoint(
                config,
                trusted_host,
                capyctl_config::effective::CheckpointFacts::provisional(),
            )
            .map(|effective| (effective, true))
            .map_err(ManagedConfigurationError::Rejected)
        }
        Err(error) => Err(ManagedConfigurationError::Rejected(error)),
    }
}

fn ensure_routes(tx: &Transaction<'_>, name: &str, routes: &[String], own: &str) -> Result<()> {
    // Reconcile every legacy alias before accepting any new route. UNION
    // deduplicates the same deployment represented in both old and new tables.
    let ambiguous: bool = tx.query_row("SELECT EXISTS(SELECT route FROM (SELECT route,deployment_id FROM deployment_routes UNION SELECT route_model_id,id FROM deployments WHERE route_model_id IS NOT NULL) GROUP BY route HAVING count(*)>1)",[],|r|r.get(0))?;
    if ambiguous {
        return Err(ManagedConfigurationError::RouteConflict);
    }
    let name_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployments WHERE name=?1 AND id!=?2)",
        params![name, own],
        |r| r.get(0),
    )?;
    if name_exists {
        return Err(ManagedConfigurationError::RouteConflict);
    }
    for route in routes {
        let collision: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM deployment_routes WHERE route=?1 AND deployment_id!=?2 UNION ALL SELECT 1 FROM deployments WHERE route_model_id=?1 AND id!=?2)",params![route,own],|r|r.get(0))?;
        if collision {
            return Err(ManagedConfigurationError::RouteConflict);
        }
    }
    Ok(())
}

/// Link a frozen revision to its actual acceptance receipt without re-resolving
/// mutable profiles. Ordinary lifecycle authority must validate all settings,
/// including fields deliberately excluded from qualification identity.
pub(crate) fn validate_revision_history(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
    effective_json: &str,
) -> Result<()> {
    let mut statement=tx.prepare("SELECT c.response_json,c.request_hash,c.command_scope,c.operation_id,o.kind FROM command_receipts c JOIN operations o ON o.id=c.operation_id WHERE o.deployment_id=?1 AND o.kind IN ('managed_configuration_create','managed_configuration_replace') AND o.state='succeeded' AND o.error_code IS NULL AND COALESCE(json_extract(c.response_json,'$.receipt.revision'),json_extract(c.response_json,'$.revision'))=?2")?;
    let rows = statement
        .query_map(params![deployment, revision], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let [(body, hash, scope, operation, kind)] = rows.as_slice() else {
        return Err(ManagedConfigurationError::CorruptStoredData);
    };
    if body.len() > MAX_BYTES || effective_json.len() > MAX_BYTES {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    let envelope: Value =
        serde_json::from_str(body).map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
    let (receipt, command_fingerprint) = if envelope["version"] == 2 {
        let stored: StoredReceiptV2 =
            serde_json::from_str(body).map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
        (stored.receipt, Some(stored.command_fingerprint))
    } else {
        (
            serde_json::from_str::<ManagedConfigurationReceipt>(body)
                .map_err(|_| ManagedConfigurationError::CorruptStoredData)?,
            None,
        )
    };
    let create = kind == "managed_configuration_create";
    let expected_scope = if create {
        "POST /management/v1/deployments/stopped".into()
    } else {
        format!("PUT /management/v1/deployments/{deployment}/stopped-configuration")
    };
    if receipt.version != 1
        || receipt.deployment_id != deployment
        || receipt.revision != revision
        || receipt.operation_id != *operation
        || receipt.generation < 1
        || receipt.accepted_at_ms < 0
        || receipt.resource_policy_revision < 1
        || *scope != expected_scope
        || (create && revision != 1)
        || (!create && revision <= 1)
        || command_fingerprint.as_ref().is_some_and(|fingerprint| {
            fingerprint.len() != 64
                || !fingerprint
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    let effective: Value = serde_json::from_str(effective_json)
        .map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
    let mut input = json!({"version":1,"scope":scope,"expected_revision":if create {None} else {Some(revision-1)},"effective":effective});
    if let Some(fingerprint) = command_fingerprint {
        input["version"] = json!(2);
        input["command_fingerprint"] = json!(fingerprint);
    }
    let computed = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&input).map_err(|_| ManagedConfigurationError::CorruptStoredData)?
        )
    );
    if computed != *hash {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    Ok(())
}

/// The idempotency scope of a configuration create (`None`) or replacement.
fn configuration_scope(target: Option<&str>) -> String {
    target.map_or_else(
        || "POST /management/v1/deployments/stopped".to_string(),
        |id| format!("PUT /management/v1/deployments/{id}/stopped-configuration"),
    )
}

#[allow(clippy::too_many_arguments)]
fn replay(
    tx: &Transaction<'_>,
    principal: &str,
    scope: &str,
    key: &str,
    kind: &str,
    target: Option<&str>,
    requested_revision: Option<i64>,
    config: &Value,
    targets: &[HostTarget],
) -> Result<Option<ManagedConfigurationReceipt>> {
    let row: Option<(String,String,String)> = tx.query_row("SELECT request_hash,operation_id,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3",params![principal,scope,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((stored_hash, operation, body)) = row else {
        return Ok(None);
    };
    if body.len() > MAX_BYTES {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    let envelope: Value =
        serde_json::from_str(&body).map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
    let (receipt, command_fingerprint) = if envelope["version"] == 2 {
        let stored: StoredReceiptV2 = serde_json::from_str(&body)
            .map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
        if stored.command_fingerprint.len() != 64
            || !stored
                .command_fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ManagedConfigurationError::CorruptStoredData);
        }
        (stored.receipt, Some(stored.command_fingerprint))
    } else {
        (
            serde_json::from_str(&body)
                .map_err(|_| ManagedConfigurationError::CorruptStoredData)?,
            None,
        )
    };
    if receipt.version != 1
        || receipt.operation_id != operation
        || receipt.revision < 1
        || receipt.generation < 1
        || receipt.resource_policy_revision < 1
        || receipt.accepted_at_ms < 0
    {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN effective_revisions e ON e.deployment_id=o.deployment_id WHERE o.id=?1 AND o.deployment_id=?2 AND o.kind=?3 AND o.state='succeeded' AND o.error_code IS NULL AND e.revision=?4)",params![operation,receipt.deployment_id,kind,receipt.revision],|r|r.get(0))?;
    if !valid
        || target.is_some_and(|id| id != receipt.deployment_id)
        || ulid::Ulid::from_string(&receipt.deployment_id).is_err()
    {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    let frozen: String = tx.query_row(
        "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1 AND revision=?2",
        params![receipt.deployment_id, receipt.revision],
        |r| r.get(0),
    )?;
    if frozen.len() > MAX_BYTES {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    let effective: Value =
        serde_json::from_str(&frozen).map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
    // ADR 0013 §3: the command is judged as the canonical host saw it, scoped
    // to that host's ledger keys when it is an enrolled one.
    let canonical_host = effective["host"]["name"].as_str().unwrap_or_default();
    let host_target = targets
        .iter()
        .find(|t| t.host_id == canonical_host)
        .or_else(|| targets.first());
    let config = match host_target {
        Some(target) => scoped_source(target, config)
            .map_err(|_| ManagedConfigurationError::IdempotencyConflict)?,
        None => config.clone(),
    };
    let config = &config;
    let trusted_host = host_target
        .map(|target| target.trusted_host.clone())
        .unwrap_or(Value::Null);
    let trusted_host = &trusted_host;
    let expected_revision = target.map(|_| receipt.revision - 1);
    let mut frozen_input = json!({"version":1,"scope":scope,"expected_revision":expected_revision,"effective":effective});
    if let Some(fingerprint) = &command_fingerprint {
        frozen_input["version"] = json!(2);
        frozen_input["command_fingerprint"] = json!(fingerprint);
    }
    let frozen_hash = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&frozen_input)
                .map_err(|_| ManagedConfigurationError::CorruptStoredData)?
        )
    );
    if frozen_hash != stored_hash {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    if requested_revision != expected_revision {
        return Err(ManagedConfigurationError::IdempotencyConflict);
    }
    if let Some(fingerprint) = command_fingerprint {
        let deadline = effective["request_deadline_ms"]
            .as_i64()
            .filter(|v| *v > 0)
            .ok_or(ManagedConfigurationError::CorruptStoredData)?;
        let requested = deployment_command_fingerprint(config, deadline)
            .map_err(|_| ManagedConfigurationError::IdempotencyConflict)?;
        // Owner decision 2026-09-22: a command accepted before E1 and retried
        // after the upgrade still names its original body, which the upgrade
        // recorded; it is the same command, not a conflicting one.
        let legacy: Option<String> = tx.query_row("SELECT legacy_command_fingerprint FROM engine_config_migrations WHERE deployment_id=?1 AND revision=?2 AND outcome='migrated'", params![receipt.deployment_id, receipt.revision], |r| r.get(0)).optional()?.flatten();
        if requested != fingerprint && legacy.as_deref() != Some(requested.as_str()) {
            return Err(ManagedConfigurationError::IdempotencyConflict);
        }
    } else {
        // Pre-V2 receipts did not retain independent command identity. Preserve
        // their exact old resolution rule; never rewrite historical receipts.
        let mut requested = resolve_effective(config, trusted_host)
            .map_err(|_| ManagedConfigurationError::IdempotencyConflict)?;
        requested.routes.sort();
        if serde_json::to_value(requested).map_err(|_| ManagedConfigurationError::Invalid)?
            != effective
        {
            return Err(ManagedConfigurationError::IdempotencyConflict);
        }
    }
    Ok(Some(receipt))
}

/// The next revision and generation of a replacement, and whether the
/// deployment is running (holds a coherent runtime on some instance).
///
/// A stopped deployment is replaced exactly as before instances existed, and
/// so is one whose start failed once nothing is retained (found live
/// 2026-10-03: it had to be deleted to be corrected). ADR
/// 0013 §7 and owner decision Q8: a running deployment is replaced too — a
/// count-only revision leaves its running instances untouched and any other
/// revision stops and restarts them — but only while everything it retains is
/// explained by an instance's retained launch. Any other retained accounting
/// (an owner, a lease, a claim or an endpoint without such a launch) still
/// refuses the replacement, because nothing could reconcile it.
fn replacement_fence(tx: &Transaction<'_>, id: &str, expected: i64) -> Result<(i64, i64, bool)> {
    let row:Option<(i64,i64,bool)> = tx.query_row("SELECT revision,current_generation,(observed_state IN ('stopped','failed') AND admission_enabled=0 AND dispatch_enabled=0 AND suspended=0 AND kind='model') FROM deployments WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let (revision, generation, stopped) = row.ok_or(ManagedConfigurationError::RevisionConflict)?;
    if revision != expected {
        return Err(ManagedConfigurationError::RevisionConflict);
    }
    let managed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE deployment_id=?1 AND kind='managed_configuration_create' AND state='succeeded')",[id],|r|r.get(0))?;
    let retained: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE deployment_id=?1 AND state!='released' UNION ALL SELECT 1 FROM endpoint_leases e JOIN runtime_bindings b ON b.id=e.binding_id WHERE b.deployment_id=?1 UNION ALL SELECT 1 FROM request_leases WHERE deployment_id=?1 UNION ALL SELECT 1 FROM resource_owners WHERE deployment_id=?1 UNION ALL SELECT 1 FROM owners WHERE deployment_id=?1 UNION ALL SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1 UNION ALL SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND state NOT IN ('succeeded','failed') UNION ALL SELECT 1 FROM lifecycle_steps WHERE deployment_id=?1 AND state NOT IN ('completed','cancelled'))",[id],|r|r.get(0))?;
    if !managed {
        return Err(ManagedConfigurationError::RuntimeRetained);
    }
    let running = if stopped && !retained {
        false
    } else {
        // Every retained artifact belongs to an instance with a retained
        // launch the lifecycle can stop; nothing else is retained.
        let coherent: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=?1 AND b.state!='released')
               AND NOT EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=?1 AND b.state!='released'
                   AND NOT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.binding_id=b.id AND o.kind='initialize'))
               AND NOT EXISTS(SELECT 1 FROM endpoint_leases e JOIN runtime_bindings b ON b.id=e.binding_id WHERE b.deployment_id=?1 AND b.state='released')
               AND NOT EXISTS(SELECT 1 FROM owners WHERE deployment_id=?1)
               AND NOT EXISTS(SELECT 1 FROM resource_owners o WHERE o.deployment_id=?1 AND NOT EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=?1 AND b.instance_index=o.instance_index AND b.state!='released'))
               AND NOT EXISTS(SELECT 1 FROM request_leases l WHERE l.deployment_id=?1 AND NOT EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=?1 AND b.instance_index=l.instance_index AND b.state!='released'))
               AND NOT EXISTS(SELECT 1 FROM lifecycle_claims c WHERE c.deployment_id=?1 AND NOT EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=?1 AND b.instance_index=c.instance_index AND b.state!='released'))
               AND EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND kind='model' AND suspended=0)",
            [id],
            |r| r.get(0),
        )?;
        if !coherent {
            return Err(ManagedConfigurationError::RuntimeRetained);
        }
        true
    };
    Ok((
        revision
            .checked_add(1)
            .ok_or(ManagedConfigurationError::RevisionConflict)?,
        generation
            .checked_add(1)
            .filter(|v| *v > 1)
            .ok_or(ManagedConfigurationError::RevisionConflict)?,
        running,
    ))
}

/// The single embedded target a trusted host document names. A document
/// without a name still serves an exact replay, which never resolves it; a new
/// acceptance against it fails to resolve.
fn embedded_target(trusted_host: &Value) -> Result<HostTarget> {
    let host_id = trusted_host["name"]
        .as_str()
        .filter(|name| valid_identifier(name))
        .unwrap_or_default();
    Ok(HostTarget {
        host_id: host_id.into(),
        host_name: host_id.into(),
        trusted_host: trusted_host.clone(),
        scoped: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn persist_receipt(
    tx: &Transaction<'_>,
    principal: &str,
    scope: &str,
    key: &str,
    hash: &str,
    receipt: &ManagedConfigurationReceipt,
    kind: &str,
    command_fingerprint: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,?3,'succeeded')",
        params![receipt.operation_id, receipt.deployment_id, kind],
    )?;
    let stored = StoredReceiptV2 {
        version: 2,
        receipt: receipt.clone(),
        command_fingerprint: command_fingerprint.into(),
    };
    tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",params![principal,scope,key,hash,receipt.operation_id,serde_json::to_string(&stored).map_err(|_| ManagedConfigurationError::Invalid)?])?;
    Ok(())
}

/// Owner decision 2026-09-22 (schema v19): re-seal the acceptance receipt of a
/// revision the upgrade rewrote. The request hash binds the frozen effective
/// revision, so it is recomputed exactly as `validate_revision_history` checks
/// it, and a v2 receipt's command identity becomes the migrated source's.
/// Returns the original body's command fingerprint under today's algorithm, so a
/// retry of that body after the upgrade stays idempotent (see `replay`).
pub(crate) fn reseal_migrated_receipt(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
    effective_json: &str,
    migrated_source: Option<&Value>,
    legacy_source: Option<&Value>,
) -> Result<Option<String>> {
    let mut statement = tx.prepare("SELECT c.principal_id,c.command_scope,c.idempotency_key,c.response_json,o.kind FROM command_receipts c JOIN operations o ON o.id=c.operation_id WHERE o.deployment_id=?1 AND o.kind IN ('managed_configuration_create','managed_configuration_replace') AND o.state='succeeded' AND o.error_code IS NULL AND COALESCE(json_extract(c.response_json,'$.receipt.revision'),json_extract(c.response_json,'$.revision'))=?2")?;
    let rows = statement
        .query_map(params![deployment, revision], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let [(principal, scope, key, body, kind)] = rows.as_slice() else {
        // A revision accepted before managed configuration has no receipt.
        if rows.is_empty() {
            return Ok(None);
        }
        return Err(ManagedConfigurationError::CorruptStoredData);
    };
    let effective: Value = serde_json::from_str(effective_json)
        .map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
    let deadline = effective["request_deadline_ms"]
        .as_i64()
        .filter(|v| *v > 0)
        .ok_or(ManagedConfigurationError::CorruptStoredData)?;
    let create = kind == "managed_configuration_create";
    let mut input = json!({"version":1,"scope":scope,"expected_revision":if create {None} else {Some(revision-1)},"effective":effective});
    let envelope: Value =
        serde_json::from_str(body).map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
    let body = if envelope["version"] == 2 {
        let mut stored: StoredReceiptV2 =
            serde_json::from_str(body).map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
        if let Some(source) = migrated_source {
            stored.command_fingerprint = deployment_command_fingerprint(source, deadline)
                .map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
        }
        input["version"] = json!(2);
        input["command_fingerprint"] = json!(stored.command_fingerprint);
        serde_json::to_string(&stored).map_err(|_| ManagedConfigurationError::Invalid)?
    } else {
        body.clone()
    };
    let hash = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&input).map_err(|_| ManagedConfigurationError::CorruptStoredData)?
        )
    );
    tx.execute("UPDATE command_receipts SET request_hash=?4,response_json=?5 WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3",params![principal,scope,key,hash,body])?;
    Ok(legacy_source.and_then(|source| deployment_command_fingerprint(source, deadline).ok()))
}

#[cfg(test)]
mod tests;

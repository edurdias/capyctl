//! Ordinary Fake cold initialization from a frozen managed configuration.
pub mod cleanup;
// SPEC §13.2 (W13): an owned engine process that exited without a Terminate.
pub mod engine_exit;
mod expiry;
mod failed_launch;
// ADR 0015: discovery that skips instances the coordinator is already driving.
pub mod lanes;
pub(crate) mod legacy_engine_config;
pub mod local_recovery;
pub mod park;
pub mod placement;
mod receipt;
pub mod reconcile;
// SPEC §6.3: an operator's Stop of a deployment that holds nothing.
mod recorded_stop;
pub mod recovery;
pub mod retired_leases;
// Owner decision 2026-09-23: the startup memory budget and per-host gate.
pub mod startup;
// SPEC §10, ADR 0013 §8 (W10): request-driven switching.
pub mod switching;
pub mod unarmed_stop;
pub mod worker;
use crate::lifecycle::completion::{
    canonical_members, check_session, completion_value, decode, encode, fresh, identity_dtos,
    members,
};
use crate::lifecycle::ArmResult;
use crate::resource_ledger::{self, reserve_increase_in_transaction, GrantRequest};
use crate::resource_policy::{read_selected_policy, ResourcePolicySnapshot};
use crate::{
    dispatch::CoordinatorSession,
    lifecycle::{
        insert_prepared_binding, BindingDto, DeploymentFence, IdentityDto, LifecycleError,
        PreparedBinding, ReserveBinding,
    },
};
use mllm_config::effective::{decode_effective_snapshot, EffectiveDeployment};
use mllm_config::resource_controls::ResourceContext;
use mllm_domain::completion::{
    verify_completion, CompletionEvidence, CompletionExpectation, ExecutionIdentities,
    OwnedLaunchReceipt, StepExecutionContext, TransitionToken,
};
use mllm_domain::resources::{
    Allocation, DeviceClaim, MemoryLimit, PhaseFootprint, ResourcePhase, Sharing,
};
use mllm_scheduler::residency::AdmissionContext;
pub use receipt::StartReceipt;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u8,
    kind: Kind,
    operation_id: String,
    step_id: String,
    deployment_id: String,
    revision: i64,
    generation: i64,
    session_id: String,
    binding_id: String,
    incarnation: String,
    binding_json: String,
    effective_json: String,
    accepted_at_ms: i64,
    deadline_ms: i64,
    execution: Option<Execution>,
    /// ADR 0013 §5: the instance this incarnation realizes. Absent for
    /// instance 0, so every plan written before instances existed keeps its
    /// exact encoding (receipts and stored steps compare it byte for byte).
    #[serde(default, skip_serializing_if = "is_zero")]
    instance_index: u32,
    /// Owner decision 2026-09-23: the measured startup peak this start
    /// reserves as its cold phase, frozen when it was accepted. Absent when
    /// the revision's own cold phase applies (declared, placeholder, or no
    /// measurement yet), so every earlier plan keeps its exact encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    startup_bytes: Option<i64>,
    /// Owner decision 2026-09-23: a solo first start. `startup_bytes` is the
    /// host's whole managed limit, reserved because the unmeasured placeholder
    /// exceeds it; the run is measured and later starts use the real peak.
    /// Absent when false, so earlier plans keep their exact encoding.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    whole_host: bool,
    /// Owner decision 2026-09-23 (solo first start): `startup_bytes` is the
    /// placeholder estimate recomputed from the weights sized or hashed after
    /// the revision was frozen, not a measured peak. Absent when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    estimated: bool,
}
fn is_zero(value: &u32) -> bool {
    *value == 0
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Initialize,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Execution {
    issued_at_ms: i64,
    grant_id: String,
    expected_epoch: u64,
    policy_revision: i64,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Association {
    version: u8,
    kind: String,
    step_id: String,
    session_id: String,
    binding_id: String,
    incarnation: String,
    identities: Vec<IdentityDto>,
    observed_at_ms: i64,
    receipt: String,
}

fn resource(error: impl std::fmt::Display) -> LifecycleError {
    LifecycleError::Rejected(error.to_string())
}
fn one(n: usize) -> Result<(), LifecycleError> {
    if n == 1 {
        Ok(())
    } else {
        Err(LifecycleError::Conflict)
    }
}
impl Plan {
    /// ADR 0013 §5: the resource owner charged for this incarnation.
    fn owner(&self) -> String {
        crate::instances::instance_owner_id(&self.deployment_id, self.instance_index)
    }
    fn fence(&self) -> DeploymentFence {
        DeploymentFence {
            deployment_id: self.deployment_id.clone(),
            revision: self.revision,
            generation: self.generation,
        }
    }
    fn context(
        &self,
        effective: &EffectiveDeployment,
    ) -> Result<StepExecutionContext, LifecycleError> {
        let execution = self.execution.as_ref().ok_or(LifecycleError::Conflict)?;
        Ok(StepExecutionContext {
            token: TransitionToken {
                deployment_id: self.deployment_id.clone(),
                revision: self.revision,
                generation: self.generation,
                operation_id: self.operation_id.clone(),
                step_id: self.step_id.clone(),
            },
            binding_id: self.binding_id.clone(),
            incarnation: self.incarnation.clone(),
            issued_at_ms: execution.issued_at_ms,
            deadline_ms: self.deadline_ms,
            identities: ExecutionIdentities::OwnedLaunch,
            completion_target: Some(phase(&effective.resources.ready, ResourcePhase::Ready)),
            grant_id: Some(execution.grant_id.clone()),
            launch_settings: Some(effective.engine_config.clone()),
        })
    }
}
fn phase(p: &mllm_config::effective::PhaseFootprint, phase: ResourcePhase) -> PhaseFootprint {
    PhaseFootprint {
        phase,
        allocations: p
            .allocations
            .iter()
            .map(|a| Allocation {
                domain: a.domain.clone(),
                bytes: a.bytes,
                host_kv_bytes: a.host_kv_bytes,
            })
            .collect(),
        devices: p
            .devices
            .iter()
            .map(|d| DeviceClaim {
                device: d.id.clone(),
                sharing: if d.sharing == mllm_config::effective::Sharing::Shared {
                    Sharing::Shared
                } else {
                    Sharing::Exclusive
                },
            })
            .collect(),
    }
}

pub(crate) fn is_ordinary(tx: &Transaction<'_>, id: &str) -> Result<bool, LifecycleError> {
    Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND o.kind='initialize')", [id], |r| r.get(0))?)
}

// ADR 0011: every managed deployment is ordinary; there is no other kind.
fn check_managed_command_target(tx: &Transaction<'_>, id: &str) -> Result<(), LifecycleError> {
    let managed: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM deployments d JOIN operations o ON o.deployment_id=d.id WHERE d.id=?1 AND d.kind='model' AND o.kind='managed_configuration_create' AND o.state='succeeded')", [id], |r| r.get(0))?;
    if !managed {
        return Err(LifecycleError::Unsupported);
    }
    Ok(())
}

/// ADR 0013 §3: a frozen revision as resolved on one host. The canonical
/// `effective_revisions` row is the first resolving host's and is the one the
/// acceptance receipt binds; every other resolving host's revision was written
/// in the same acceptance transaction beside it.
pub(crate) fn frozen_on_host(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
    host: Option<&str>,
) -> Result<(String, String), LifecycleError> {
    let canonical: Option<(String, String, Option<String>)> = tx.query_row(
        "SELECT effective_json,fingerprint,CASE WHEN json_valid(effective_json) THEN json_extract(effective_json,'$.host.name') END FROM effective_revisions WHERE deployment_id=?1 AND revision=?2",
        params![deployment_id, revision],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).optional()?;
    let (raw, fingerprint, canonical_host) = canonical.ok_or(LifecycleError::Conflict)?;
    match host {
        Some(host) if canonical_host.as_deref() != Some(host) => tx
            .query_row(
                "SELECT effective_json,fingerprint FROM host_effective_revisions WHERE deployment_id=?1 AND revision=?2 AND host_id=?3 AND outcome='resolved'",
                params![deployment_id, revision, host],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(LifecycleError::Conflict),
        _ => Ok((raw, fingerprint)),
    }
}

/// ADR 0019 (discrete GPU design §7): the revision as resolved on `host` with
/// `device` selected, when the host offered a GPU choice for it; otherwise
/// the host's own resolution ([`frozen_on_host`]).
pub(crate) fn frozen_on_device(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
    host: Option<&str>,
    device: Option<&str>,
) -> Result<(String, String), LifecycleError> {
    if let (Some(host), Some(device)) = (host, device) {
        let chosen: Option<(String, String)> = tx
            .query_row(
                "SELECT effective_json,fingerprint FROM host_device_effective_revisions WHERE deployment_id=?1 AND revision=?2 AND host_id=?3 AND device=?4",
                params![deployment_id, revision, host, device],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some(chosen) = chosen {
            return Ok(chosen);
        }
    }
    frozen_on_host(tx, deployment_id, revision, host)
}

/// ADR 0013 §3: validate a frozen revision a plan names. The acceptance
/// receipt binds the canonical revision; a revision resolved on another host
/// must be that host's row of the same accepted revision.
pub(crate) fn validate_frozen(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
    raw: &str,
) -> Result<(), LifecycleError> {
    let canonical: String = tx
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1 AND revision=?2",
            params![deployment_id, revision],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(LifecycleError::CorruptStoredData)?;
    crate::managed_configuration::validate_revision_history(
        tx,
        deployment_id,
        revision,
        &canonical,
    )
    .map_err(|error| match error {
        crate::managed_configuration::ManagedConfigurationError::Sql(error) => {
            LifecycleError::Sql(error)
        }
        _ => LifecycleError::CorruptStoredData,
    })?;
    if raw != canonical {
        // ADR 0019: or one GPU's resolution of it on a multi-GPU host.
        let resolved: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM host_effective_revisions WHERE deployment_id=?1 AND revision=?2 AND outcome='resolved' AND effective_json=?3)
                 OR EXISTS(SELECT 1 FROM host_device_effective_revisions WHERE deployment_id=?1 AND revision=?2 AND effective_json=?3)",
            params![deployment_id, revision, raw],
            |r| r.get(0),
        )?;
        if !resolved {
            return Err(LifecycleError::CorruptStoredData);
        }
    }
    Ok(())
}

fn effective(
    tx: &Transaction<'_>,
    fence: &DeploymentFence,
) -> Result<(String, EffectiveDeployment), LifecycleError> {
    // ADR 0013 §4: an instance runs the revision as resolved on its placed
    // host, and (ADR 0019) on the GPU it was placed on there.
    let (host, device): (Option<String>, Option<String>) = tx
        .query_row(
            "SELECT host_id,device FROM deployment_instances WHERE deployment_id=?1 AND generation=?2",
            params![fence.deployment_id, fence.generation],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or_default();
    let (raw, fingerprint) = frozen_on_device(
        tx,
        &fence.deployment_id,
        fence.revision,
        host.as_deref(),
        device.as_deref(),
    )?;
    validate_frozen(tx, &fence.deployment_id, fence.revision, &raw).map_err(
        |error| match error {
            LifecycleError::Sql(error) => LifecycleError::Sql(error),
            _ => LifecycleError::CorruptStoredData,
        },
    )?;
    let effective =
        decode_effective_snapshot(&raw).map_err(|_| LifecycleError::CorruptStoredData)?;
    let managed: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE deployment_id=?1 AND kind='managed_configuration_create' AND state='succeeded')", [&fence.deployment_id], |r| r.get(0))?;
    if !managed || fingerprint != effective.recipe_fingerprint {
        return Err(LifecycleError::Conflict);
    }
    // ADR 0013 §7: an instance still running an earlier revision (a count-only
    // revision leaves it untouched; a non-count one stops it, owner decision
    // Q8) is judged against that revision, not today's name and routes.
    let current: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND revision=?2)",
        params![fence.deployment_id, fence.revision],
        |r| r.get(0),
    )?;
    if current {
        let consistent: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND revision=?2 AND name=?3 AND route_model_id IS NULL)",
            params![fence.deployment_id,fence.revision,effective.name],|r|r.get(0),
        )?;
        let mut routes = tx
            .prepare("SELECT route FROM deployment_routes WHERE deployment_id=?1 ORDER BY route")?;
        let routes = routes
            .query_map([&fence.deployment_id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !consistent || routes != effective.routes {
            return Err(LifecycleError::Conflict);
        }
    } else {
        let historical: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND revision>?2 AND route_model_id IS NULL)",
            params![fence.deployment_id, fence.revision],
            |r| r.get(0),
        )?;
        if !historical {
            return Err(LifecycleError::Conflict);
        }
    }
    Ok((raw, effective))
}

fn policy(
    tx: &Transaction<'_>,
    effective: &EffectiveDeployment,
) -> Result<ResourcePolicySnapshot, LifecycleError> {
    policy_checked(tx, effective, false)
}
fn policy_checked(
    tx: &Transaction<'_>,
    effective: &EffectiveDeployment,
    detailed: bool,
) -> Result<ResourcePolicySnapshot, LifecycleError> {
    let policy = read_selected_policy(tx, &effective.host.name)
        .map_err(resource)?
        .ok_or(if detailed {
            LifecycleError::ReconciliationRequired
        } else {
            LifecycleError::Conflict
        })?;
    if policy.context != ResourceContext::from_host(&effective.host) {
        return Err(if detailed {
            LifecycleError::ReconciliationRequired
        } else {
            LifecycleError::Conflict
        });
    }
    policy
        .controls
        .validate(&policy.context)
        .map_err(resource)?;
    for selected in &effective.selected_devices {
        let sharing = policy
            .controls
            .device_sharing_overrides
            .get(&selected.id)
            .ok_or(LifecycleError::Conflict)?;
        if selected.sharing == mllm_config::effective::Sharing::Shared
            && (*sharing != selected.sharing || policy.controls.device_sharing != selected.sharing)
        {
            return Err(if detailed {
                LifecycleError::HostPolicyDenied
            } else {
                LifecycleError::Conflict
            });
        }
    }
    Ok(policy)
}

fn current(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    p: &Plan,
    completed: bool,
) -> Result<(), LifecycleError> {
    current_admitted(tx, s, p, completed, true)
}

/// The fenced check that a plan is still this session's current work.
///
/// `admitted` is whether the deployment must still be admitting. Every path that
/// carries authority requires it. Only the deadline release of a step that never
/// armed passes `false`, because a deployment that closed its own admission must
/// still reach its deadline instead of holding a reservation for ever.
// ADR 0011 decision 4: a deployment that fails closes its own admission.
fn current_admitted(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    p: &Plan,
    completed: bool,
    admitted: bool,
) -> Result<(), LifecycleError> {
    check_session(tx, s)?;
    // ADR 0013 §5: the plan's own instance must still carry its fence.
    let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM instance_runtime WHERE id=?1 AND revision=?2 AND current_generation=?3 AND instance_index=?5 AND kind='model' AND desired_state='ready' AND suspended=0 AND (?4=0 OR admission_enabled=1))", params![p.deployment_id,p.revision,p.generation,admitted,p.instance_index], |r|r.get(0))?;
    let claims: bool = tx.query_row("SELECT COUNT(*)=1 AND COALESCE(SUM(deployment_id=?2 AND revision=?3 AND generation=?4),0)=1 FROM lifecycle_claims WHERE operation_id=?1",params![p.operation_id,p.deployment_id,p.revision,p.generation],|r|r.get(0))?;
    if !valid || p.session_id != s.id() || (!completed && !claims) {
        return Err(LifecycleError::Stale);
    }
    Ok(())
}

fn load(
    tx: &Transaction<'_>,
    id: &str,
) -> Result<(Plan, EffectiveDeployment, String), LifecycleError> {
    if !is_ordinary(tx, id)? {
        return Err(LifecycleError::Unsupported);
    }
    let (raw, state): (String, String) = tx.query_row(
        "SELECT step_json,state FROM lifecycle_steps WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let p: Plan = decode(&raw)?;
    if p.version != 1
        || p.step_id != id
        || p.accepted_at_ms < 0
        || p.deadline_ms <= p.accepted_at_ms
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    for id in [
        &p.operation_id,
        &p.step_id,
        &p.deployment_id,
        &p.binding_id,
        &p.incarnation,
        &p.session_id,
    ] {
        if ulid::Ulid::from_string(id).is_err() {
            return Err(LifecycleError::CorruptStoredData);
        }
    }
    let (effective_json, e) = effective(tx, &p.fence())?;
    if effective_json != p.effective_json {
        return Err(LifecycleError::Conflict);
    }
    let identity = binding_identity(tx, &p.deployment_id, p.revision, &e)?;
    let binding: BindingDto = decode(&p.binding_json)?;
    if binding.version != 1
        || binding.identity_id != identity.id()
        || binding.payload != identity.payload()?
        || binding.credential_ref
            != e.profile
                .security
                .credential_ref
                .clone()
                .ok_or(LifecycleError::Conflict)?
    {
        return Err(LifecycleError::Conflict);
    }
    validate_local(tx, &p, &e, &state)?;
    Ok((p, e, state))
}

/// Current local arm/ownership relationships. No catalog cache or send authority.
fn validate_local(
    tx: &Transaction<'_>,
    p: &Plan,
    e: &EffectiveDeployment,
    state: &str,
) -> Result<(), LifecycleError> {
    let binding: BindingDto = decode(&p.binding_json)?;
    let endpoint: std::net::SocketAddr = binding
        .endpoint
        .parse()
        .map_err(|_| LifecycleError::CorruptStoredData)?;
    if endpoint.ip() != std::net::Ipv4Addr::LOCALHOST
        || !(e.host.endpoint_port_range.start..=e.host.endpoint_port_range.end)
            .contains(&endpoint.port())
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let run = crate::lifecycle::validate_initialize_run(
        tx,
        &p.fence(),
        &p.operation_id,
        &p.session_id,
        p.deadline_ms,
    )?;
    let cancelled = state == "cancelled";
    let binding_state = match state {
        "planned" => "reserved",
        "completed" => "live",
        "cancelled" => "released",
        _ => "uncertain",
    };
    let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps WHERE id=?1 AND operation_id=?2 AND deployment_id=?3 AND binding_id=?4 AND session_id=?5 AND ordinal=0) AND EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?4 AND deployment_id=?3 AND revision=?6 AND incarnation=?7 AND ownership='managed' AND binding_json=?8 AND state=?9) AND (?11 OR EXISTS(SELECT 1 FROM endpoint_leases WHERE binding_id=?4 AND host='127.0.0.1' AND port=?10)) AND (SELECT COUNT(*) FROM lifecycle_steps WHERE operation_id=?2)=1 AND (SELECT COUNT(*) FROM endpoint_leases WHERE binding_id=?4)=?12 AND (SELECT COUNT(*) FROM runtime_bindings WHERE deployment_id=?3 AND instance_index=?13 AND state!='released')=?12 AND EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?4 AND instance_index=?13)",params![p.step_id,p.operation_id,p.deployment_id,p.binding_id,p.session_id,p.revision,p.incarnation,p.binding_json,binding_state,endpoint.port(),cancelled,i64::from(!cancelled),p.instance_index],|r|r.get(0))?;
    let expected_operation = match state {
        "planned" if run == "queued" => "pending",
        "armed" if run == "running" => "running",
        "uncertain" if run == "uncertain" => "running",
        "completed" if run == "succeeded" => "succeeded",
        "cancelled" if run == "failed" => "failed",
        _ => return Err(LifecycleError::Conflict),
    };
    let operation: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND deployment_id=?2 AND kind='initialize' AND state=?3 AND error_code IS ?4)",params![p.operation_id,p.deployment_id,expected_operation,cancelled.then_some(expiry::ERROR_CODE)],|r|r.get(0))?;
    if !valid || !operation {
        return Err(LifecycleError::CorruptStoredData);
    }
    let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
    match &p.execution {
        None if state == "planned" || cancelled => {
            let effects: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM resource_grants WHERE operation_id=?1) OR EXISTS(SELECT 1 FROM lifecycle_steps WHERE id=?2 AND grant_id IS NOT NULL)",params![p.operation_id,p.step_id],|r|r.get(0))?;
            if effects || ledger.owners.contains_key(&p.owner()) {
                return Err(LifecycleError::Conflict);
            }
        }
        Some(execution) if state != "planned" => {
            if execution.issued_at_ms < p.accepted_at_ms
                || execution.issued_at_ms >= p.deadline_ms
                || execution.policy_revision < 1
            {
                return Err(LifecycleError::CorruptStoredData);
            }
            let frozen = resource_ledger::encode(&startup::cold(p, e)).map_err(resource)?;
            let identity = encode(&(
                &p.deployment_id,
                &p.operation_id,
                p.revision,
                p.generation,
                execution.expected_epoch,
                &frozen,
            ))?;
            let exact: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM resource_grants WHERE id=?1 AND deployment_id=?2 AND operation_id=?3 AND request_json=?4 AND committed_epoch=?5) AND EXISTS(SELECT 1 FROM lifecycle_steps WHERE id=?6 AND grant_id=?1) AND (SELECT COUNT(*) FROM resource_grants WHERE operation_id=?3)=1", params![execution.grant_id,p.deployment_id,p.operation_id,identity,execution.expected_epoch.checked_add(1).ok_or(LifecycleError::CorruptStoredData)?,p.step_id],|r|r.get(0))?;
            // SPEC §7.3 (W5): a completed launch holds its Ready footprint, or
            // the footprint a park or restore of it has left (parked, or a
            // transition peak while one is armed or uncertain).
            let held = if state == "completed" {
                park::retained_footprint(e, ledger.owners.get(&p.owner()))
            } else {
                ledger.owners.get(&p.owner()) == Some(&startup::cold(p, e))
            };
            if !exact || ledger.epoch <= execution.expected_epoch || !held {
                return Err(LifecycleError::Conflict);
            }
        }
        _ => return Err(LifecycleError::CorruptStoredData),
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Start {
    pub operation_id: String,
    pub step_id: String,
    pub binding_id: String,
    pub joined: bool,
}

/// What a runtime binding is created and verified against.
///
/// ADR 0011 decision 1: every residency is identified this way, including one that
/// parks. A qualification catalog entry used to be required for that case instead;
/// nothing read it when a deployment parked or woke, and what it supplied was this
/// same recipe and host information.
enum BindingIdentity {
    Declared { id: String, payload: String },
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredBindingV1 {
    version: u8,
    kind: &'static str,
    residency: String,
    recipe_fingerprint: String,
    host: String,
    hardware_fingerprint: String,
    environment_fingerprint: String,
}

impl BindingIdentity {
    fn id(&self) -> &str {
        match self {
            Self::Declared { id, .. } => id,
        }
    }
    fn payload(&self) -> Result<String, LifecycleError> {
        match self {
            Self::Declared { payload, .. } => Ok(payload.clone()),
        }
    }
}

/// A runtime's identity, derived from what it was admitted against.
///
/// ADR 0011 decision 1: every residency is identified this way. A changed recipe or
/// a moved host yields a different identity, so a binding cannot be silently reused
/// across either — which is the only property the lifecycle needs from an identity.
/// Parking used to require a qualification catalog entry instead; nothing read it
/// when parking, and what it supplied was this same recipe and host information.
///
/// Owner decision 2026-09-22 (schema v19): a revision the upgrade carried from
/// the pre-E1 shape has a new recipe fingerprint, because the fingerprinted
/// structure changed, not the recipe. Its bindings keep the identity recorded
/// before the upgrade, so a running engine is still this deployment's own and
/// its launch can be adopted, stopped and cleaned up exactly as recorded.
fn binding_identity(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
    e: &mllm_config::effective::EffectiveDeployment,
) -> Result<BindingIdentity, LifecycleError> {
    let recipe_fingerprint: String = tx
        .query_row(
            "SELECT legacy_fingerprint FROM engine_config_migrations WHERE deployment_id=?1 AND revision=?2 AND outcome='migrated' AND fingerprint=?3",
            params![deployment_id, revision, e.recipe_fingerprint],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| e.recipe_fingerprint.clone());
    let descriptor = DeclaredBindingV1 {
        version: 1,
        kind: "declared",
        residency: residency_name(e.residency).to_string(),
        recipe_fingerprint: recipe_fingerprint.clone(),
        host: e.host.name.clone(),
        hardware_fingerprint: e.host.hardware_fingerprint.clone(),
        environment_fingerprint: e.host.environment_fingerprint.clone(),
    };
    Ok(BindingIdentity::Declared {
        id: format!("declared:{recipe_fingerprint}"),
        payload: encode(&descriptor)?,
    })
}

/// The serialized name of a residency, stable across refactors of the enum.
fn residency_name(residency: mllm_config::effective::Residency) -> &'static str {
    use mllm_config::effective::Residency;
    match residency {
        Residency::RestartOnly => "restart_only",
        Residency::HostBacked => "host_backed",
        Residency::Deep => "deep",
    }
}

impl crate::Store {
    /// Clock-aware administrative start; only an actual Fake catalog
    /// and its verified source cleanup authorize a fresh managed binding.
    pub fn accept_start(
        &self,
        s: &CoordinatorSession,
        f: &DeploymentFence,
        now: i64,
        deadline: i64,
    ) -> Result<Start, LifecycleError> {
        if now < 0 || deadline <= now {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let accepted = Self::accept_start_in_transaction(&tx, s, f, now, deadline, false)?;
        tx.commit()?;
        Ok(accepted)
    }

    fn accept_start_in_transaction(
        tx: &Transaction<'_>,
        s: &CoordinatorSession,
        f: &DeploymentFence,
        now: i64,
        deadline: i64,
        detailed: bool,
    ) -> Result<Start, LifecycleError> {
        let existing: Option<String> = tx.query_row("SELECT s.id FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=s.operation_id JOIN operations o ON o.id=r.operation_id WHERE r.deployment_id=?1 AND r.revision=?2 AND r.generation=?3 AND r.state IN ('queued','running','uncertain') AND o.kind='initialize'",params![f.deployment_id,f.revision,f.generation],|r|r.get(0)).optional()?;
        if let Some(id) = existing {
            let (p, _, _) = load(tx, &id)?;
            current(tx, s, &p, false)?;
            return Ok(Start {
                operation_id: p.operation_id,
                step_id: p.step_id,
                binding_id: p.binding_id,
                joined: true,
            });
        }
        if detailed {
            check_managed_command_target(tx, &f.deployment_id)?;
        }
        // ADR 0013 §5: the fence names the instance to start.
        let instance = crate::instances::fence_instance(tx, f).map_err(|error| match error {
            LifecycleError::Stale => LifecycleError::Conflict,
            error => error,
        })?;
        let (raw, e) = effective(tx, f)?;
        // ADR 0014 §7 (WE3): a revision whose resources wait for its checkpoint
        // digest, or whose checkpoint is known not to match, never starts.
        crate::checkpoint_digests::admit_start(tx, &f.deployment_id, f.revision)?;
        // ADR 0008: and while its declared remote source is not yet on disk.
        crate::model_sources::admit_start(tx, &f.deployment_id, f.revision)?;
        let identity =
            binding_identity(tx, &f.deployment_id, f.revision, &e).map_err(
                |error| match error {
                    LifecycleError::Invalid | LifecycleError::Conflict if detailed => {
                        LifecycleError::Unsupported
                    }
                    error => error,
                },
            )?;
        let controls = policy_checked(tx, &e, detailed)?.controls;
        let outstanding: i64 = tx.query_row(
            "SELECT COUNT(*) FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id WHERE o.kind='initialize' AND r.state NOT IN ('succeeded','failed')",
            [], |row| row.get(0),
        )?;
        if outstanding >= i64::from(controls.queue.max_pending_total) {
            if detailed {
                return Err(LifecycleError::QueueFull);
            }
            return Err(LifecycleError::Rejected("initialization queue full".into()));
        }
        if deadline
            .checked_sub(now)
            .is_none_or(|duration| duration > e.request_deadline_ms)
        {
            return Err(LifecycleError::Invalid);
        }
        // SPEC §6 / T30: a verified failed launch keeps desired=ready. Explicit
        // retry may create a fresh operation only after all retained runtime,
        // claims, leases, and reservations are gone; the guards below remain.
        // ADR 0013 §5: the instance, not its siblings, must hold nothing.
        let stopped: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM instance_runtime WHERE id=?1 AND revision=?2 AND current_generation=?3 AND instance_index=?5 AND name=?4 AND kind='model' AND lifecycle='active' AND (desired_state='stopped' OR (desired_state='ready' AND EXISTS(SELECT 1 FROM operations o JOIN lifecycle_runs r ON r.operation_id=o.id WHERE o.deployment_id=?1 AND r.instance_index=?5 AND o.kind='initialize' AND o.state='failed' AND o.error_code='launch_failed'))) AND observed_state='stopped' AND admission_enabled=0 AND dispatch_enabled=0 AND suspended=0) AND NOT EXISTS(SELECT 1 FROM runtime_bindings WHERE deployment_id=?1 AND instance_index=?5 AND state!='released') AND NOT EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1 AND instance_index=?5) AND NOT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?5) AND NOT EXISTS(SELECT 1 FROM resource_owners WHERE deployment_id=?1 AND instance_index=?5) AND NOT EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND instance_index=?5 AND state NOT IN ('succeeded','failed'))", params![f.deployment_id,f.revision,f.generation,e.name,instance],|r|r.get(0))?;
        if !stopped {
            if detailed {
                let retained: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE deployment_id=?1 AND instance_index=?2 AND state!='released') OR EXISTS(SELECT 1 FROM resource_owners WHERE deployment_id=?1 AND instance_index=?2) OR EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?2)", params![f.deployment_id, instance], |r| r.get(0))?;
                if retained {
                    return Err(LifecycleError::RuntimeRetained);
                }
            }
            return Err(LifecycleError::Conflict);
        }
        let operation_id = ulid::Ulid::new().to_string();
        let step_id = ulid::Ulid::new().to_string();
        let binding_id = ulid::Ulid::new().to_string();
        let incarnation = ulid::Ulid::new().to_string();
        let credential_ref = e
            .profile
            .security
            .credential_ref
            .clone()
            .filter(|v| !v.trim().is_empty())
            .ok_or(LifecycleError::Conflict)?;
        let payload = identity.payload()?;
        let mut reserved = None;
        // SPEC §3 (v21): a host's port range is its own. Only leases on this
        // instance's host take a port out of it.
        let host_key = crate::lifecycle::endpoint_host_key(tx, f)?;
        for port in e.host.endpoint_port_range.start..=e.host.endpoint_port_range.end {
            let leased: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM endpoint_leases WHERE host_id=?2 AND host='127.0.0.1' AND port=?1)",
                params![port, host_key],
                |r| r.get(0),
            )?;
            if leased {
                continue;
            }
            let request = ReserveBinding {
                id: binding_id.clone(),
                fence: f.clone(),
                incarnation: incarnation.clone(),
                identity_id: identity.id().into(),
                ownership: "managed".into(),
                endpoint_host: "127.0.0.1".into(),
                endpoint_port: port,
                credential_ref: credential_ref.clone(),
                binding_payload: payload.clone(),
            };
            match PreparedBinding::prepare_for_host(tx, &request) {
                Ok(binding) => {
                    insert_prepared_binding(tx, s, &binding)?;
                    reserved = Some(binding);
                    break;
                }
                Err(LifecycleError::Conflict) => continue,
                Err(error) => return Err(error),
            }
        }
        let _reservation = reserved.ok_or(if detailed {
            LifecycleError::CapacityBlocked
        } else {
            LifecycleError::Conflict
        })?;
        let binding_json = tx.query_row(
            "SELECT binding_json FROM runtime_bindings WHERE id=?1",
            [&binding_id],
            |r| r.get(0),
        )?;
        let startup::FrozenStartup {
            bytes: startup_bytes,
            whole_host,
            estimated,
        } = startup::frozen_plan_startup(tx, &f.deployment_id, f.revision, &e)?;
        let p = Plan {
            version: 1,
            kind: Kind::Initialize,
            operation_id: operation_id.clone(),
            step_id: step_id.clone(),
            deployment_id: f.deployment_id.clone(),
            revision: f.revision,
            generation: f.generation,
            session_id: s.id().into(),
            binding_id: binding_id.clone(),
            incarnation,
            binding_json,
            effective_json: raw,
            accepted_at_ms: now,
            deadline_ms: deadline,
            execution: None,
            instance_index: instance,
            startup_bytes,
            whole_host,
            estimated,
        };
        tx.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'initialize','pending')",params![operation_id,f.deployment_id])?;
        crate::lifecycle::insert_initialize_run(tx, s, f, &operation_id, deadline)?;
        tx.execute("INSERT INTO lifecycle_claims(deployment_id,operation_id,revision,generation,instance_index) VALUES(?1,?2,?3,?4,?5)",params![f.deployment_id,operation_id,f.revision,f.generation,instance])?;
        tx.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,0,?3,?4,?5,'planned',?6)",params![step_id,operation_id,f.deployment_id,binding_id,s.id(),encode(&p)?])?;
        // ADR 0013 §6: the instance is wanted and admitting; the deployment's
        // aggregate follows its instances.
        tx.execute(
            "UPDATE deployment_instances SET desired_state='ready',admission_enabled=1,pending_start_until_ms=NULL,last_error=NULL WHERE deployment_id=?1 AND instance_index=?2",
            params![f.deployment_id, instance],
        )?;
        event(tx, s, &p, Transition::Accepted, None)?;
        Ok(Start {
            operation_id,
            step_id,
            binding_id,
            joined: false,
        })
    }
    /// Reading or cloning context never authorizes replay; only ArmResult::New does.
    pub fn initialize_execution(
        &self,
        s: &CoordinatorSession,
        id: &str,
    ) -> Result<StepExecutionContext, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let (p, e, state) = load(&tx, id)?;
        current(&tx, s, &p, state == "completed")?;
        p.context(&e)
    }
}

impl crate::Store {
    /// Records one spawn attempt and its conservative grant atomically. Only `New`
    /// may lead to a later send.
    ///
    /// ADR 0011: the only armed step an mllm store knows is an ordinary initialize.
    pub fn arm_step(
        &self,
        s: &CoordinatorSession,
        id: &str,
        context: AdmissionContext<'_>,
    ) -> Result<ArmResult, LifecycleError> {
        if id.parse::<ulid::Ulid>().is_ok_and(|v| v.to_string() == id) {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            check_session(&tx, s)?;
            if is_ordinary(&tx, id)? {
                let result = arm(&tx, s, id, context)?;
                tx.commit()?;
                return Ok(result);
            }
            return Err(LifecycleError::Unsupported);
        }
        Err(LifecycleError::Invalid)
    }
}

pub(crate) fn arm(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
    context: AdmissionContext<'_>,
) -> Result<ArmResult, LifecycleError> {
    arm_with_context(tx, s, id, context, &[]).map(|(arm, _)| arm)
}

pub(crate) fn arm_with_context(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
    context: AdmissionContext<'_>,
    residents: &[mllm_domain::resources::ProcessResident],
) -> Result<(ArmResult, Option<StepExecutionContext>), LifecycleError> {
    let (mut p, e, state) = load(tx, id)?;
    current(tx, s, &p, false)?;
    if matches!(state.as_str(), "armed" | "uncertain") {
        return Ok((ArmResult::AlreadyRecorded, None));
    }
    if state != "planned"
        || context.now_ms < p.accepted_at_ms
        || context.now_ms >= p.deadline_ms
        || !context.resident_floors.is_empty()
    {
        return Err(LifecycleError::Conflict);
    }
    let policy = policy(tx, &e)?;
    let limits: Vec<_> = policy
        .controls
        .domains
        .iter()
        .map(|(id, d)| MemoryLimit {
            domain: id.clone(),
            managed_bytes: d.managed_limit,
            free_reserve_bytes: d.free_reserve,
            host_kv_bytes: d.host_kv_limit,
            parked_bytes: d.parked_limit,
        })
        .collect();
    let mut supplied = context.limits.to_vec();
    supplied.sort_by(|a, b| a.domain.cmp(&b.domain));
    if supplied != limits
        || context.ttl_ms != policy.controls.observation_ttl_ms
        || context.max_parked != policy.controls.max_parked as usize
    {
        return Err(LifecycleError::Conflict);
    }
    let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
    // ADR 0007 (found live 2026-09-23, matrix M33): Ready engines on this
    // host are credited what the host sampled for their own processes, so
    // memory they already hold is not charged a second time.
    let floors = crate::resident_floors::resident_floors(
        tx,
        &ledger,
        &p.owner(),
        context.observations,
        residents,
        &crate::resident_floors::domain_kinds(&policy.controls),
    )?;
    let context = AdmissionContext {
        resident_floors: &floors,
        ..context
    };
    let execution = Execution {
        issued_at_ms: context.now_ms,
        grant_id: ulid::Ulid::new().to_string(),
        expected_epoch: ledger.epoch,
        policy_revision: policy.revision,
    };
    reserve_increase_in_transaction(
        tx,
        &GrantRequest {
            id: execution.grant_id.clone(),
            owner_id: p.owner(),
            deployment_id: p.deployment_id.clone(),
            operation_id: p.operation_id.clone(),
            revision: p.revision,
            generation: p.generation,
            expected_epoch: ledger.epoch,
            // Owner decision 2026-09-23: the startup peak until Ready.
            next: startup::cold(&p, &e),
        },
        context,
    )
    .map_err(resource)?;
    one(tx.execute(
        "UPDATE runtime_bindings SET state='uncertain' WHERE id=?1 AND state='reserved'",
        [&p.binding_id],
    )?)?;
    one(tx.execute(
        "UPDATE operations SET state='running' WHERE id=?1 AND state='pending'",
        [&p.operation_id],
    )?)?;
    one(tx.execute("UPDATE lifecycle_runs SET state='running' WHERE operation_id=?1 AND session_id=?2 AND state='queued'",params![p.operation_id,s.id()])?)?;
    p.execution = Some(execution);
    one(tx.execute("UPDATE lifecycle_steps SET state='armed',step_json=?2,grant_id=?3 WHERE id=?1 AND session_id=?4 AND state='planned' AND grant_id IS NULL",params![id,encode(&p)?,p.execution.as_ref().unwrap().grant_id,s.id()])?)?;
    event(tx, s, &p, Transition::Armed, None)?;
    Ok((ArmResult::New { step_id: id.into() }, Some(p.context(&e)?)))
}

fn association(tx: &Transaction<'_>, p: &Plan) -> Result<Option<Association>, LifecycleError> {
    let raw: Option<String> = tx.query_row("SELECT association_json FROM owned_launch_associations WHERE step_id=?1 AND binding_id=?2 AND incarnation=?3",params![p.step_id,p.binding_id,p.incarnation],|r|r.get(0)).optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let a: Association = decode(&raw)?;
    let stored: String = tx.query_row(
        "SELECT identities_json FROM runtime_bindings WHERE id=?1",
        [&p.binding_id],
        |r| r.get(0),
    )?;
    let identities: Vec<IdentityDto> = decode(&stored)?;
    if a.version != 1
        || a.kind != "owned_launch"
        || a.step_id != p.step_id
        || a.session_id != p.session_id
        || a.binding_id != p.binding_id
        || a.incarnation != p.incarnation
        || identities != a.identities
    {
        return Err(LifecycleError::Conflict);
    }
    canonical_members(&members(&a.identities)?)?;
    crate::lifecycle::completion::nonempty_receipt(&a.receipt)?;
    Ok(Some(a))
}

pub(crate) fn record_launch(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
    r: &OwnedLaunchReceipt,
    now: i64,
) -> Result<(), LifecycleError> {
    let (p, e, state) = load(tx, id)?;
    current(tx, s, &p, state == "completed")?;
    let context = p.context(&e)?;
    crate::lifecycle::completion::nonempty_receipt(&r.receipt)?;
    if r.binding_id != p.binding_id || r.incarnation != p.incarnation {
        return Err(LifecycleError::Conflict);
    }
    let canonical = canonical_members(&r.identities)?;
    let supplied = Association {
        version: 1,
        kind: "owned_launch".into(),
        step_id: id.into(),
        session_id: s.id().into(),
        binding_id: r.binding_id.clone(),
        incarnation: r.incarnation.clone(),
        identities: identity_dtos(&canonical),
        observed_at_ms: r.observed_at_ms,
        receipt: r.receipt.clone(),
    };
    if let Some(old) = association(tx, &p)? {
        return if old == supplied {
            Ok(())
        } else {
            Err(LifecycleError::Conflict)
        };
    }
    if state != "armed" {
        return Err(LifecycleError::Conflict);
    }
    fresh(
        context.issued_at_ms,
        context.deadline_ms,
        r.observed_at_ms,
        now,
        policy(tx, &e)?.controls.observation_ttl_ms,
    )?;
    // Spec §3: the association wrote the API identity before the engine ran.
    // A durable launcher records that identity as soon as the API process
    // exists, then this call must accept exactly that prior content, or none.
    let prior_json: String = tx.query_row(
        "SELECT identities_json FROM runtime_bindings WHERE id=?1",
        [&p.binding_id],
        |r| r.get(0),
    )?;
    let prior: Vec<IdentityDto> = decode(&prior_json)?;
    if !prior.is_empty() && prior != identity_dtos(std::slice::from_ref(&canonical[0])) {
        return Err(LifecycleError::Conflict);
    }
    tx.execute("INSERT INTO owned_launch_associations(step_id,binding_id,incarnation,association_json) VALUES(?1,?2,?3,?4)",params![id,p.binding_id,p.incarnation,encode(&supplied)?])?;
    tx.execute(
        "UPDATE runtime_bindings SET identities_json=?2 WHERE id=?1",
        params![p.binding_id, encode(&supplied.identities)?],
    )?;
    event(tx, s, &p, Transition::OwnedLaunchAssociated, None)
}

pub(crate) fn complete(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
    evidence: &CompletionEvidence,
    now: i64,
    ttl: i64,
) -> Result<(), LifecycleError> {
    let (p, e, state) = load(tx, id)?;
    current(tx, s, &p, state == "completed")?;
    let association = association(tx, &p)?.ok_or(LifecycleError::Conflict)?;
    let supplied = encode(&completion_value(evidence)?)?;
    let prior: Option<(String, u64)> = tx
        .query_row(
            "SELECT evidence_json,committed_epoch FROM lifecycle_evidence WHERE step_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((old, epoch)) = prior {
        let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
        if state != "completed"
            || old != supplied
            || epoch > ledger.epoch
            || epoch
                <= p.execution
                    .as_ref()
                    .ok_or(LifecycleError::Conflict)?
                    .expected_epoch
                    + 1
        {
            return Err(LifecycleError::Conflict);
        }
        return Ok(());
    }
    if state != "armed" {
        return Err(LifecycleError::Conflict);
    }
    // ADR 0013 §5: only this instance's leases and effects are in the way.
    let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?3) OR EXISTS(SELECT 1 FROM lifecycle_steps s JOIN runtime_bindings b ON b.id=s.binding_id WHERE s.deployment_id=?1 AND b.instance_index=?3 AND s.id!=?2 AND s.state IN ('armed','uncertain'))",params![p.deployment_id,id,p.instance_index],|r|r.get(0))?;
    if pending {
        return Err(LifecycleError::Conflict);
    }
    let policy = policy(tx, &e)?;
    if ttl != policy.controls.observation_ttl_ms {
        return Err(LifecycleError::Invalid);
    }
    let context = p.context(&e)?;
    fresh(
        context.issued_at_ms,
        context.deadline_ms,
        association.observed_at_ms,
        now,
        ttl,
    )?;
    verify_completion(
        &CompletionExpectation {
            token: context.token,
            identities: members(&association.identities)?,
            target: context.completion_target.ok_or(LifecycleError::Conflict)?,
            issued_at_ms: context.issued_at_ms,
            deadline_ms: context.deadline_ms,
        },
        evidence,
        now,
        ttl,
    )
    .map_err(resource)?;
    let epoch = resource_ledger::advance_completion_epoch(tx)?;
    one(tx.execute(
        "UPDATE resource_owners SET footprint_json=?2 WHERE owner_id=?1",
        params![
            p.owner(),
            resource_ledger::encode(&phase(&e.resources.ready, ResourcePhase::Ready))
                .map_err(resource)?
        ],
    )?)?;
    one(tx.execute(
        "UPDATE runtime_bindings SET state='live' WHERE id=?1 AND state='uncertain'",
        [&p.binding_id],
    )?)?;
    one(tx.execute("UPDATE deployment_instances SET observed_state='ready',dispatch_enabled=1 WHERE deployment_id=?1 AND revision=?2 AND generation=?3 AND desired_state='ready' AND NOT EXISTS(SELECT 1 FROM deployments d WHERE d.id=?1 AND d.suspended=1)",params![p.deployment_id,p.revision,p.generation])?)?;
    one(tx.execute(
        "UPDATE lifecycle_steps SET state='completed' WHERE id=?1 AND state='armed'",
        [id],
    )?)?;
    one(tx.execute(
        "UPDATE lifecycle_runs SET state='succeeded' WHERE operation_id=?1 AND state='running'",
        [&p.operation_id],
    )?)?;
    one(tx.execute(
        "UPDATE operations SET state='succeeded' WHERE id=?1 AND state='running'",
        [&p.operation_id],
    )?)?;
    tx.execute(
        "INSERT INTO lifecycle_evidence(step_id,evidence_json,committed_epoch) VALUES(?1,?2,?3)",
        params![id, supplied, epoch],
    )?;
    one(tx.execute(
        "DELETE FROM lifecycle_claims WHERE operation_id=?1",
        [&p.operation_id],
    )?)?;
    event(tx, s, &p, Transition::Ready, Some(epoch))
}

use crate::events::LifecycleTransition as Transition;
fn event(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    p: &Plan,
    transition: Transition,
    epoch: Option<u64>,
) -> Result<(), LifecycleError> {
    use crate::events::{append_event, EventMetadata, EventOperationId};
    let id = |s: &str| {
        s.parse()
            .map(EventOperationId::generated)
            .map_err(|_| LifecycleError::CorruptStoredData)
    };
    append_event(
        tx,
        &EventMetadata::LifecycleRecorded {
            transition,
            operation_id: id(&p.operation_id)?,
            deployment_id: id(&p.deployment_id)?,
            step_id: id(&p.step_id)?,
            session_epoch: s.epoch(),
            committed_epoch: epoch,
        },
    )
    .map(|_| ())
    .map_err(|error| match error {
        crate::events::EventWriteError::Sql(error) => LifecycleError::Sql(error),
        _ => LifecycleError::CorruptStoredData,
    })
}

#[cfg(test)]
mod tests;

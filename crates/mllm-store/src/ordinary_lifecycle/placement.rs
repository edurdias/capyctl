//! ADR 0013 §4, §9 (unit I2): placing an instance and accepting its start.
//!
//! Placement runs in the same transaction that accepts the start: the host is
//! chosen from the allowed hosts the revision resolved on that are eligible
//! now, against the durable ledger as each host's admission sees it plus every
//! start accepted but not yet armed there, so two placements in one command do
//! not both count the same headroom. The chosen host, its devices and the
//! instance's generation are written with the start. ADR 0007's full check —
//! fresh observations and the ledger epoch — still runs when the start arms,
//! against exactly the host chosen here; nothing is ever placed over budget.
//!
//! Owner decision Q5: an explicit start targets every instance; an on-demand
//! start brings up one instance (the lowest-index one the operator did not
//! stop that fits) and starts the rest only where they fit, never queueing
//! them. Nothing here evicts anything: with no automatic switching yet (W10),
//! an instance that fits nowhere is left for its deadline with a capacity
//! diagnostic.
use super::*;
use crate::instances::{draw_generation, instance_owner_id};
use crate::resource_ledger::{read_snapshot, scoped_to_domain_hosts};
use mllm_config::instances::{Placement, PlacementStrategy};
use mllm_domain::resources::MemoryLimit;
use mllm_scheduler::placement::{place, HostCandidate, Strategy};
use std::collections::BTreeSet;

/// Which hosts are eligible now (W12: a live reconciled session, approved
/// configuration and a matching profile build). `None` means the caller has no
/// eligibility source (the embedded host, which is its own), and every
/// resolving host is a candidate.
pub type Eligible<'a> = Option<&'a BTreeSet<String>>;

/// What preparing one instance for a start found.
pub(crate) enum Prepared {
    /// The instance has a start in flight: join it.
    Joined(DeploymentFence),
    /// The instance holds a runtime that is not a start (Ready, stopping).
    Running,
    /// The instance was placed and fenced for a fresh start.
    Placed(DeploymentFence),
    /// No allowed host can take it now; the closed diagnostic.
    Unplaceable(&'static str),
}

fn strategy(placement: &Placement) -> Strategy {
    match placement.strategy {
        PlacementStrategy::Spread => Strategy::Spread,
        PlacementStrategy::Pack => Strategy::Pack,
    }
}

/// Every start accepted but not yet armed, as a charge on the host its
/// instance was placed on, keyed by the instance's owner id. Owner decision
/// 2026-09-23: it is charged its steady (Ready) footprint. Its startup peak is
/// serialized behind other starts on that host by the activation gate, so
/// placement only has to know the host can hold everything once Ready.
fn pending_charges(
    tx: &Transaction<'_>,
) -> Result<Vec<(String, String, PhaseFootprint)>, LifecycleError> {
    let rows: Vec<(String, String)> = tx
        .prepare(
            "SELECT s.id,s.step_json FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
              WHERE o.kind='initialize' AND s.state='planned' AND s.grant_id IS NULL",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut charges = Vec::new();
    for (_, raw) in rows {
        let plan: Plan = decode(&raw)?;
        let e = decode_effective_snapshot(&plan.effective_json)
            .map_err(|_| LifecycleError::CorruptStoredData)?;
        charges.push((
            e.host.name.clone(),
            plan.owner(),
            super::startup::steady(&e),
        ));
    }
    Ok(charges)
}

/// ADR 0013 §4 step 1: every allowed host the revision resolved on, with what
/// its admission would see.
pub(super) fn candidates(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
    instance: u32,
    eligible: Eligible<'_>,
    reclaim_parked: bool,
) -> Result<Vec<HostCandidate>, LifecycleError> {
    let hosts: Vec<(String, String)> = tx
        .prepare(
            "SELECT host_id,effective_json FROM host_effective_revisions
              WHERE deployment_id=?1 AND revision=?2 AND outcome='resolved' ORDER BY host_id",
        )?
        .query_map(params![deployment_id, revision], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<Result<_, _>>()?;
    let ledger = read_snapshot(tx).map_err(resource)?;
    let pending = pending_charges(tx)?;
    let mut candidates = Vec::new();
    for (host, raw) in hosts {
        let e = decode_effective_snapshot(&raw).map_err(|_| LifecycleError::CorruptStoredData)?;
        let Some(policy) = read_selected_policy(tx, &e.host.name).map_err(resource)? else {
            continue;
        };
        let limits: Vec<MemoryLimit> = policy
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
        let mut scoped =
            scoped_to_domain_hosts(tx, &ledger, limits.iter().map(|l| l.domain.as_str()))
                .map_err(resource)?;
        // Owner decision 2026-09-23: a launch still starting is judged at the
        // footprint it drops to at Ready; the gate orders the startup peaks.
        super::startup::steady_view(tx, &mut scoped)?;
        for (on, owner, footprint) in &pending {
            if *on == e.host.name && !scoped.owners.contains_key(owner) {
                scoped.owners.insert(owner.clone(), footprint.clone());
            }
        }
        // SPEC §6.5 (W5): parked capacity is reclaimable. When nothing fits
        // as the ledger stands, a host full only of parked instances still
        // fits; the start reclaims the least recently parked ones before it
        // arms (`park::reclaim_for_start`), each leaving the ledger only on
        // its own verified cleanup.
        if reclaim_parked {
            for owner in super::park::reclaimable_parked_owners(tx)? {
                scoped.owners.remove(&owner);
            }
        }
        let instances_here: u32 = tx.query_row(
            "SELECT COUNT(*) FROM deployment_instances i WHERE i.deployment_id=?1 AND i.instance_index!=?2 AND i.host_id=?3
               AND (EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=i.deployment_id AND b.instance_index=i.instance_index AND b.state!='released')
                    OR EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.state NOT IN ('succeeded','failed')))",
            params![deployment_id, instance, host],
            |r| r.get(0),
        )?;
        // An enrolled host whose agent holds one journal claim at a time (an
        // agent that does not advertise per-launch claims) cannot take a
        // second launch while it runs any retained one. A host advertising
        // per-launch claims (SPEC §§3.1, 7.3) co-hosts other deployments,
        // judged by the fit below and again by the host itself. One that
        // advertises only per-launch claims (journal v4) still cannot run two
        // instances of one deployment, because its journal fences each
        // deployment's commands by the highest generation it has seen, so the
        // older instance's commands would be refused as stale. A host that
        // fences per instance (journal v5, ADR 0013 §4, owner decision P1)
        // co-hosts instances of one deployment too, judged by fit alone.
        // The embedded host can run several launches of any kind.
        let occupied: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_id=?1)
                AND (EXISTS(SELECT 1 FROM remote_binding_ingress r JOIN runtime_bindings b ON b.id=r.binding_id
                            WHERE r.host_id=?1 AND b.state!='released'
                              AND NOT (b.deployment_id=?2 AND b.instance_index=?3)
                              AND (?4=0 OR (?4=1 AND b.deployment_id=?2)))
                     OR EXISTS(SELECT 1 FROM deployment_instances i JOIN runtime_bindings b
                               ON b.deployment_id=i.deployment_id AND b.instance_index=i.instance_index
                            WHERE i.host_id=?1 AND b.state!='released'
                              AND NOT (i.deployment_id=?2 AND i.instance_index=?3)
                              AND (?4=0 OR (?4=1 AND i.deployment_id=?2))))",
            params![
                host,
                deployment_id,
                instance,
                // 0: one claim at a time; 1: per launch; 2: per instance.
                tx.query_row(
                    "SELECT COALESCE(MAX(CASE mode WHEN 'per_instance' THEN 2 WHEN 'per_launch' THEN 1 ELSE 0 END),0)
                       FROM host_launch_claims WHERE host_id=?1",
                    [&host],
                    |r| r.get::<_, i64>(0),
                )?
            ],
            |r| r.get(0),
        )?;
        // Owner decision 2026-09-23: the instance's own startup peak
        // (declared, measured on this host, or the placeholder), or the whole
        // managed limit for a solo first start.
        let (footprint, whole_host) =
            super::startup::startup_footprint(tx, deployment_id, revision, &e)?;
        candidates.push(HostCandidate {
            occupied,
            whole_host,
            eligible: eligible.is_none_or(|set| set.contains(&host)),
            host_id: host,
            instances_here,
            footprint,
            limits,
            ledger: scoped,
            max_parked: policy.controls.max_parked as usize,
        });
    }
    Ok(candidates)
}

/// ADR 0013 §4, §5: get instance `instance` ready for a start: join a start in
/// flight, or place it and fence it on the deployment's current revision with
/// its own generation (drawn from the deployment's counter the first time).
pub(crate) fn prepare(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instance: u32,
    eligible: Eligible<'_>,
) -> Result<Prepared, LifecycleError> {
    type Row = (Option<i64>, Option<i64>, Option<String>, String);
    let row: Option<Row> = tx
        .query_row(
            "SELECT revision,generation,host_id,state FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2",
            params![deployment_id, instance],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let (revision, generation, last_host, lifecycle) = row.ok_or(LifecycleError::NotFound)?;
    if lifecycle != "active" {
        return Err(LifecycleError::Conflict);
    }
    let in_flight: Option<(i64, i64)> = tx
        .query_row(
            "SELECT r.revision,r.generation FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id
              WHERE r.deployment_id=?1 AND r.instance_index=?2 AND o.kind='initialize' AND r.state IN ('queued','running','uncertain')",
            params![deployment_id, instance],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((revision, generation)) = in_flight {
        return Ok(Prepared::Joined(DeploymentFence {
            deployment_id: deployment_id.into(),
            revision,
            generation,
        }));
    }
    let holds: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE deployment_id=?1 AND instance_index=?2 AND state!='released')
             OR EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND instance_index=?2 AND state NOT IN ('succeeded','failed'))",
        params![deployment_id, instance],
        |r| r.get(0),
    )?;
    if holds {
        return Ok(Prepared::Running);
    }
    let current: i64 = tx.query_row(
        "SELECT revision FROM deployments WHERE id=?1",
        [deployment_id],
        |r| r.get(0),
    )?;
    let spec = tx
        .query_row(
            "SELECT placement_json FROM deployment_revision_instances WHERE deployment_id=?1 AND revision=?2",
            params![deployment_id, current],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|json| serde_json::from_str::<Placement>(&json))
        .transpose()
        .map_err(|_| LifecycleError::CorruptStoredData)?
        .unwrap_or_default();
    let owner = instance_owner_id(deployment_id, instance);
    let hosts = candidates(tx, deployment_id, current, instance, eligible, false)?;
    let chosen = match place(
        &hosts,
        &owner,
        strategy(&spec),
        spec.max_per_host,
        last_host.as_deref(),
    ) {
        Ok(chosen) => chosen,
        // SPEC §6.5 (W5): only when nothing fits without it is parked
        // capacity counted as reclaimable.
        Err(unplaceable) => {
            let hosts = candidates(tx, deployment_id, current, instance, eligible, true)?;
            match place(
                &hosts,
                &owner,
                strategy(&spec),
                spec.max_per_host,
                last_host.as_deref(),
            ) {
                Ok(chosen) => chosen,
                Err(_) => return Ok(Prepared::Unplaceable(unplaceable.code())),
            }
        }
    };
    let (raw, _) = frozen_on_host(tx, deployment_id, current, Some(&chosen.host_id))?;
    let e = decode_effective_snapshot(&raw).map_err(|_| LifecycleError::CorruptStoredData)?;
    let devices = serde_json::to_string(&e.selected_devices)
        .map_err(|_| LifecycleError::CorruptStoredData)?;
    // A stopped instance keeps the generation its stop moved it to on the
    // same host. One that never ran, or that moves to another host, draws a
    // fresh one from the deployment's counter: a host agent fences each
    // deployment's commands by the highest generation it has seen, so an
    // older generation arriving at a new host would be refused as stale.
    // ADR 0013 §5 (I2 hazard): the same holds on its own host when that host
    // fences per deployment (an enrolled host without per-instance fencing)
    // and another instance of the deployment has drawn a newer generation,
    // which that host may have seen. A host that fences per instance compares
    // only this instance's own, monotonic, generations.
    let _ = revision;
    let per_deployment_fence: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_id=?1)
            AND NOT EXISTS(SELECT 1 FROM host_launch_claims WHERE host_id=?1 AND mode='per_instance')",
        [&chosen.host_id],
        |r| r.get(0),
    )?;
    let newest: i64 = tx.query_row(
        "SELECT MAX(current_generation,COALESCE((SELECT MAX(generation) FROM deployment_instances WHERE deployment_id=?1),0)) FROM deployments WHERE id=?1",
        [deployment_id],
        |r| r.get(0),
    )?;
    let generation = match generation {
        Some(generation)
            if last_host
                .as_deref()
                .is_none_or(|last| last == chosen.host_id.as_str())
                && !(per_deployment_fence && generation < newest) =>
        {
            generation
        }
        _ => draw_generation(tx, deployment_id)?,
    };
    one(tx.execute(
        "UPDATE deployment_instances SET revision=?3,generation=?4,host_id=?5,device_json=?6,
                placed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE deployment_id=?1 AND instance_index=?2",
        params![
            deployment_id,
            instance,
            current,
            generation,
            chosen.host_id,
            devices
        ],
    )?)?;
    Ok(Prepared::Placed(DeploymentFence {
        deployment_id: deployment_id.into(),
        revision: current,
        generation,
    }))
}

/// Record a capacity diagnostic, and keep the start pending until `until`.
pub(crate) fn defer(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instance: u32,
    code: &str,
    until: Option<i64>,
) -> Result<(), LifecycleError> {
    tx.execute(
        "UPDATE deployment_instances SET last_error=?3,
                pending_start_until_ms=CASE WHEN ?4 IS NULL THEN pending_start_until_ms ELSE ?4 END
          WHERE deployment_id=?1 AND instance_index=?2",
        params![deployment_id, instance, format!("placement: {code}"), until],
    )?;
    Ok(())
}

/// Which instances a start command targets (owner decision Q5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartScope {
    /// `start deployment`: every active instance; one that fits nowhere now
    /// stays pending until the command's deadline.
    All,
    /// On-demand activation: one instance the operator did not stop, then the
    /// rest only where they fit now.
    OnDemand,
    /// `start instance <n>`.
    Instance(u32),
}

/// The active instance indices of a deployment, lowest first.
fn active(
    tx: &Transaction<'_>,
    deployment_id: &str,
    stoppable: bool,
) -> Result<Vec<u32>, LifecycleError> {
    Ok(tx
        .prepare(
            "SELECT instance_index FROM deployment_instances WHERE deployment_id=?1 AND state='active'
               AND (?2=0 OR operator_stopped=0) ORDER BY instance_index",
        )?
        .query_map(params![deployment_id, stoppable], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

impl crate::Store {
    /// Accept the start of the instances `scope` names in one transaction and
    /// return the start a caller observes: the first instance's, joined or new.
    /// Starts of the other instances are their own operations, accepted in the
    /// same transaction. `Err(CapacityBlocked)` when nothing could be started
    /// because no allowed host fits.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn accept_scoped_start_in_transaction(
        tx: &Transaction<'_>,
        s: &CoordinatorSession,
        deployment_id: &str,
        scope: StartScope,
        now: i64,
        deadline: i64,
        eligible: Eligible<'_>,
    ) -> Result<Start, LifecycleError> {
        let targets = match scope {
            StartScope::All => active(tx, deployment_id, false)?,
            StartScope::OnDemand => active(tx, deployment_id, true)?,
            StartScope::Instance(k) => vec![k],
        };
        if targets.is_empty() {
            return Err(LifecycleError::Disabled);
        }
        let mut first: Option<Start> = None;
        let mut refusal: Option<LifecycleError> = None;
        let mut blocked = false;
        // Owner decision 2026-09-23: whether every instance that fit nowhere
        // was refused only because its solo first start needs an empty host.
        let mut needs_empty_host = true;
        for instance in targets {
            // Q5: on demand, one instance is brought up; the rest only fill in
            // where they fit, so a failure to fit is not queued for them.
            let filling = scope == StartScope::OnDemand && first.is_some();
            // Each instance's placement and start commit together or not at
            // all: a refused start undoes what placing it wrote (its host,
            // devices and any freshly drawn generation), while the other
            // instances' starts in this command stand.
            tx.execute_batch("SAVEPOINT scoped_start_instance")?;
            let prepared = match prepare(tx, deployment_id, instance, eligible) {
                Ok(prepared) => prepared,
                Err(error) => {
                    undo_instance(tx)?;
                    return Err(error);
                }
            };
            let mut undo = false;
            match prepared {
                Prepared::Joined(fence) => {
                    match Self::accept_start_in_transaction(tx, s, &fence, now, deadline, true) {
                        Ok(start) => {
                            first.get_or_insert(start);
                        }
                        Err(error) => {
                            undo = true;
                            refusal.get_or_insert(error);
                        }
                    }
                }
                Prepared::Running => {
                    refusal.get_or_insert(LifecycleError::RuntimeRetained);
                }
                Prepared::Placed(fence) => {
                    match Self::accept_start_in_transaction(tx, s, &fence, now, deadline, true) {
                        Ok(start) => {
                            first.get_or_insert(start);
                        }
                        Err(error) if filling => {
                            undo = true;
                            let _ = error;
                        }
                        Err(error) => {
                            undo = true;
                            refusal.get_or_insert(error);
                        }
                    }
                }
                Prepared::Unplaceable(code) => {
                    blocked = true;
                    needs_empty_host &= code == "startup_requires_empty_host";
                    let until = (scope != StartScope::OnDemand).then_some(deadline);
                    if !filling {
                        defer(tx, deployment_id, instance, code, until)?;
                    }
                }
            }
            if undo {
                undo_instance(tx)?;
            } else {
                tx.execute_batch("RELEASE scoped_start_instance")?;
            }
            if scope == StartScope::OnDemand && first.is_none() && refusal.is_some() {
                // An instance whose start is refused for its own reason does
                // not stop another from being chosen.
                refusal = None;
            }
        }
        match first {
            Some(start) => Ok(start),
            None if blocked && refusal.is_none() && needs_empty_host => {
                Err(LifecycleError::StartupRequiresEmptyHost)
            }
            None if blocked && refusal.is_none() => Err(LifecycleError::CapacityBlocked),
            None => Err(refusal.unwrap_or(LifecycleError::CapacityBlocked)),
        }
    }
}

/// Undo and end the savepoint one instance's placement and start ran under.
fn undo_instance(tx: &Transaction<'_>) -> Result<(), LifecycleError> {
    tx.execute_batch("ROLLBACK TO scoped_start_instance; RELEASE scoped_start_instance")?;
    Ok(())
}

/// A reconciliation start's deadline: the pending deadline, bounded by the
/// revision's own request deadline as resolved on the chosen host.
pub(crate) fn bounded_deadline(
    tx: &Transaction<'_>,
    fence: &DeploymentFence,
    now: i64,
    deadline: i64,
) -> Result<i64, LifecycleError> {
    let (_, e) = effective(tx, fence)?;
    Ok(deadline.min(now.saturating_add(e.request_deadline_ms)))
}

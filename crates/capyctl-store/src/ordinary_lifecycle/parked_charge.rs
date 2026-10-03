//! ADR 0014 amendment A13: the parked charge measured per revision.
//!
//! A parked engine keeps part of its device memory: the CUDA context, the
//! allocator's buffers and, for SGLang, the CUDA graphs its park does not
//! release (found live 2026-10-03 on a 16 GB discrete GPU: 1.57 GiB with
//! graphs, 0.88 GiB without, against the 1 GiB placeholder). Until measured,
//! the parked phase charges the placeholder its revision was resolved with.
//!
//! Once a park of the revision completes, the coordinator samples the host
//! and the memory the parked processes hold (matched by the identities this
//! store recorded, as for resident floors) is recorded per revision, host,
//! engine installation and memory domain. Only the domains that hold the
//! engine's device memory are measured (a `device` domain on a discrete host,
//! the pool on a unified one); the host RAM charged on a discrete host's
//! system domain keeps its own placeholder. The largest residue recorded is
//! kept, as for startup peaks.
//!
//! A park of that revision there is then charged the measured residue, never
//! less than the placeholder (the host re-checks co-residence against the
//! revision's own parked phase, so the controller never charges less than the
//! host) and never more than the Ready charge on that domain. Owners already
//! parked are moved to the new charge when it is recorded, so the ledger
//! always holds the footprint this module computes. A deployment that declares
//! its `resources:` keeps its declared parked phase.
use super::*;
use capyctl_config::effective::DomainMemory;
use capyctl_domain::launch::SettingSource;
use capyctl_domain::resources::{MemoryObservation, ProcessResident};
use std::collections::BTreeMap;

/// Measured parked bytes, by memory domain.
pub(super) type Measured = BTreeMap<String, i64>;

/// Whether a measurement may replace this revision's parked phase: a parking
/// revision whose resources CapyCTL derived.
fn measurable(e: &EffectiveDeployment) -> bool {
    super::park::parks(e)
        && e.engine_config.provenance().get("resources") == Some(&SettingSource::Derived)
}

/// The residue recorded for this revision on its host and installation.
pub(super) fn measured(
    conn: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    e: &EffectiveDeployment,
) -> Result<Measured, LifecycleError> {
    if !measurable(e) {
        return Ok(Measured::new());
    }
    Ok(conn
        .prepare(
            "SELECT domain,bytes FROM parked_measurements
              WHERE deployment_id=?1 AND revision=?2 AND host_id=?3 AND installation=?4",
        )?
        .query_map(
            params![
                deployment_id,
                revision,
                e.host.name,
                super::startup::installation(e)
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )?
        .collect::<Result<_, _>>()?)
}

/// `parked` with each measured domain charged its residue: never below the
/// placeholder there, never above the Ready charge on that domain.
pub(super) fn apply(
    parked: &PhaseFootprint,
    ready: &PhaseFootprint,
    measured: &Measured,
) -> PhaseFootprint {
    let mut out = parked.clone();
    for allocation in &mut out.allocations {
        let Some(&bytes) = measured.get(&allocation.domain) else {
            continue;
        };
        let ceiling = ready
            .allocations
            .iter()
            .find(|a| a.domain == allocation.domain)
            .map_or(allocation.bytes, |a| a.bytes)
            .max(allocation.bytes);
        allocation.bytes = bytes.clamp(allocation.bytes, ceiling);
    }
    out
}

/// The parked charge of the current revision, for status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ParkedStatus {
    /// What a park of the current revision on its host is charged on the
    /// domains that hold the engine's device memory.
    pub bytes: i64,
    /// `measured` once a residue was recorded there, else `placeholder`.
    pub provenance: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub measured: Vec<ParkedMeasurement>,
}

/// One recorded parked residue, for status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ParkedMeasurement {
    pub host_id: String,
    pub installation: String,
    pub domain: String,
    pub bytes: i64,
    pub measured_at_ms: i64,
}

/// The kind of each domain the revision's host declares.
fn kind(e: &EffectiveDeployment, domain: &str) -> Option<DomainMemory> {
    e.host.domains.get(domain).map(|d| d.memory)
}

/// Whether an allocation on `domain` holds the engine's device memory.
fn device_memory(e: &EffectiveDeployment, domain: &str) -> bool {
    matches!(
        kind(e, domain),
        Some(DomainMemory::Device | DomainMemory::Unified)
    )
}

/// The current revision's parked charge and its measurements. `None` for a
/// deployment whose parked phase is not measured (restart-only, declared
/// resources) or whose revision does not decode.
pub(crate) fn status(
    conn: &rusqlite::Connection,
    deployment_id: &str,
) -> rusqlite::Result<Option<ParkedStatus>> {
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT e.effective_json,d.revision FROM deployments d JOIN effective_revisions e
              ON e.deployment_id=d.id AND e.revision=d.revision WHERE d.id=?1",
            [deployment_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((raw, revision)) = row else {
        return Ok(None);
    };
    let Ok(e) = decode_effective_snapshot(&raw) else {
        return Ok(None);
    };
    if !measurable(&e) {
        return Ok(None);
    }
    let Ok(here) = measured(conn, deployment_id, revision, &e) else {
        return Ok(None);
    };
    let parked = apply(
        &phase(&e.resources.parked, ResourcePhase::Parked),
        &phase(&e.resources.ready, ResourcePhase::Ready),
        &here,
    );
    let bytes = parked
        .allocations
        .iter()
        .filter(|a| device_memory(&e, &a.domain))
        .fold(0_i64, |sum, a| sum.saturating_add(a.bytes));
    let measured = conn
        .prepare(
            "SELECT host_id,installation,domain,bytes,measured_at_ms FROM parked_measurements
              WHERE deployment_id=?1 AND revision=?2 ORDER BY host_id,installation,domain",
        )?
        .query_map(params![deployment_id, revision], |r| {
            Ok(ParkedMeasurement {
                host_id: r.get(0)?,
                installation: r.get(1)?,
                domain: r.get(2)?,
                bytes: r.get(3)?,
                measured_at_ms: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(ParkedStatus {
        bytes,
        provenance: if here.is_empty() {
            "placeholder"
        } else {
            "measured"
        },
        measured,
    }))
}

impl crate::Store {
    /// ADR 0014 amendment A13: record what the processes of a parked launch
    /// held in a sample the host took after its park completed (`since_ms`),
    /// and move every owner of that revision parked on the host to the new
    /// charge. Returns whether a residue was recorded: nothing is when the
    /// park did not complete, the instance is no longer parked, the revision's
    /// parked phase is declared, a measured domain's sample is older than
    /// `since_ms`, or the sample names none of the launch's processes.
    pub fn record_parked_residue(
        &self,
        s: &CoordinatorSession,
        step_id: &str,
        observations: &[MemoryObservation],
        residents: &[ProcessResident],
        since_ms: i64,
        now_ms: i64,
    ) -> Result<bool, LifecycleError> {
        if now_ms < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let Some(park) = super::park::completed_park(&tx, step_id)? else {
            return Ok(false);
        };
        let Ok((p, e, identities)) = super::park::launch(&tx, &park.deployment_id, park.instance)
        else {
            return Ok(false);
        };
        let still_parked: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM deployment_instances WHERE deployment_id=?1
                AND instance_index=?2 AND generation=?3 AND observed_state='parked')",
            params![park.deployment_id, park.instance, park.generation],
            |r| r.get(0),
        )?;
        if !still_parked || p.binding_id != park.binding_id || !measurable(&e) {
            return Ok(false);
        }
        let processes: Vec<&ProcessResident> = residents
            .iter()
            .filter(|r| {
                identities.iter().any(|i| {
                    i.pid == r.pid && i.boot_id == r.boot_id && i.start_ticks == r.start_ticks
                })
            })
            .collect();
        if processes.is_empty() {
            return Ok(false);
        }
        let mut residue = Measured::new();
        for allocation in &e.resources.parked.allocations {
            let Some(kind) = kind(&e, &allocation.domain) else {
                continue;
            };
            if !matches!(kind, DomainMemory::Device | DomainMemory::Unified) {
                continue;
            }
            let fresh = observations
                .iter()
                .any(|o| o.domain == allocation.domain && o.sampled_at_ms >= since_ms);
            if !fresh {
                continue;
            }
            let bytes = processes.iter().try_fold(0_i64, |sum, r| {
                sum.checked_add(match kind {
                    DomainMemory::Device => r.device_bytes,
                    _ => r.bytes,
                })
            });
            if let Some(bytes) = bytes.filter(|b| *b > 0) {
                residue.insert(allocation.domain.clone(), bytes);
            }
        }
        if residue.is_empty() {
            return Ok(false);
        }
        // Read before the measurement changes what a parked launch of this
        // revision is charged: until the ledger is moved below, a launch whose
        // ledger entry no longer matches its footprints does not load.
        let parked = parked_launches(&tx, &p.deployment_id)?;
        let installation = super::startup::installation(&e);
        for (domain, bytes) in &residue {
            tx.execute(
                "INSERT INTO parked_measurements(deployment_id,revision,host_id,installation,domain,bytes,step_id,measured_at_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(deployment_id,revision,host_id,installation,domain) DO UPDATE SET
                   step_id=CASE WHEN excluded.bytes>bytes THEN excluded.step_id ELSE step_id END,
                   measured_at_ms=CASE WHEN excluded.bytes>bytes THEN excluded.measured_at_ms ELSE measured_at_ms END,
                   bytes=MAX(bytes,excluded.bytes)",
                params![p.deployment_id, p.revision, e.host.name, installation, domain, bytes, step_id, now_ms],
            )?;
        }
        move_parked_owners(&tx, parked)?;
        tx.commit()?;
        Ok(true)
    }
}

/// The launches of `deployment` that are parked now.
fn parked_launches(
    tx: &Transaction<'_>,
    deployment: &str,
) -> Result<Vec<(Plan, EffectiveDeployment)>, LifecycleError> {
    let instances: Vec<u32> = tx
        .prepare(
            "SELECT instance_index FROM deployment_instances
              WHERE deployment_id=?1 AND observed_state='parked' ORDER BY instance_index",
        )?
        .query_map([deployment], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(instances
        .into_iter()
        .filter_map(|instance| super::park::launch(tx, deployment, instance).ok())
        .map(|(p, e, _)| (p, e))
        .collect())
}

/// Charge every parked launch the parked footprint its revision has now. An
/// owner in any other phase is left alone: a park or a wake in flight holds
/// its transition peak, which the parked residue never exceeds (it is capped
/// at the Ready charge).
fn move_parked_owners(
    tx: &Transaction<'_>,
    parked: Vec<(Plan, EffectiveDeployment)>,
) -> Result<(), LifecycleError> {
    let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
    let mut changed = false;
    for (p, e) in parked {
        let owner = p.owner();
        let Some(held) = ledger.owners.get(&owner) else {
            continue;
        };
        if held.phase != ResourcePhase::Parked {
            continue;
        }
        let next = super::park::charged_footprints(tx, &p, &e)?.parked;
        let next = resource_ledger::encode(&next).map_err(resource)?;
        if resource_ledger::encode(held).ok().as_ref() == Some(&next) {
            continue;
        }
        tx.execute(
            "UPDATE resource_owners SET footprint_json=?2 WHERE owner_id=?1",
            params![owner, next],
        )?;
        changed = true;
    }
    if changed {
        resource_ledger::advance_completion_epoch(tx)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "parked_charge_tests.rs"]
mod tests;

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
//!
//! ADR 0014 amendment A19: each measured park is also recorded for its launch
//! (an instance's generation), its first charge kept beside its latest. When
//! the latest has grown past the first by more than the host's
//! `resource_policy.parked_growth_limit`, the launch's next park is a stop
//! (`outgrown`), so its next activation starts a fresh engine.
use super::*;
use capyctl_config::effective::{DomainMemory, ParkedGrowthLimit};
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
    /// ADR 0014 amendment A19: each instance's latest measured launch, its
    /// first and latest parked charge per domain, against its host's
    /// `parked_growth_limit`. Additive; absent until a launch is measured.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub growth: Vec<ParkedGrowth>,
}

/// ADR 0014 amendment A19: one launch's parked-charge growth on one domain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ParkedGrowth {
    pub instance: u32,
    pub generation: i64,
    pub domain: String,
    /// The charge its first measured park was given.
    pub first_bytes: i64,
    /// The charge its latest measured park was given.
    pub last_bytes: i64,
    /// How many of its parks were measured.
    pub parks: i64,
    /// The growth past `first_bytes` its host allows; absent when the
    /// host's bound is `off`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<i64>,
    /// `within_limit`; `past_limit` (its next park is a stop, reason
    /// `parked_growth`); or `stopped` (the bound turned a park of it into the
    /// stop `stop_operation_id`, so its next activation starts fresh).
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_operation_id: Option<String>,
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
    conn: &Transaction<'_>,
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
    let growth = growth_status(conn, deployment_id, &e.host.name)?;
    Ok(Some(ParkedStatus {
        bytes,
        provenance: if here.is_empty() {
            "placeholder"
        } else {
            "measured"
        },
        measured,
        growth,
    }))
}

/// ADR 0014 amendment A19: the bound the host `host` states (`auto` when it
/// has published no policy).
fn growth_limit(
    tx: &Transaction<'_>,
    host: &str,
) -> Result<ParkedGrowthLimit, crate::resource_policy::ResourcePolicyError> {
    Ok(read_selected_policy(tx, host)?
        .map(|policy| policy.controls.parked_growth_limit)
        .unwrap_or_default())
}

/// One launch's recorded parked charges on one domain.
struct LaunchCharge {
    domain: String,
    first: i64,
    last: i64,
}

/// Whether `last` grew past `first` by more than `limit` allows; `None` when
/// the bound is off.
fn past(limit: ParkedGrowthLimit, first: i64, last: i64) -> Option<(bool, i64)> {
    let bound = limit.bound(first)?;
    Some((last.saturating_sub(first) > bound, bound))
}

/// A launch whose parked charge grew past its host's bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Growth {
    pub(super) domain: String,
    pub(super) first_bytes: i64,
    pub(super) last_bytes: i64,
    pub(super) bound_bytes: i64,
}

impl Growth {
    /// The evidence a stop the bound decided records.
    pub(super) fn reason(&self) -> String {
        const GIB: f64 = (1u64 << 30) as f64;
        format!(
            "its parked charge on {} grew from {:.1} GiB at its first park to {:.1} GiB, \
             past the {:.1} GiB of growth the host's parked_growth_limit allows",
            self.domain,
            self.first_bytes as f64 / GIB,
            self.last_bytes as f64 / GIB,
            self.bound_bytes as f64 / GIB,
        )
    }
}

/// ADR 0014 amendment A19: whether the launch `p` (resolved as `e`) has a
/// parked charge that grew past its host's `parked_growth_limit` since its
/// first measured park. Its next park is then a stop, which releases nothing
/// before its own cleanup evidence (AGENTS.md: uncertainty retains
/// accounting), so its next activation starts a fresh engine.
pub(super) fn outgrown(
    tx: &Transaction<'_>,
    p: &Plan,
    e: &EffectiveDeployment,
) -> Result<Option<Growth>, LifecycleError> {
    let charges = launch_charges(tx, &p.deployment_id, p.instance_index, p.generation)?;
    if charges.is_empty() {
        return Ok(None);
    }
    let limit = growth_limit(tx, &e.host.name).map_err(resource)?;
    Ok(charges.into_iter().find_map(|c| {
        let (over, bound) = past(limit, c.first, c.last)?;
        over.then_some(Growth {
            domain: c.domain,
            first_bytes: c.first,
            last_bytes: c.last,
            bound_bytes: bound,
        })
    }))
}

/// Record that the bound turned a park of the launch `p` into the stop
/// `operation`, for status.
pub(super) fn mark_stopped(
    tx: &Transaction<'_>,
    p: &Plan,
    operation: &str,
) -> Result<(), LifecycleError> {
    tx.execute(
        "UPDATE parked_launch_residues SET stop_operation_id=?4
          WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
        params![p.deployment_id, p.instance_index, p.generation, operation],
    )?;
    Ok(())
}

fn launch_charges(
    tx: &Transaction<'_>,
    deployment: &str,
    instance: u32,
    generation: i64,
) -> Result<Vec<LaunchCharge>, LifecycleError> {
    Ok(tx
        .prepare(
            "SELECT domain,first_bytes,last_bytes FROM parked_launch_residues
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 ORDER BY domain",
        )?
        .query_map(params![deployment, instance, generation], |r| {
            Ok(LaunchCharge {
                domain: r.get(0)?,
                first: r.get(1)?,
                last: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// Each instance's latest measured launch against its host's bound.
fn growth_status(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision_host: &str,
) -> rusqlite::Result<Vec<ParkedGrowth>> {
    type Row = (
        u32,
        i64,
        String,
        i64,
        i64,
        i64,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = tx
        .prepare(
            "SELECT r.instance_index,r.generation,r.domain,r.first_bytes,r.last_bytes,r.parks,
                    r.stop_operation_id,i.host_id
               FROM parked_launch_residues r LEFT JOIN deployment_instances i
                 ON i.deployment_id=r.deployment_id AND i.instance_index=r.instance_index
              WHERE r.deployment_id=?1 ORDER BY r.instance_index,r.domain",
        )?
        .query_map([deployment_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .map(
            |(instance, generation, domain, first, last, parks, stop, host)| {
                let limit =
                    growth_limit(tx, host.as_deref().unwrap_or(revision_host)).unwrap_or_default();
                let verdict = past(limit, first, last);
                ParkedGrowth {
                    instance,
                    generation,
                    domain,
                    first_bytes: first,
                    last_bytes: last,
                    parks,
                    limit_bytes: verdict.map(|(_, bound)| bound),
                    state: match (&stop, verdict) {
                        (Some(_), _) => "stopped",
                        (None, Some((true, _))) => "past_limit",
                        _ => "within_limit",
                    },
                    stop_operation_id: stop,
                }
            },
        )
        .collect())
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
        record_launch_charges(&tx, &park, &e, &residue, step_id, now_ms)?;
        move_parked_owners(&tx, parked)?;
        tx.commit()?;
        Ok(true)
    }
}

/// ADR 0014 amendment A19: record this park's charge for its launch, the
/// first one kept as the launch's baseline, and drop the instance's earlier
/// launches. A charge is the residue as a park is charged it (never below the
/// placeholder, never above the Ready charge), what status shows. A step
/// recorded twice counts once.
fn record_launch_charges(
    tx: &Transaction<'_>,
    park: &super::park::CompletedPark,
    e: &EffectiveDeployment,
    residue: &Measured,
    step_id: &str,
    now_ms: i64,
) -> Result<(), LifecycleError> {
    tx.execute(
        "DELETE FROM parked_launch_residues WHERE deployment_id=?1 AND instance_index=?2 AND generation<?3",
        params![park.deployment_id, park.instance, park.generation],
    )?;
    let charged = apply(
        &phase(&e.resources.parked, ResourcePhase::Parked),
        &phase(&e.resources.ready, ResourcePhase::Ready),
        residue,
    );
    for domain in residue.keys() {
        let Some(bytes) = charged
            .allocations
            .iter()
            .find(|a| &a.domain == domain)
            .map(|a| a.bytes)
            .filter(|bytes| *bytes > 0)
        else {
            continue;
        };
        tx.execute(
            "INSERT INTO parked_launch_residues(deployment_id,instance_index,generation,domain,
                first_bytes,first_step_id,last_bytes,last_step_id,parks,measured_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?5,?6,1,?7)
             ON CONFLICT(deployment_id,instance_index,generation,domain) DO UPDATE SET
               parks=parks+(excluded.last_step_id!=last_step_id),
               first_bytes=CASE WHEN excluded.last_step_id=first_step_id
                 THEN MAX(first_bytes,excluded.first_bytes) ELSE first_bytes END,
               last_bytes=CASE WHEN excluded.last_step_id=last_step_id
                 THEN MAX(last_bytes,excluded.last_bytes) ELSE excluded.last_bytes END,
               last_step_id=excluded.last_step_id,
               measured_at_ms=excluded.measured_at_ms",
            params![
                park.deployment_id,
                park.instance,
                park.generation,
                domain,
                bytes,
                step_id,
                now_ms
            ],
        )?;
    }
    Ok(())
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

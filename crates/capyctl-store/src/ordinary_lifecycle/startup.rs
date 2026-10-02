//! Owner decision 2026-09-23: the startup memory budget.
//!
//! A launch's memory peaks while it initializes (weights read through
//! transient buffers, kernel compilation, graph capture) and settles at its
//! steady request once Ready. Admission reserves the startup peak as the cold
//! phase from arm until Ready (ADR 0007 already distinguishes cold from
//! ready), and the Ready completion replaces it with the steady footprint.
//!
//! The peak is, in order of preference:
//!
//! 1. declared as `engine_config.memory.startup` (or as the cold phase of a
//!    declared `resources:` block), which is the operator's and never replaced;
//! 2. measured by a first run on the host: the largest drop in the host's
//!    published memory availability while an uncontended Initialize ran,
//!    recorded per (revision, host, engine installation) when it reached Ready;
//! 3. the placeholder `max(request + graphs, weights × 1.6 + margin)` the
//!    revision was resolved with (ADR 0014 amendment A8 adds the graphs) (`capyctl_config::effective::default_startup_bytes`).
//!
//! A start freezes the measured peak it will reserve into its plan when it is
//! accepted, so every later check of that start (arm, cleanup, release) sees
//! the same footprint even if a new measurement is recorded meanwhile.
//!
//! Placement judges an instance's startup peak against every other launch's
//! steady footprint: launches still in their startup phase will drop to it.
//! The per-host activation gate (`Store::startup_gate`, called from the
//! coordinator's scheduler, ADR 0015) then holds a start whose peak does not
//! fit beside the current reservations and the peaks of starts already in
//! flight on that host, until one of them reaches Ready. A start that would
//! not fit even then is not held: its arm refuses it as before. Where an
//! SGLang launch is involved, starts on one host are serialized through their
//! startup phase whatever the peaks (`sglang_startup_contended`).
use super::*;
use capyctl_config::effective::{measurable, startup_budget, StartupBudget, StartupProvenance};
use capyctl_scheduler::placement::{fits, HostRefusal};
use std::collections::{BTreeMap, BTreeSet};

/// The engine installation a measurement belongs to: the approved executable
/// and its build. A rebuilt installation measures again.
pub(crate) fn installation(e: &EffectiveDeployment) -> String {
    format!("{}@{}", e.profile.executable, e.profile.build_fingerprint)
}

fn total(footprint: &capyctl_config::effective::PhaseFootprint) -> i64 {
    footprint.allocations.iter().fold(0_i64, |sum, allocation| {
        sum.saturating_add(allocation.bytes)
    })
}

/// The largest startup peak recorded for this revision on its host and
/// installation, if any.
fn measured_peak(
    tx: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    e: &EffectiveDeployment,
) -> Result<Option<i64>, LifecycleError> {
    Ok(tx
        .query_row(
            "SELECT peak_bytes FROM startup_measurements
              WHERE deployment_id=?1 AND revision=?2 AND host_id=?3 AND installation=?4",
            params![deployment_id, revision, e.host.name, installation(e)],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .filter(|peak| *peak > 0))
}

/// The startup bytes a new start of this revision freezes into its plan: the
/// measured peak, never below the steady footprint, when the revision's own
/// budget is only a placeholder. `None` keeps the revision's cold phase.
pub(super) fn frozen_startup(
    tx: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    e: &EffectiveDeployment,
) -> Result<Option<i64>, LifecycleError> {
    if !measurable(e) {
        return Ok(None);
    }
    Ok(measured_peak(tx, deployment_id, revision, e)?
        .map(|peak| peak.max(total(&e.resources.ready))))
}

/// Owner decision 2026-09-23 (solo first start): the placeholder startup peak
/// `max(request + graphs, weights × 1.6 + margin)` recomputed with the weights recorded
/// for the revision since it was frozen, when it was frozen without them and
/// the recomputed value is larger. Found live 2026-09-23: a revision accepted
/// while its checkpoint digest was pending was frozen with the request as its
/// placeholder, so its startup estimate never exceeded the managed limit and
/// the solo first start never triggered. The weights are sized by a stat walk
/// within seconds of the deploy (`DigestCheckpoint` size-only), long before
/// the full digest. Never lowers a frozen estimate and never touches a
/// declared or measured peak.
fn weighed_placeholder(
    tx: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    e: &EffectiveDeployment,
) -> Result<Option<i64>, LifecycleError> {
    let memory = e.engine_config.memory();
    if !measurable(e) || memory.weights_bytes.is_some() {
        return Ok(None);
    }
    // The hashed weights once recorded, else the sized ones.
    let weights: Option<i64> = tx
        .query_row(
            "SELECT COALESCE(
                (SELECT weights_bytes FROM checkpoint_digests
                  WHERE deployment_id=?1 AND revision=?2 AND state='recorded'),
                (SELECT z.weights_bytes FROM checkpoint_sizes z JOIN checkpoint_digests c
                   ON c.deployment_id=z.deployment_id AND c.revision=z.revision
                  WHERE z.deployment_id=?1 AND z.revision=?2 AND c.state='pending'))",
            params![deployment_id, revision],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    // The derived cold phase carries the engine's CUDA context and graphs
    // beside the startup peak (re-review parity rule), so the recomputed
    // estimate does too.
    let Some(estimate) = weights
        .and_then(|weights| {
            capyctl_config::effective::default_startup_bytes(
                memory.request_bytes,
                Some(weights),
                memory.margin_bytes,
                memory.startup_graphs_bytes.unwrap_or(0),
            )
        })
        .and_then(|estimate| {
            estimate
                .checked_add(capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES)
        })
    else {
        return Ok(None);
    };
    Ok((estimate > total(&e.resources.cold)).then_some(estimate))
}

/// Owner decision 2026-09-23 (solo first start): the bytes an unmeasured
/// start reserves when its placeholder startup estimate exceeds the host's
/// managed limit while its steady request fits: the whole managed limit, so it
/// runs alone on its host and its peak is measured uncontended. `None` for a
/// declared or measured peak, one that fits, or one whose request cannot fit
/// even once Ready (it stays refused for capacity).
pub(super) fn whole_host_startup(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
    e: &EffectiveDeployment,
) -> Result<Option<i64>, LifecycleError> {
    if !measurable(e) || measured_peak(tx, deployment_id, revision, e)?.is_some() {
        return Ok(None);
    }
    let [allocation] = e.resources.cold.allocations.as_slice() else {
        return Ok(None);
    };
    let estimate = weighed_placeholder(tx, deployment_id, revision, e)?
        .unwrap_or(allocation.bytes)
        .max(allocation.bytes);
    let Some(policy) = read_selected_policy(tx, &e.host.name).map_err(resource)? else {
        return Ok(None);
    };
    let Some(domain) = policy.controls.domains.get(&allocation.domain) else {
        return Ok(None);
    };
    let limit = domain.managed_limit;
    Ok((estimate > limit && total(&e.resources.ready) <= limit).then_some(limit))
}

/// What a start accepted now freezes into its plan.
pub(super) struct FrozenStartup {
    /// The startup bytes it reserves: measured, the whole managed limit for a
    /// solo first start, or the weighed placeholder; `None` keeps the
    /// revision's cold phase.
    pub(super) bytes: Option<i64>,
    /// A solo first start.
    pub(super) whole_host: bool,
    /// `bytes` is the weighed placeholder, not a measurement.
    pub(super) estimated: bool,
}

/// What a start accepted now freezes into its plan: the startup bytes it
/// reserves (measured, the whole managed limit for a solo first start, or the
/// placeholder recomputed with weights sized since the freeze) and which.
pub(super) fn frozen_plan_startup(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
    e: &EffectiveDeployment,
) -> Result<FrozenStartup, LifecycleError> {
    if let Some(limit) = whole_host_startup(tx, deployment_id, revision, e)? {
        return Ok(FrozenStartup {
            bytes: Some(limit),
            whole_host: true,
            estimated: false,
        });
    }
    if let Some(bytes) = frozen_startup(tx, deployment_id, revision, e)? {
        return Ok(FrozenStartup {
            bytes: Some(bytes),
            whole_host: false,
            estimated: false,
        });
    }
    let estimate = weighed_placeholder(tx, deployment_id, revision, e)?;
    Ok(FrozenStartup {
        bytes: estimate,
        whole_host: false,
        estimated: estimate.is_some(),
    })
}

fn with_startup(e: &EffectiveDeployment, startup: Option<i64>) -> PhaseFootprint {
    let mut footprint = phase(&e.resources.cold, ResourcePhase::Cold);
    if let (Some(bytes), [allocation]) = (startup, footprint.allocations.as_mut_slice()) {
        allocation.bytes = bytes;
    }
    footprint
}

/// The cold (startup) footprint a start reserves from arm until Ready.
pub(super) fn cold(p: &Plan, e: &EffectiveDeployment) -> PhaseFootprint {
    with_startup(e, p.startup_bytes)
}

/// The startup footprint an instance of this revision would reserve if it were
/// accepted now (placement judges it before any plan exists), and whether it
/// is a solo first start that needs the whole host.
pub(super) fn startup_footprint(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
    e: &EffectiveDeployment,
) -> Result<(PhaseFootprint, bool), LifecycleError> {
    let frozen = frozen_plan_startup(tx, deployment_id, revision, e)?;
    Ok((with_startup(e, frozen.bytes), frozen.whole_host))
}

/// The footprint a launch holds once Ready.
pub(super) fn steady(e: &EffectiveDeployment) -> PhaseFootprint {
    phase(&e.resources.ready, ResourcePhase::Ready)
}

/// The steady footprint of every launch that is armed and still starting,
/// by resource owner. An uncertain start is not among them: what it holds is
/// unknown, so it keeps its full startup charge.
fn starting_owners(
    tx: &rusqlite::Connection,
) -> Result<BTreeMap<String, PhaseFootprint>, LifecycleError> {
    let rows: Vec<String> = tx
        .prepare(
            "SELECT s.step_json FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
              WHERE o.kind='initialize' AND s.state='armed'",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut owners = BTreeMap::new();
    for raw in rows {
        let plan: Plan = decode(&raw)?;
        let e = decode_effective_snapshot(&plan.effective_json)
            .map_err(|_| LifecycleError::CorruptStoredData)?;
        owners.insert(plan.owner(), steady(&e));
    }
    Ok(owners)
}

/// Charge every launch still in its startup phase its steady footprint: the
/// capacity a host offers once everything on it has reached Ready.
pub(super) fn steady_view(
    tx: &rusqlite::Connection,
    ledger: &mut capyctl_domain::resources::LedgerSnapshot,
) -> Result<(), LifecycleError> {
    for (owner, steady) in starting_owners(tx)? {
        if let Some(footprint) = ledger.owners.get_mut(&owner) {
            if footprint.phase == ResourcePhase::Cold {
                *footprint = steady;
            }
        }
    }
    Ok(())
}

/// What the per-host activation gate decided for one planned start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupGate {
    /// Dispatch it: its startup peak fits now, or waiting would not help (the
    /// arm judges it with fresh observations, as before).
    Proceed,
    /// Hold it, planned, until a start in flight on its host reaches Ready:
    /// its peak fits beside the steady footprints but not beside the peaks of
    /// the starts in flight now.
    Wait,
}

/// A start's startup reservation, for status (T14).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StartupReservation {
    pub bytes: i64,
    pub provenance: StartupProvenance,
}

fn reservation(p: &Plan, e: &EffectiveDeployment) -> StartupReservation {
    let budget = startup_budget(e);
    StartupReservation {
        bytes: total_domain(&cold(p, e)),
        provenance: if p.whole_host {
            StartupProvenance::WholeHost
        } else if p.estimated {
            StartupProvenance::Default
        } else if p.startup_bytes.is_some() {
            StartupProvenance::Measured
        } else {
            budget.provenance
        },
    }
}

fn total_domain(footprint: &PhaseFootprint) -> i64 {
    footprint.allocations.iter().fold(0_i64, |sum, allocation| {
        sum.saturating_add(allocation.bytes)
    })
}

/// One recorded startup peak, for status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StartupMeasurement {
    pub host_id: String,
    pub installation: String,
    pub peak_bytes: i64,
    pub measured_at_ms: i64,
}

/// A deployment's startup budget as status shows it: what its current
/// revision reserves before any measurement, and every peak measured for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StartupStatus {
    pub bytes: i64,
    pub provenance: StartupProvenance,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub measured: Vec<StartupMeasurement>,
}

/// The current revision's startup budget and its measurements. `None` for an
/// unknown deployment or one whose revision does not decode.
pub(crate) fn status(
    conn: &rusqlite::Connection,
    deployment_id: &str,
) -> rusqlite::Result<Option<StartupStatus>> {
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
    let StartupBudget { bytes, provenance } = startup_budget(&e);
    // The placeholder recomputed with weights sized since the freeze.
    let (bytes, provenance) = match weighed_placeholder(conn, deployment_id, revision, &e) {
        Ok(Some(estimate)) => (estimate, StartupProvenance::Default),
        _ => (bytes, provenance),
    };
    let measured = conn
        .prepare(
            "SELECT host_id,installation,peak_bytes,measured_at_ms FROM startup_measurements
              WHERE deployment_id=?1 AND revision=?2 ORDER BY host_id,installation",
        )?
        .query_map(params![deployment_id, revision], |r| {
            Ok(StartupMeasurement {
                host_id: r.get(0)?,
                installation: r.get(1)?,
                peak_bytes: r.get(2)?,
                measured_at_ms: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(Some(StartupStatus {
        bytes,
        provenance,
        measured,
    }))
}

/// Every start in flight (planned, armed or uncertain) with the startup
/// reservation it holds or will hold, by deployment and instance. A step whose
/// plan does not decode is skipped.
pub(crate) fn instance_reservations(
    conn: &rusqlite::Connection,
) -> Result<Vec<(String, u32, StartupReservation)>, LifecycleError> {
    let rows: Vec<String> = conn
        .prepare(
            "SELECT s.step_json FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
              WHERE o.kind='initialize' AND s.state IN ('planned','armed','uncertain')
              ORDER BY s.id",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    // Status is a read: a step it cannot interpret shows no reservation here
    // (its instance's derived state already reports what is known).
    let mut out = Vec::new();
    for raw in rows {
        let Ok(plan) = decode::<Plan>(&raw) else {
            continue;
        };
        let Ok(e) = decode_effective_snapshot(&plan.effective_json) else {
            continue;
        };
        out.push((
            plan.deployment_id.clone(),
            plan.instance_index,
            reservation(&plan, &e),
        ));
    }
    Ok(out)
}

fn is_sglang(e: &EffectiveDeployment) -> bool {
    e.profile.engine == capyctl_config::engine_policy::Engine::Sglang
}

/// Found live 2026-09-23 (matrix M08, host-a): SGLang sizes its static
/// pool from the device-wide free memory it profiles while it loads, so a
/// second launch allocating on the same unified host during that window makes
/// SGLang refuse to start ("Loaded weights leave no GPU memory for the KV
/// cache") even though both reservations fit. So a start on a host where an
/// SGLang launch is involved (the start itself, or another start still
/// loading there) waits until every other start in flight on that host has
/// reached Ready, whatever the peaks. Only starts already dispatched
/// (`in_flight`) or armed are waited for, so two held starts never wait on
/// each other.
fn sglang_startup_contended(
    tx: &Transaction<'_>,
    step_id: &str,
    e: &EffectiveDeployment,
    in_flight: &BTreeSet<String>,
) -> Result<bool, LifecycleError> {
    let armed: Vec<String> = tx
        .prepare(
            "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
              WHERE o.kind='initialize' AND s.state='armed'",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut others = false;
    let mut other_sglang = false;
    for other in in_flight.iter().chain(armed.iter()) {
        if other == step_id {
            continue;
        }
        let Ok((_, eq, state)) = load(tx, other) else {
            continue;
        };
        if eq.host.name == e.host.name && (state == "planned" || state == "armed") {
            others = true;
            other_sglang |= is_sglang(&eq);
        }
    }
    Ok(others && (is_sglang(e) || other_sglang))
}

impl crate::Store {
    /// Owner decision 2026-09-23, ADR 0015 (per-host activation gate): whether
    /// the planned start `step_id` may be dispatched now. `in_flight` names
    /// the Initialize steps the coordinator is driving; one that has not armed
    /// yet is charged its startup peak here as if it had. Store reads only;
    /// the arm still judges the start against fresh observations.
    pub fn startup_gate(
        &self,
        s: &CoordinatorSession,
        step_id: &str,
        in_flight: &BTreeSet<String>,
    ) -> Result<StartupGate, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let (p, e, state) = load(&tx, step_id)?;
        if state != "planned" {
            return Ok(StartupGate::Proceed);
        }
        let Ok(policy) = policy(&tx, &e) else {
            return Ok(StartupGate::Proceed);
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
        if sglang_startup_contended(&tx, step_id, &e, in_flight)? {
            return Ok(StartupGate::Wait);
        }
        let ledger = resource_ledger::read_snapshot(&tx).map_err(resource)?;
        let mut now = resource_ledger::scoped_to_domain_hosts(
            &tx,
            &ledger,
            limits.iter().map(|l| l.domain.as_str()),
        )
        .map_err(resource)?;
        let mut unarmed = BTreeMap::new();
        for other in in_flight {
            if other == step_id {
                continue;
            }
            let Ok((q, eq, other_state)) = load(&tx, other) else {
                continue;
            };
            if other_state == "planned"
                && eq.host.name == e.host.name
                && !now.owners.contains_key(&q.owner())
            {
                now.owners.insert(q.owner(), cold(&q, &eq));
                unarmed.insert(q.owner(), steady(&eq));
            }
        }
        let candidate = cold(&p, &e);
        let max_parked = policy.controls.max_parked as usize;
        match fits(&now, &p.owner(), &candidate, &limits, max_parked) {
            Ok(_) => Ok(StartupGate::Proceed),
            Err(HostRefusal::Insufficient) => {
                let mut settled = now;
                steady_view(&tx, &mut settled)?;
                for (owner, steady) in unarmed {
                    settled.owners.insert(owner, steady);
                }
                Ok(
                    if fits(&settled, &p.owner(), &candidate, &limits, max_parked).is_ok() {
                        StartupGate::Wait
                    } else {
                        StartupGate::Proceed
                    },
                )
            }
            Err(_) => Ok(StartupGate::Proceed),
        }
    }

    /// Owner decision 2026-09-23: record the startup peak an uncontended
    /// Initialize measured, once it has reached Ready. The largest peak seen
    /// for the revision on its host and installation is kept, so a later
    /// launch never reserves less than any run needed. Returns whether a
    /// measurement was written (a step that is not completed records none).
    pub fn record_startup_peak(
        &self,
        s: &CoordinatorSession,
        step_id: &str,
        peak_bytes: i64,
        now_ms: i64,
    ) -> Result<bool, LifecycleError> {
        if peak_bytes <= 0 || now_ms < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (p, e, state) = load(&tx, step_id)?;
        if state != "completed" {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO startup_measurements(deployment_id,revision,host_id,installation,peak_bytes,step_id,measured_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(deployment_id,revision,host_id,installation) DO UPDATE SET
               step_id=CASE WHEN excluded.peak_bytes>peak_bytes THEN excluded.step_id ELSE step_id END,
               measured_at_ms=CASE WHEN excluded.peak_bytes>peak_bytes THEN excluded.measured_at_ms ELSE measured_at_ms END,
               peak_bytes=MAX(peak_bytes,excluded.peak_bytes)",
            params![
                p.deployment_id,
                p.revision,
                e.host.name,
                installation(&e),
                peak_bytes,
                p.step_id,
                now_ms
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The host an instance is placed on, if it is placed.
    pub fn instance_host(
        &self,
        deployment_id: &str,
        instance: u32,
    ) -> Result<Option<String>, LifecycleError> {
        Ok(self
            .conn
            .query_row(
                "SELECT host_id FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2",
                params![deployment_id, instance],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Owner decision 2026-09-23: the startup peak recorded for a revision on
    /// a host and installation, if any.
    pub fn startup_measurement(
        &self,
        deployment_id: &str,
        revision: i64,
        host_id: &str,
    ) -> Result<Option<(String, i64)>, LifecycleError> {
        Ok(self
            .conn
            .query_row(
                "SELECT installation,peak_bytes FROM startup_measurements
                  WHERE deployment_id=?1 AND revision=?2 AND host_id=?3
                  ORDER BY measured_at_ms DESC LIMIT 1",
                params![deployment_id, revision, host_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }
}

impl worker::InitializeWork {
    /// Owner decision 2026-09-23: the startup reservation this start holds
    /// from arm until Ready, and where it came from.
    pub fn startup_reservation(&self) -> StartupReservation {
        reservation(self.plan(), self.effective())
    }
}

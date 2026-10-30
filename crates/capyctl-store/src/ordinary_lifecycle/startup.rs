//! Owner decision 2026-09-23: the startup memory budget.
//!
//! A launch's memory peaks while it initializes (weights read through
//! transient buffers, kernel compilation, graph capture) and settles at its
//! steady request once Ready. The measured peak leaves out the samples taken
//! while a kernel build ran (ADR 0014 amendment A12). Admission reserves the
//! startup peak as the cold phase from arm until Ready (ADR 0007 already distinguishes cold from
//! ready), and the Ready completion replaces it with the steady footprint.
//!
//! The peak is, in order of preference:
//!
//! 1. declared as `engine_config.memory.startup` (or as the cold phase of a
//!    declared `resources:` block), which is the operator's and never replaced;
//! 2. measured by a first run on the host, recorded per (revision, host,
//!    engine installation, memory domain) when it reached Ready. On one pool
//!    (unified memory) it is the largest drop in the host's published memory
//!    availability while an uncontended Initialize ran. On a discrete GPU
//!    (ADR 0019 §3) the card and host RAM are measured apart, from what the
//!    launch's own processes held: the GPU memory the driver attributes to
//!    them on the card, their resident pages in host RAM. Each domain's
//!    estimate is replaced once that domain is measured;
//! 3. the placeholder `max(request + graphs, weights × 2.25 + margin)` the
//!    revision was resolved with (ADR 0014 amendment A8 adds the graphs;
//!    amendments A21 and A22 size vLLM's eager loader as
//!    `min(weights × 3.6, weights + 32 GiB)` instead of `weights × 2.25`)
//!    (`capyctl_config::effective::default_startup_bytes`).
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
use capyctl_config::effective::{
    startup_budget, startup_measure, StartupMeasure, StartupOverLimit, StartupProvenance,
};
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

/// The startup peaks recorded for this revision on its host and installation,
/// by memory domain. ADR 0035 §3: at the revision's tensor-parallel size N
/// only; a peak measured at N = 2 never sizes a start at N = 1.
fn measured_peaks(
    tx: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    e: &EffectiveDeployment,
) -> Result<BTreeMap<String, i64>, LifecycleError> {
    Ok(tx
        .prepare(
            "SELECT domain,peak_bytes FROM startup_measurements
              WHERE deployment_id=?1 AND revision=?2 AND host_id=?3 AND installation=?4
                AND tensor_parallel=?5 AND peak_bytes>0",
        )?
        .query_map(
            params![
                deployment_id,
                revision,
                e.host.name,
                installation(e),
                e.local_tensor_parallel()
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )?
        .collect::<Result<_, _>>()?)
}

/// Whether `domain` is one of the host's discrete GPUs.
fn is_device(e: &EffectiveDeployment, domain: &str) -> bool {
    e.host
        .domains
        .get(domain)
        .is_some_and(|d| d.memory == capyctl_config::effective::DomainMemory::Device)
}

/// ADR 0035 §§1a, 3: the per-GPU device peak measured for this revision on
/// `e`'s host and installation at tensor-parallel size `n`, whichever GPUs it
/// was measured on (the GPUs are identical), for `topology: auto`'s choice of
/// N; `None` when none is recorded at that size.
pub(super) fn measured_device_peak(
    tx: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    e: &EffectiveDeployment,
    n: u32,
) -> Result<Option<i64>, LifecycleError> {
    let rows: Vec<(String, i64)> = tx
        .prepare(
            "SELECT domain,peak_bytes FROM startup_measurements
              WHERE deployment_id=?1 AND revision=?2 AND host_id=?3 AND installation=?4
                AND tensor_parallel=?5 AND peak_bytes>0",
        )?
        .query_map(
            params![deployment_id, revision, e.host.name, installation(e), n],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|(domain, _)| is_device(e, domain))
        .map(|(_, peak)| peak)
        .max())
}

/// The pool a single-allocation placeholder is measured on (unified memory).
fn pool(e: &EffectiveDeployment) -> Option<String> {
    match startup_measure(e) {
        Some(StartupMeasure::Pool { domain }) => Some(domain),
        _ => None,
    }
}

/// The bytes the Ready phase charges on `domain`.
fn ready_on(e: &EffectiveDeployment, domain: &str) -> i64 {
    e.resources
        .ready
        .allocations
        .iter()
        .filter(|a| a.domain == domain)
        .fold(0_i64, |sum, a| sum.saturating_add(a.bytes))
}

/// The measured startup bytes a new start of this revision freezes into its
/// plan when the revision's own budget is only a placeholder: on one pool the
/// measured peak, never below the steady footprint (`.0`); on a discrete GPU
/// each measured domain's peak, never below that domain's steady charge
/// (`.1`, ADR 0019 §3). `(None, empty)` keeps the revision's cold phase.
pub(super) fn frozen_startup(
    tx: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    e: &EffectiveDeployment,
) -> Result<(Option<i64>, BTreeMap<String, i64>), LifecycleError> {
    let Some(measure) = startup_measure(e) else {
        return Ok((None, BTreeMap::new()));
    };
    let measured = measured_peaks(tx, deployment_id, revision, e)?;
    Ok(match measure {
        StartupMeasure::Pool { domain } => (
            measured
                .get(&domain)
                .map(|peak| (*peak).max(total(&e.resources.ready))),
            BTreeMap::new(),
        ),
        StartupMeasure::Domains { device, host } => {
            // ADR 0035 §3: on N GPUs the peak is per GPU and applies to every
            // GPU of the set, whichever GPUs it was measured on; on one GPU
            // to that GPU's domain only, as before.
            let n = e.local_tensor_parallel();
            let (device_peak, devices) = if n > 1 {
                (
                    measured
                        .iter()
                        .filter(|(domain, _)| is_device(e, domain))
                        .map(|(_, peak)| *peak)
                        .max(),
                    capyctl_config::effective::startup_device_domains(e),
                )
            } else {
                (measured.get(&device).copied(), vec![device])
            };
            let mut domains = BTreeMap::new();
            if let Some(peak) = device_peak {
                for domain in devices {
                    let bytes = peak.max(ready_on(e, &domain));
                    domains.insert(domain, bytes);
                }
            }
            if let Some(peak) = measured.get(&host) {
                domains.insert(host.clone(), (*peak).max(ready_on(e, &host)));
            }
            (None, domains)
        }
    })
}

/// Owner decision 2026-09-23 (solo first start): the placeholder startup peak
/// `max(request + graphs, load + margin)` recomputed with the weights
/// recorded for the revision since it was frozen, when it was frozen without
/// them and the recomputed value is larger. The load follows ADR 0014
/// amendments A21 and A22 exactly as the original resolution picked it:
/// larger while vLLM actually gets `--safetensors-load-strategy eager`,
/// capped as the revision recorded (`startup_eager_cap_bytes`), uncapped for
/// a revision frozen before the cap. Found live 2026-09-23: a revision
/// accepted while its checkpoint digest was pending was frozen with the
/// request as its placeholder, so its startup estimate never exceeded the
/// managed limit and the solo first start never triggered. The weights are
/// sized by a stat walk within seconds of the deploy (`DigestCheckpoint`
/// size-only), long before the full digest. Never lowers a frozen estimate
/// and never touches a declared or measured peak.
fn weighed_placeholder(
    tx: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    e: &EffectiveDeployment,
) -> Result<Option<i64>, LifecycleError> {
    let memory = e.engine_config.memory();
    if pool(e).is_none() || memory.weights_bytes.is_some() {
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
    let eager = match &e.engine_config {
        capyctl_domain::launch::LaunchSettings::Vllm(settings)
            if settings.renders_eager_loader() =>
        {
            Some(match memory.startup_eager_cap_bytes {
                Some(cap) => capyctl_config::effective::EagerStartup::Capped(cap),
                None => capyctl_config::effective::EagerStartup::Uncapped,
            })
        }
        _ => None,
    };
    let Some(estimate) = weights
        .and_then(|weights| {
            capyctl_config::effective::default_startup_bytes(
                memory.request_bytes,
                Some(weights),
                memory.margin_bytes,
                memory.startup_graphs_bytes,
                eager,
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
    let Some(domain) = pool(e) else {
        return Ok(None);
    };
    if measured_peaks(tx, deployment_id, revision, e)?.contains_key(&domain) {
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
    /// The startup bytes it reserves on one pool: measured, the whole managed
    /// limit for a solo first start, or the weighed placeholder; `None` keeps
    /// the revision's cold phase.
    pub(super) bytes: Option<i64>,
    /// ADR 0019 §3: on a discrete GPU, the measured startup bytes it reserves
    /// on each measured domain; a domain absent keeps its cold charge.
    pub(super) domains: BTreeMap<String, i64>,
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
            domains: BTreeMap::new(),
            whole_host: true,
            estimated: false,
        });
    }
    let (bytes, domains) = frozen_startup(tx, deployment_id, revision, e)?;
    if bytes.is_some() || !domains.is_empty() {
        return Ok(FrozenStartup {
            bytes,
            domains,
            whole_host: false,
            estimated: false,
        });
    }
    let estimate = weighed_placeholder(tx, deployment_id, revision, e)?;
    Ok(FrozenStartup {
        bytes: estimate,
        domains: BTreeMap::new(),
        whole_host: false,
        estimated: estimate.is_some(),
    })
}

fn with_startup(
    e: &EffectiveDeployment,
    startup: Option<i64>,
    domains: &BTreeMap<String, i64>,
) -> PhaseFootprint {
    let mut footprint = phase(&e.resources.cold, ResourcePhase::Cold);
    if let (Some(bytes), [allocation]) = (startup, footprint.allocations.as_mut_slice()) {
        allocation.bytes = bytes;
    }
    for allocation in &mut footprint.allocations {
        if let Some(bytes) = domains.get(&allocation.domain) {
            allocation.bytes = *bytes;
        }
    }
    footprint
}

/// The cold (startup) footprint a start reserves from arm until Ready.
/// ADR 0033 §6 (lifecycle design §7): with the LMCache charge it was granted,
/// laid over after the startup peak so a measured engine peak never drops it.
pub(super) fn cold(
    conn: &rusqlite::Connection,
    p: &Plan,
    e: &EffectiveDeployment,
) -> Result<PhaseFootprint, LifecycleError> {
    super::lmcache::charged(
        conn,
        &p.binding_id,
        e,
        with_startup(e, p.startup_bytes, &p.startup_domains),
        super::lmcache::Charged::Planned,
    )
}

/// The startup footprint an instance of this revision would reserve if it were
/// accepted now on `host` (placement judges it before any plan exists), and
/// whether it is a solo first start that needs the whole host. ADR 0033 §6:
/// with the LMCache charge a start there would be granted.
pub(super) fn startup_footprint(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
    host: &str,
    e: &EffectiveDeployment,
) -> Result<(PhaseFootprint, bool), LifecycleError> {
    let frozen = frozen_plan_startup(tx, deployment_id, revision, e)?;
    Ok((
        super::lmcache::prospective(
            tx,
            deployment_id,
            revision,
            host,
            e,
            with_startup(e, frozen.bytes, &frozen.domains),
        )?,
        frozen.whole_host,
    ))
}

/// The footprint a launch holds once Ready, with the LMCache charge its host
/// did not report off (ADR 0033 §6).
pub(super) fn steady(
    conn: &rusqlite::Connection,
    p: &Plan,
    e: &EffectiveDeployment,
) -> Result<PhaseFootprint, LifecycleError> {
    super::lmcache::charged(
        conn,
        &p.binding_id,
        e,
        phase(&e.resources.ready, ResourcePhase::Ready),
        super::lmcache::Charged::Reported,
    )
}

/// The Ready footprint a start of this revision on `host` would hold, with
/// the LMCache charge it would be granted (no plan exists yet).
pub(super) fn prospective_steady(
    conn: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    host: &str,
    e: &EffectiveDeployment,
) -> Result<PhaseFootprint, LifecycleError> {
    super::lmcache::prospective(
        conn,
        deployment_id,
        revision,
        host,
        e,
        phase(&e.resources.ready, ResourcePhase::Ready),
    )
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
        owners.insert(plan.owner(), steady(tx, &plan, &e)?);
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
    /// ADR 0019 §3: on a discrete GPU, the card's and host RAM's parts of
    /// `bytes`, each with its own provenance. Absent on one pool.
    #[serde(flatten)]
    pub split: Option<StartupSplit>,
}

/// ADR 0019 §3 (found live 2026-10-03): on a discrete GPU the part of a
/// startup reservation charged on the card (the startup peak and the engine's
/// CUDA context) and the part charged on host RAM (the engine process), apart,
/// so the card's figure is not read as the sum, each with where it came from:
/// a domain whose peak a first run measured is `measured`, any other keeps
/// the provenance of the estimate it reserves. ADR 0035 §3: on N GPUs the
/// card's part is each GPU's (every GPU is charged one rank), which is what a
/// host lays over each of the launch's GPUs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StartupSplit {
    pub device_bytes: i64,
    pub host_bytes: i64,
    pub device_provenance: StartupProvenance,
    pub host_provenance: StartupProvenance,
}

/// `footprint` split between the card and host RAM, when any of its
/// allocations is on a device domain (a discrete GPU). `measured` names the
/// domains whose measured peak it reserves; every other domain is
/// `unmeasured`.
fn split(
    e: &EffectiveDeployment,
    footprint: &PhaseFootprint,
    measured: &BTreeMap<String, i64>,
    unmeasured: StartupProvenance,
) -> Option<StartupSplit> {
    let on_device = |domain: &str| {
        e.host
            .domains
            .get(domain)
            .is_some_and(|d| d.memory == capyctl_config::effective::DomainMemory::Device)
    };
    let cold = &footprint.allocations;
    if !cold.iter().any(|a| on_device(&a.domain)) {
        return None;
    }
    let part = |device: bool| {
        let mut allocations = cold.iter().filter(|a| on_device(&a.domain) == device);
        let bytes = if device {
            // ADR 0035 §3: one GPU's part (the largest; they are equal).
            allocations.clone().map(|a| a.bytes).max().unwrap_or(0)
        } else {
            allocations
                .clone()
                .fold(0_i64, |sum, a| sum.saturating_add(a.bytes))
        };
        let provenance = if allocations.all(|a| measured.contains_key(&a.domain)) && bytes > 0 {
            StartupProvenance::Measured
        } else {
            unmeasured
        };
        (bytes, provenance)
    };
    let ((device_bytes, device_provenance), (host_bytes, host_provenance)) =
        (part(true), part(false));
    Some(StartupSplit {
        device_bytes,
        host_bytes,
        device_provenance,
        host_provenance,
    })
}

/// A reservation's provenance on a discrete GPU is the card's, the figure
/// status shows first; elsewhere `whole`.
fn overall(whole: StartupProvenance, split: &Option<StartupSplit>) -> StartupProvenance {
    split.as_ref().map_or(whole, |s| s.device_provenance)
}

/// The engine's own startup reservation (T14); the LMCache charge beside it
/// is shown with the instance's LMCache.
fn reservation(p: &Plan, e: &EffectiveDeployment) -> StartupReservation {
    let footprint = with_startup(e, p.startup_bytes, &p.startup_domains);
    let provenance = if p.whole_host {
        StartupProvenance::WholeHost
    } else if p.estimated {
        StartupProvenance::Default
    } else if p.startup_bytes.is_some() {
        StartupProvenance::Measured
    } else {
        startup_budget(e).provenance
    };
    let split = split(e, &footprint, &p.startup_domains, provenance);
    StartupReservation {
        bytes: total_domain(&footprint),
        provenance: overall(provenance, &split),
        split,
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
    /// The memory domain it was measured on: the pool on unified memory, the
    /// card or host RAM on a discrete GPU (ADR 0019 §3).
    pub domain: String,
    pub peak_bytes: i64,
    pub measured_at_ms: i64,
    /// ADR 0035 §3: the tensor-parallel size it was measured at (the
    /// per-GPU peak of a launch on that many GPUs). Absent at 1. Additive.
    #[serde(skip_serializing_if = "one")]
    pub tensor_parallel: u32,
}

fn one(n: &u32) -> bool {
    *n == 1
}

/// A deployment's startup budget as status shows it: what a start of its
/// current revision accepted now reserves, and every peak measured for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StartupStatus {
    pub bytes: i64,
    pub provenance: StartupProvenance,
    /// ADR 0019 §3: on a discrete GPU, the card's and host RAM's parts of
    /// `bytes`, each with its own provenance. Absent on one pool.
    #[serde(flatten)]
    pub split: Option<StartupSplit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub measured: Vec<StartupMeasurement>,
    /// #219: the card whose part of `bytes` is above the managed limit its
    /// host's policy states now: such a start is never admitted, and with
    /// nothing measured the figure never drops. Absent when every card holds
    /// it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub over_limit: Option<StartupOverLimit>,
}

/// #219 (ADR 0019 §3): the first card of `footprint` charged above the
/// managed limit the host's current policy states for it. `None` when every
/// card holds its charge, and when the policy cannot be read (status is a
/// read; admission judges the start on its own).
fn over_limit(
    tx: &Transaction<'_>,
    e: &EffectiveDeployment,
    footprint: &PhaseFootprint,
) -> Option<StartupOverLimit> {
    let policy = read_selected_policy(tx, &e.host.name).ok()??;
    footprint
        .allocations
        .iter()
        .filter(|a| is_device(e, &a.domain))
        .find_map(|a| {
            let limit = policy.controls.domains.get(&a.domain)?.managed_limit;
            (a.bytes > limit).then(|| StartupOverLimit {
                domain: a.domain.clone(),
                bytes: a.bytes,
                limit_bytes: limit,
            })
        })
}

/// The current revision's startup budget and its measurements. `None` for an
/// unknown deployment or one whose revision does not decode.
pub(crate) fn status(
    tx: &Transaction<'_>,
    deployment_id: &str,
) -> rusqlite::Result<Option<StartupStatus>> {
    let row: Option<(String, i64)> = tx
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
    // Owner decision 2026-10-07 (found live: an SGLang deployment showed its
    // 55.2 GiB placeholder after a 29.7 GiB peak was measured): the figure is
    // the one admission reserves (`frozen_plan_startup`): a measured peak on
    // the revision's host and installation (per domain on a discrete GPU),
    // else the whole managed limit for a solo first start (owner decision
    // 2026-09-23; ADR 0015 amendment 2026-09-23), else the placeholder
    // recomputed with weights sized since the freeze. Found live 2026-10-16:
    // a parking vLLM deployment of 71 GiB of weights showed its 265.2 GiB
    // eager-loader placeholder while every start of it reserved the 108 GiB
    // managed limit.
    let frozen = frozen_plan_startup(tx, deployment_id, revision, &e).ok();
    let (footprint, measured_domains, provenance) = match frozen {
        Some(f) => (
            with_startup(&e, f.bytes, &f.domains),
            f.domains,
            if f.whole_host {
                StartupProvenance::WholeHost
            } else if f.estimated {
                StartupProvenance::Default
            } else if f.bytes.is_some() {
                StartupProvenance::Measured
            } else {
                startup_budget(&e).provenance
            },
        ),
        None => (
            with_startup(&e, None, &BTreeMap::new()),
            BTreeMap::new(),
            startup_budget(&e).provenance,
        ),
    };
    let measured = tx
        .prepare(
            "SELECT host_id,installation,domain,peak_bytes,measured_at_ms,tensor_parallel FROM startup_measurements
              WHERE deployment_id=?1 AND revision=?2 ORDER BY host_id,installation,tensor_parallel,domain",
        )?
        .query_map(params![deployment_id, revision], |r| {
            Ok(StartupMeasurement {
                host_id: r.get(0)?,
                installation: r.get(1)?,
                domain: r.get(2)?,
                peak_bytes: r.get(3)?,
                measured_at_ms: r.get(4)?,
                tensor_parallel: r.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let split = split(&e, &footprint, &measured_domains, provenance);
    Ok(Some(StartupStatus {
        bytes: total_domain(&footprint),
        provenance: overall(provenance, &split),
        split,
        measured,
        over_limit: over_limit(tx, &e, &footprint),
    }))
}

/// ADR 0035 §1, §1a: the current revision's topology as status and `deploy`
/// show it ([`capyctl_config::effective::topology_view`]), as resolved on its
/// first host, the size `topology: auto` would choose there now taking the
/// startup peak measured at each size into account. `None` on one GPU
/// without `auto`, and for an unknown deployment or one whose revision does
/// not decode.
pub(crate) fn topology_status(
    tx: &Transaction<'_>,
    deployment_id: &str,
) -> rusqlite::Result<Option<serde_json::Value>> {
    let row: Option<(String, i64)> = tx
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
    Ok(capyctl_config::effective::topology_view(&e, &|n| {
        measured_device_peak(tx, deployment_id, revision, &e, n)
            .ok()
            .flatten()
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
                reserve_absorbs_unmanaged: d.memory
                    == capyctl_config::effective::DomainMemory::Device,
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
                now.owners.insert(q.owner(), cold(&tx, &q, &eq)?);
                unarmed.insert(q.owner(), steady(&tx, &q, &eq)?);
            }
        }
        let candidate = cold(&tx, &p, &e)?;
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

    /// Owner decision 2026-09-23: record the startup peaks an uncontended
    /// Initialize measured, once it has reached Ready, by memory domain: the
    /// pool on unified memory, the card and host RAM apart on a discrete GPU
    /// (ADR 0019 §3). The largest peak seen for the revision on its host,
    /// installation and domain is kept, so a later launch never reserves less
    /// than any run needed. Every domain must be one its cold phase charges.
    /// Returns whether a measurement was written (a step that is not
    /// completed records none).
    pub fn record_startup_peak(
        &self,
        s: &CoordinatorSession,
        step_id: &str,
        peaks: &BTreeMap<String, i64>,
        now_ms: i64,
    ) -> Result<bool, LifecycleError> {
        if peaks.is_empty() || peaks.values().any(|peak| *peak <= 0) || now_ms < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (p, e, state) = load(&tx, step_id)?;
        if !peaks.keys().all(|domain| {
            e.resources
                .cold
                .allocations
                .iter()
                .any(|a| &a.domain == domain)
        }) {
            return Err(LifecycleError::Invalid);
        }
        if state != "completed" {
            return Ok(false);
        }
        // ADR 0035 §3: kept per revision, host, installation and N, the
        // tensor-parallel size the launch ran at.
        for (domain, peak_bytes) in peaks {
            tx.execute(
                "INSERT INTO startup_measurements(deployment_id,revision,host_id,installation,tensor_parallel,domain,peak_bytes,step_id,measured_at_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(deployment_id,revision,host_id,installation,tensor_parallel,domain) DO UPDATE SET
                   step_id=CASE WHEN excluded.peak_bytes>peak_bytes THEN excluded.step_id ELSE step_id END,
                   measured_at_ms=CASE WHEN excluded.peak_bytes>peak_bytes THEN excluded.measured_at_ms ELSE measured_at_ms END,
                   peak_bytes=MAX(peak_bytes,excluded.peak_bytes)",
                params![
                    p.deployment_id,
                    p.revision,
                    e.host.name,
                    installation(&e),
                    e.local_tensor_parallel(),
                    domain,
                    peak_bytes,
                    p.step_id,
                    now_ms
                ],
            )?;
        }
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
}

impl worker::InitializeWork {
    /// Owner decision 2026-09-23: the startup reservation this start holds
    /// from arm until Ready, and where it came from.
    pub fn startup_reservation(&self) -> StartupReservation {
        reservation(self.plan(), self.effective())
    }
}

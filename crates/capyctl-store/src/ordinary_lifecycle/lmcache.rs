//! ADR 0033 (lifecycle design §§1.6, 3, 4.1, 5, 7, 9, 10): what the store
//! plans, reserves and records for one launch's LMCache server.
//!
//! A start computes `capyctl_config::lmcache::plan` with the store's inputs
//! when it is accepted. On, it leases the server's two loopback ports from the
//! host's endpoint range (disjoint from endpoint leases and group rendezvous
//! ports, held while the binding is retained) and freezes the charge: the
//! pool, the overhead and device allowances or the revision's measured
//! figures, and the engine's KV cache a restart-parked server keeps mapped.
//! Off, it records the reason, and a reason the store knows (anything but the
//! deployment's own `disabled`) journals `lmcache_off` once. The host's report
//! (on, or off with a launch-time reason) is recorded at Ready or failure.
use super::*;
use crate::events::{lmcache_off_in, lmcache_server_exited_in, LmcacheLaunch};
use crate::lifecycle::completion::identities;
use capyctl_config::lmcache::{
    self as lc, ChargeDomains, LmcacheCharge, LmcacheMode, LmcacheOff, LmcacheOffReason, PlanInputs,
};
use capyctl_domain::completion::ProcessIdentity;

/// The charge and ports one launch's server was granted (lifecycle design
/// §10: the store's half of `SingleLaunchPlan.lmcache`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LmcacheGrant {
    pub rpc_port: u16,
    pub http_port: u16,
    pub pool_bytes: i64,
    pub overhead_bytes: i64,
    pub device_bytes: i64,
    pub held_kv_bytes: i64,
}

impl LmcacheGrant {
    /// ADR 0033 §6: the charge laid over the launch's phases.
    pub fn charge(&self) -> LmcacheCharge {
        LmcacheCharge {
            pool_bytes: self.pool_bytes,
            overhead_bytes: self.overhead_bytes,
            device_bytes: self.device_bytes,
            held_kv_bytes: self.held_kv_bytes,
        }
    }
}

/// What the store planned for one launch's LMCache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlannedLmcache {
    On(LmcacheGrant),
    Off(LmcacheOff),
}

/// One launch's LMCache as the store planned it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchLmcachePlan {
    /// The deployment's declared `lmcache` (`required` fails the launch on a
    /// launch-time reason).
    pub mode: LmcacheMode,
    /// ADR 0033 §10: the host's or the deployment's `retention: zero`
    /// (`RLIMIT_CORE` 0 and engine output in memory).
    pub retention_zero: bool,
    /// ADR 0033 §7: a `restart_only` park stops the engine and keeps the
    /// server, when the server runs.
    pub restart_park: bool,
    pub planned: PlannedLmcache,
}

impl LaunchLmcachePlan {
    pub fn grant(&self) -> Option<&LmcacheGrant> {
        match &self.planned {
            PlannedLmcache::On(grant) => Some(grant),
            PlannedLmcache::Off(_) => None,
        }
    }
    pub fn required(&self) -> bool {
        self.mode == LmcacheMode::Required
    }
}

/// What a host reported about a launch's LMCache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LmcacheOutcome {
    On,
    Off {
        reason: LmcacheOffReason,
        detail: String,
    },
}

/// ADR 0033 §8: what [`crate::Store::release_failed_launch_lmcache`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LmcacheFallback {
    /// Released, and started again with LMCache off.
    Relaunched(Start),
    /// Released and failed with this code; nothing was started again (under
    /// `required`, or a start the store refused).
    Failed(String),
}

/// ADR 0033 §§5, 12: one Ready or parked embedded launch that runs a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalLmcacheLaunch {
    pub fence: DeploymentFence,
    pub instance_index: u32,
    pub binding_id: String,
    pub step_id: String,
    pub server: Vec<ProcessIdentity>,
    pub http_port: u16,
    pub pool_bytes: i64,
    pub parked: bool,
}

/// ADR 0033 §7 (lifecycle design §4.1): how a Ready single launch parks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkMode {
    /// It does not park (a park is refused, an idle one stops).
    None,
    /// The engine sleeps (deep or host-backed).
    Engine,
    /// `restart_only` with LMCache on: the engine stops, the server stays.
    EngineStop,
}

/// Which of a launch's charges a footprint carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Charged {
    /// The charge the start reserved, whatever its host reported since: the
    /// cold phase it armed with is compared exactly until it settles.
    Planned,
    /// The charge it holds once its host reported: none when the host said
    /// LMCache was off (ADR 0033 §6: the steady charge drops it).
    Reported,
}

/// The stored row's charge, and whether the launch parks restart-only.
struct Row {
    planned_on: bool,
    pool: Option<i64>,
    overhead: Option<i64>,
    device: Option<i64>,
    held_kv: Option<i64>,
    state: Option<String>,
    restart_park: bool,
}

fn row(conn: &rusqlite::Connection, binding_id: &str) -> Result<Option<Row>, LifecycleError> {
    Ok(conn
        .query_row(
            "SELECT planned='on',pool_bytes,overhead_bytes,device_bytes,held_kv_bytes,state,restart_park=1
               FROM launch_lmcache WHERE binding_id=?1",
            [binding_id],
            |r| {
                Ok(Row {
                    planned_on: r.get(0)?,
                    pool: r.get(1)?,
                    overhead: r.get(2)?,
                    device: r.get(3)?,
                    held_kv: r.get(4)?,
                    state: r.get(5)?,
                    restart_park: r.get(6)?,
                })
            },
        )
        .optional()?)
}

/// ADR 0033 §6: the charge `binding_id` holds, and whether its parked phase
/// is a restart-park's (the server alone, with the stopped engine's KV).
pub(super) fn charge_of(
    conn: &rusqlite::Connection,
    binding_id: &str,
    which: Charged,
) -> Result<Option<(LmcacheCharge, bool)>, LifecycleError> {
    let Some(row) = row(conn, binding_id)? else {
        return Ok(None);
    };
    let reported_off = row.state.as_deref() == Some("off");
    if !row.planned_on || (which == Charged::Reported && reported_off) {
        return Ok(None);
    }
    let (Some(pool), Some(overhead), Some(device), Some(held_kv)) =
        (row.pool, row.overhead, row.device, row.held_kv)
    else {
        return Err(LifecycleError::CorruptStoredData);
    };
    Ok(Some((
        LmcacheCharge {
            pool_bytes: pool,
            overhead_bytes: overhead,
            device_bytes: device,
            held_kv_bytes: held_kv,
        },
        row.restart_park && !reported_off,
    )))
}

/// The domains a launch resolved as `e` is charged in.
fn domains(e: &EffectiveDeployment) -> Result<ChargeDomains, LifecycleError> {
    lc::charge_domains(&e.host, &e.selected_devices).ok_or_else(|| {
        LifecycleError::Rejected("no memory domain can take the LMCache charge".into())
    })
}

/// ADR 0033 §6: `footprint` with `charge` laid over it through the one
/// overlay the host applies too (`apply_charge`). `parked` says it is the
/// parked phase; with `restart_park` that phase is the server's charge alone.
pub(super) fn lay(
    e: &EffectiveDeployment,
    footprint: &PhaseFootprint,
    charge: &LmcacheCharge,
    restart_park: bool,
    parked: bool,
) -> Result<PhaseFootprint, LifecycleError> {
    let empty = |phase| PhaseFootprint {
        phase,
        allocations: Vec::new(),
        devices: Vec::new(),
    };
    let mut phases = capyctl_domain::resources::RecipeFootprints {
        cold: empty(ResourcePhase::Cold),
        ready: empty(ResourcePhase::Ready),
        parking: empty(ResourcePhase::Parking),
        parked: empty(ResourcePhase::Parked),
        wake: empty(ResourcePhase::Wake),
    };
    let slot = if parked {
        &mut phases.parked
    } else {
        &mut phases.cold
    };
    slot.allocations = footprint.allocations.clone();
    lc::apply_charge(&mut phases, charge, restart_park, &domains(e)?);
    let mut out = footprint.clone();
    out.allocations = if parked {
        phases.parked.allocations
    } else {
        phases.cold.allocations
    };
    Ok(out)
}

/// `footprint` of `binding_id`'s launch with its LMCache charge, if any.
pub(super) fn charged(
    conn: &rusqlite::Connection,
    binding_id: &str,
    e: &EffectiveDeployment,
    footprint: PhaseFootprint,
    which: Charged,
) -> Result<PhaseFootprint, LifecycleError> {
    match charge_of(conn, binding_id, which)? {
        Some((charge, _)) => lay(e, &footprint, &charge, false, false),
        None => Ok(footprint),
    }
}

/// v56 (ADR 0033 §§6, 7, amendment A13): add `launch_lmcache.held_kv_state`
/// unless a store rolled back from v56 or later still has it.
pub(crate) fn migrate_v56(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    let present: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('launch_lmcache') WHERE name='held_kv_state')",
        [],
        |r| r.get(0),
    )?;
    if !present {
        tx.execute_batch(
            "ALTER TABLE launch_lmcache ADD COLUMN held_kv_state TEXT
               CHECK(held_kv_state IN('parking','held'));",
        )?;
    }
    Ok(())
}

/// ADR 0033 §§6, 7, amendment A13 (found live on vLLM, 2026-10-23): whether
/// `binding_id`'s server holds the KV of an engine a restart-park stopped
/// that its Ready charge keeps after the wake. The wake's engine registering
/// does not release it: only a GPU reading of the server below that KV cache
/// does ([`release_held_kv`]).
pub(super) fn kv_held(
    conn: &rusqlite::Connection,
    binding_id: &str,
) -> Result<bool, LifecycleError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM launch_lmcache WHERE binding_id=?1 AND held_kv_state='held')",
        [binding_id],
        |r| r.get(0),
    )?)
}

/// ADR 0033 §§6, 7, amendment A13: a restart-park of `binding_id` was armed,
/// so from now on its server may hold the engine's KV: none of its GPU
/// readings is recorded as its measurement until one shows that KV released.
/// A launch already holding KV from an earlier park keeps `held`.
pub(super) fn kv_may_be_held(tx: &Transaction<'_>, binding_id: &str) -> Result<(), LifecycleError> {
    tx.execute(
        "UPDATE launch_lmcache SET held_kv_state=COALESCE(held_kv_state,'parking')
          WHERE binding_id=?1 AND planned='on' AND held_kv_bytes>0",
        [binding_id],
    )?;
    Ok(())
}

/// ADR 0033 §§6, 7, amendment A13: a restart-park of `binding_id` completed:
/// its server holds the stopped engine's KV, charged while parked and, after
/// the wake, beside the new engine until a reading shows it released.
pub(super) fn kv_held_by_server(
    tx: &Transaction<'_>,
    binding_id: &str,
) -> Result<(), LifecycleError> {
    tx.execute(
        "UPDATE launch_lmcache SET held_kv_state='held'
          WHERE binding_id=?1 AND planned='on' AND held_kv_bytes>0",
        [binding_id],
    )?;
    Ok(())
}

/// ADR 0033 §§6, 7, amendment A13: `footprint` with the stopped engine's KV
/// `charge` holds added where the parked phase charges it: the device domain
/// on a discrete GPU, the one shared pool on unified memory.
pub(super) fn with_held_kv(
    e: &EffectiveDeployment,
    mut footprint: PhaseFootprint,
    charge: &LmcacheCharge,
) -> Result<PhaseFootprint, LifecycleError> {
    use lc::ChargeAllocations;
    if charge.held_kv_bytes > 0 {
        let domains = domains(e)?;
        let domain = domains.device.as_deref().unwrap_or(&domains.system);
        footprint
            .allocations
            .add_charge(domain, charge.held_kv_bytes, 0);
    }
    Ok(footprint)
}

/// The figures measured for this revision on `host`, if any.
fn measured(
    conn: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    host: &str,
) -> Result<Option<(i64, i64)>, LifecycleError> {
    Ok(conn
        .query_row(
            "SELECT overhead_bytes,device_bytes FROM lmcache_measurements
              WHERE deployment_id=?1 AND revision=?2 AND host_id=?3",
            params![deployment_id, revision, host],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// The runtime profile this revision resolved with on `host`, by name (its
/// installation fingerprint is `startup::installation`).
fn profile_name(
    conn: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    host: &str,
) -> Result<String, LifecycleError> {
    Ok(conn
        .query_row(
            "SELECT COALESCE(
                (SELECT json_extract(h.source_json,'$.runtime_profile') FROM host_effective_revisions h
                  WHERE h.deployment_id=?1 AND h.revision=?2 AND h.host_id=?3),
                (SELECT json_extract(s.config_json,'$.runtime_profile') FROM managed_configuration_sources s
                  WHERE s.deployment_id=?1 AND s.revision=?2),'')",
            params![deployment_id, revision, host],
            |r| r.get::<_, Option<String>>(0),
        )?
        .unwrap_or_default())
}

/// ADR 0033 §8: a sticky `engine_start_failed` of this revision on `host`'s
/// profile.
fn fallback_recorded(
    conn: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    host: &str,
    e: &EffectiveDeployment,
) -> Result<bool, LifecycleError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM lmcache_fallbacks WHERE deployment_id=?1 AND revision=?2
            AND host_id=?3 AND profile_name=?4 AND profile_fingerprint=?5)",
        params![
            deployment_id,
            revision,
            host,
            profile_name(conn, deployment_id, revision, host)?,
            super::startup::installation(e)
        ],
        |r| r.get(0),
    )?)
}

/// ADR 0033 §7: the qualified parking table the store plans against, the
/// released [`lc::QUALIFIED_PARKING`]. A test substitutes its own rows on its
/// thread, as the released table is empty until LC4 passes.
#[cfg(not(test))]
fn qualified_parking() -> &'static [lc::QualifiedParking] {
    lc::QUALIFIED_PARKING
}

#[cfg(test)]
thread_local! {
    pub(super) static QUALIFIED_PARKING: std::cell::Cell<&'static [lc::QualifiedParking]> =
        const { std::cell::Cell::new(lc::QUALIFIED_PARKING) };
}

#[cfg(test)]
fn qualified_parking() -> &'static [lc::QualifiedParking] {
    QUALIFIED_PARKING.with(std::cell::Cell::get)
}

/// ADR 0033 §7 (amendment A12): the checkpoint's model type for a launch on
/// `host`. The embedded host's checkpoint is on this machine, so its
/// `config.json` is read where the launch reads it; an enrolled host's is
/// the one that host reported with its sizing or its recorded digest, unknown
/// until it has reported one.
fn model_type_on(
    conn: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    host: &str,
    e: &EffectiveDeployment,
) -> Result<Option<String>, LifecycleError> {
    let enrolled: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_id=?1)",
        [host],
        |r| r.get(0),
    )?;
    if !enrolled {
        return Ok(lc::launch_model_type(e));
    }
    Ok(
        crate::checkpoint_digests::recorded_model_type(conn, deployment_id, revision)?
            .filter(|model_type| lc::table_key(model_type) == Some(model_type.as_str())),
    )
}

/// Contract decision 2: the plan for this revision on `host`, from what the
/// store holds: the frozen revision (declared settings, profile and its
/// registration, residency, engine options, and the product name of the GPU
/// the host published for its devices), the host's published settings and
/// declared capabilities, the sticky fallback and, for a parking residency,
/// the checkpoint's model type ([`model_type_on`]), which the qualified table
/// compares (ADR 0033 §7, amendment A12).
pub(super) fn plan_on(
    conn: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    host: &str,
    e: &EffectiveDeployment,
) -> Result<lc::LmcachePlan, LifecycleError> {
    let (host_lmcache, host_retention) = super::placement::host_lmcache_settings(conn, host, e)?;
    let declared = |capability: &str| -> Result<bool, LifecycleError> {
        Ok(conn.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_id=?1)
                 OR EXISTS(SELECT 1 FROM host_versions v, json_each(v.capabilities_json) c
                            WHERE v.host_id=?1 AND c.value=?2)",
            params![host, capability],
            |r| r.get(0),
        )?)
    };
    // Only a parking residency compares the model type.
    let model_type = if e.residency.parks() {
        model_type_on(conn, deployment_id, revision, host, e)?
    } else {
        None
    };
    let pool_bytes = e.lmcache_pool_bytes;
    let inputs = PlanInputs {
        mode: e.lmcache,
        pool_bytes,
        retention: e.retention,
        host_lmcache,
        host_retention,
        host_declares_lmcache: declared(lc::CAPABILITY)?,
        host_declares_retention: declared(lc::RETENTION_CAPABILITY)?,
        api: e.api,
        topology: e.engine_config.memory().member.is_some(),
        registration: e.profile.lmcache.as_ref(),
        kv_transfer_config_set: capyctl_config::engine_policy::kv_transfer_config_set(
            e.profile.engine,
            e.engine_config.extra_args(),
        ),
        radix_cache_disabled: capyctl_config::engine_policy::radix_cache_disabled(
            e.profile.engine,
            &e.profile
                .args
                .iter()
                .chain(e.engine_config.extra_args())
                .cloned()
                .collect::<Vec<_>>(),
        ),
        engine_start_failed: fallback_recorded(conn, deployment_id, revision, host, e)?,
        discrete_gpu: domains(e).is_ok_and(|d| d.device.is_some()),
        model_type: model_type.as_deref(),
        gpu: lc::launch_gpu(e),
        ..PlanInputs::for_profile(e.profile.engine, &e.profile.build_fingerprint, e.residency)
    };
    Ok(lc::plan_with_table(&inputs, qualified_parking()))
}

/// The charge a plan reserves, with the revision's measured figures where
/// known and the engine's KV cache a restart-parked server keeps mapped.
fn plan_charge(
    conn: &rusqlite::Connection,
    plan: &lc::LmcachePlan,
    deployment_id: &str,
    revision: i64,
    host: &str,
    e: &EffectiveDeployment,
) -> Result<Option<(LmcacheCharge, bool)>, LifecycleError> {
    let measured = measured(conn, deployment_id, revision, host)?;
    Ok(plan
        .charge_measured(measured.map(|m| m.0), measured.map(|m| m.1))
        .map(|charge| {
            (
                LmcacheCharge {
                    // LC1: a stopped engine's whole KV cache stays mapped by
                    // the server until the next engine registers.
                    held_kv_bytes: e.engine_config.memory().kv_cache_bytes.max(0),
                    ..charge
                },
                measured.is_some(),
            )
        }))
}

/// ADR 0013 §4 (placement): `footprint` with the charge a start of this
/// revision on `host` would reserve.
pub(super) fn prospective(
    conn: &rusqlite::Connection,
    deployment_id: &str,
    revision: i64,
    host: &str,
    e: &EffectiveDeployment,
    footprint: PhaseFootprint,
) -> Result<PhaseFootprint, LifecycleError> {
    let plan = plan_on(conn, deployment_id, revision, host, e)?;
    match plan_charge(conn, &plan, deployment_id, revision, host, e)? {
        Some((charge, _)) => lay(e, &footprint, &charge, false, false),
        None => Ok(footprint),
    }
}

/// Lifecycle design §1.6: whether a retained launch's LMCache server holds
/// `port` on `host`.
pub(crate) fn port_held(tx: &Transaction<'_>, host: &str, port: u16) -> rusqlite::Result<bool> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM launch_lmcache l JOIN runtime_bindings b ON b.id=l.binding_id
            WHERE b.state!='released' AND l.host_id=?1 AND ?2 IN (l.rpc_port,l.http_port))",
        params![host, port],
        |r| r.get(0),
    )
}

/// ADR 0033 §3: two free ports of `host`'s endpoint range for the server.
fn lease_ports(
    tx: &Transaction<'_>,
    host: &str,
    e: &EffectiveDeployment,
) -> Result<Option<(u16, u16)>, LifecycleError> {
    let mut found = Vec::with_capacity(2);
    for port in e.host.endpoint_port_range.start..=e.host.endpoint_port_range.end {
        let leased: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM endpoint_leases WHERE host_id=?2 AND port=?1)",
            params![port, host],
            |r| r.get(0),
        )?;
        if leased
            || crate::groups::rendezvous_port_held(tx, host, port)?
            || port_held(tx, host, port)?
        {
            continue;
        }
        found.push(port);
        if let [rpc, http] = found[..] {
            return Ok(Some((rpc, http)));
        }
    }
    Ok(None)
}

fn launch_of<'a>(
    deployment_id: &'a str,
    instance_index: u32,
    generation: i64,
    binding_id: &'a str,
    host: &'a str,
) -> LmcacheLaunch<'a> {
    LmcacheLaunch {
        deployment_id,
        instance_index,
        generation,
        binding_id,
        host_id: host,
    }
}

fn event_error(error: crate::StoreError) -> LifecycleError {
    match error {
        crate::StoreError::Sql(error) => LifecycleError::Sql(error),
        _ => LifecycleError::CorruptStoredData,
    }
}

/// Journal `lmcache_off` for a launch whose host key names one (a launch
/// without a placed host has nothing to name).
fn off_event(
    tx: &Transaction<'_>,
    launch: &LmcacheLaunch<'_>,
    reason: LmcacheOffReason,
    detail: &str,
) -> Result<bool, LifecycleError> {
    if launch.host_id.is_empty() {
        return Ok(false);
    }
    lmcache_off_in(tx, launch, reason, detail).map_err(event_error)
}

/// At accept: plan the new binding's LMCache, lease its ports and record it.
/// `force_off` (ADR 0033 §8) keeps it off whatever the plan says.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_launch(
    tx: &Transaction<'_>,
    f: &DeploymentFence,
    instance: u32,
    binding_id: &str,
    host: &str,
    e: &EffectiveDeployment,
    force_off: Option<LmcacheOffReason>,
    now: i64,
) -> Result<(), LifecycleError> {
    let mut plan = plan_on(tx, &f.deployment_id, f.revision, host, e)?;
    if let (Some(reason), true) = (force_off, plan.is_on()) {
        plan.state = lc::LmcacheState::Off(LmcacheOff::new(reason));
    }
    let mut ports = None;
    if plan.is_on() {
        ports = lease_ports(tx, host, e)?;
        if ports.is_none() {
            // ADR 0033 §§3, 9: no two free loopback ports for the server is
            // a server that cannot start.
            if plan.mode == LmcacheMode::Required {
                return Err(LifecycleError::CapacityBlocked(Some(
                    "lmcache_unavailable:server_start_failed: no free port for the LMCache \
                     server in the host's endpoint range"
                        .into(),
                )));
            }
            plan.state = lc::LmcacheState::Off(LmcacheOff::with_detail(
                LmcacheOffReason::ServerStartFailed,
                "no free port for the LMCache server in the host's endpoint range",
            ));
        }
    }
    let charge = plan_charge(tx, &plan, &f.deployment_id, f.revision, host, e)?;
    let restart_park = plan.is_on()
        && e.residency == capyctl_config::effective::Residency::RestartOnly
        && e.engine_config.memory().member.is_none();
    let off = plan.off().cloned();
    tx.execute(
        "INSERT INTO launch_lmcache(binding_id,deployment_id,instance_index,generation,revision,host_id,
            mode,retention_zero,planned,planned_reason,planned_detail,rpc_port,http_port,pool_bytes,
            overhead_bytes,device_bytes,held_kv_bytes,measured,restart_park)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
        params![
            binding_id,
            f.deployment_id,
            instance,
            f.generation,
            f.revision,
            host,
            plan.mode.name(),
            plan.retention_zero,
            if plan.is_on() { "on" } else { "off" },
            off.as_ref().map(|off| off.reason.code()),
            off.as_ref().map(|off| off.detail.clone()),
            ports.map(|p| p.0),
            ports.map(|p| p.1),
            charge.map(|c| c.0.pool_bytes),
            charge.map(|c| c.0.overhead_bytes),
            charge.map(|c| c.0.device_bytes),
            charge.map(|c| c.0.held_kv_bytes),
            charge.is_some_and(|c| c.1),
            restart_park,
        ],
    )?;
    // ADR 0033 §9: a reason known before launch is journaled once; the
    // deployment's own `lmcache: off` is the operator's choice, not news, and
    // a relaunch forced off (ADR 0033 §8) repeats what its failed launch's
    // event already said.
    if let Some(off) =
        off.filter(|off| off.reason != LmcacheOffReason::Disabled && force_off.is_none())
    {
        let launch = launch_of(&f.deployment_id, instance, f.generation, binding_id, host);
        if off_event(tx, &launch, off.reason, &off.detail)? {
            tx.execute(
                "UPDATE launch_lmcache SET off_event=1,reported_at_ms=?2 WHERE binding_id=?1",
                params![binding_id, now],
            )?;
        }
    }
    Ok(())
}

/// The plan stored for `binding_id`.
pub(crate) fn read_plan(
    conn: &rusqlite::Connection,
    binding_id: &str,
) -> Result<Option<LaunchLmcachePlan>, LifecycleError> {
    type Raw = (
        String,
        bool,
        bool,
        String,
        Option<String>,
        Option<String>,
        Option<u16>,
        Option<u16>,
        Option<(i64, i64, i64, i64)>,
    );
    let raw: Option<Raw> = conn
        .query_row(
            "SELECT mode,retention_zero=1,restart_park=1,planned,planned_reason,planned_detail,rpc_port,http_port,
                    pool_bytes,overhead_bytes,device_bytes,held_kv_bytes
               FROM launch_lmcache WHERE binding_id=?1",
            [binding_id],
            |r| {
                let charge = match (
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                    r.get::<_, Option<i64>>(10)?,
                    r.get::<_, Option<i64>>(11)?,
                ) {
                    (Some(a), Some(b), Some(c), Some(d)) => Some((a, b, c, d)),
                    _ => None,
                };
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    charge,
                ))
            },
        )
        .optional()?;
    let Some((mode, retention_zero, restart_park, planned, reason, detail, rpc, http, charge)) =
        raw
    else {
        return Ok(None);
    };
    let mode = match mode.as_str() {
        "auto" => LmcacheMode::Auto,
        "off" => LmcacheMode::Off,
        "required" => LmcacheMode::Required,
        _ => return Err(LifecycleError::CorruptStoredData),
    };
    let planned = match (planned.as_str(), rpc, http, charge) {
        ("on", Some(rpc_port), Some(http_port), Some((pool, overhead, device, held_kv))) => {
            PlannedLmcache::On(LmcacheGrant {
                rpc_port,
                http_port,
                pool_bytes: pool,
                overhead_bytes: overhead,
                device_bytes: device,
                held_kv_bytes: held_kv,
            })
        }
        ("off", ..) => {
            let reason = reason
                .as_deref()
                .and_then(LmcacheOffReason::parse)
                .ok_or(LifecycleError::CorruptStoredData)?;
            PlannedLmcache::Off(LmcacheOff::with_detail(
                reason,
                detail.unwrap_or_else(|| reason.sentence().to_owned()),
            ))
        }
        _ => return Err(LifecycleError::CorruptStoredData),
    };
    Ok(Some(LaunchLmcachePlan {
        mode,
        retention_zero,
        restart_park,
        planned,
    }))
}

/// ADR 0033 §7 (lifecycle design §4.1): how the launch `binding_id` resolved
/// as `e` parks: the engine sleeps where the revision parks; a single
/// `restart_only` launch whose host reported LMCache on stops its engine and
/// keeps the server; anything else does not park.
pub(crate) fn park_mode(
    conn: &rusqlite::Connection,
    e: &EffectiveDeployment,
    binding_id: &str,
) -> Result<ParkMode, LifecycleError> {
    if super::park::parks(e) {
        return Ok(ParkMode::Engine);
    }
    if e.residency != capyctl_config::effective::Residency::RestartOnly {
        return Ok(ParkMode::None);
    }
    let on: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM launch_lmcache WHERE binding_id=?1 AND planned='on'
            AND state='on' AND restart_park=1)",
        [binding_id],
        |r| r.get(0),
    )?;
    Ok(if on {
        ParkMode::EngineStop
    } else {
        ParkMode::None
    })
}

/// The event identity of `binding_id`'s launch.
type Identity = (String, u32, i64, String);

fn identity(conn: &rusqlite::Connection, binding_id: &str) -> Result<Identity, LifecycleError> {
    conn.query_row(
        "SELECT deployment_id,instance_index,generation,host_id FROM launch_lmcache WHERE binding_id=?1",
        [binding_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .optional()?
    .ok_or(LifecycleError::Conflict)
}

/// Record what the host reported for `binding_id` (lifecycle design §10)
/// inside the caller's transaction; `lmcache_off` once when off. The first
/// report decides: a launch's charge never changes under a reservation
/// already made from it, so a later report that disagrees changes nothing.
/// A report that repeats the reason the store planned the launch off for
/// journals nothing: the plan decided that event (ADR 0033 §8: a relaunch
/// forced off repeats what its failed launch's event already said).
pub(super) fn record_outcome(
    tx: &Transaction<'_>,
    binding_id: &str,
    outcome: &LmcacheOutcome,
    now: i64,
) -> Result<(), LifecycleError> {
    let (deployment, instance, generation, host) = identity(tx, binding_id)?;
    let (planned_on, state, planned_reason): (bool, Option<String>, Option<String>) = tx
        .query_row(
            "SELECT planned='on',state,planned_reason FROM launch_lmcache WHERE binding_id=?1",
            [binding_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
    match outcome {
        LmcacheOutcome::On => {
            // A host never runs a server the store did not plan.
            if !planned_on {
                return Err(LifecycleError::Conflict);
            }
            tx.execute(
                "UPDATE launch_lmcache SET state='on',reported_at_ms=?2 WHERE binding_id=?1 AND state IS NULL",
                params![binding_id, now],
            )?;
        }
        LmcacheOutcome::Off { reason, detail } => {
            if state.as_deref() == Some("on") {
                return Ok(());
            }
            let detail = capyctl_domain::diagnostics::bound_chars(detail, 2048);
            tx.execute(
                "UPDATE launch_lmcache SET state='off',reason=?2,detail=?3,reported_at_ms=?4
                  WHERE binding_id=?1 AND state IS NULL",
                params![binding_id, reason.code(), detail, now],
            )?;
            if *reason != LmcacheOffReason::Disabled
                && planned_reason.as_deref() != Some(reason.code())
            {
                let launch = launch_of(&deployment, instance, generation, binding_id, &host);
                if off_event(tx, &launch, *reason, &detail)? {
                    tx.execute(
                        "UPDATE launch_lmcache SET off_event=1 WHERE binding_id=?1",
                        [binding_id],
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// ADR 0033 §7 (lifecycle design §4.1): the identities a restart-park step's
/// evidence leaves the binding with, or `None` when it does not prove the
/// step. A park names the recorded server (its leader, `same_lmcache`) alone,
/// alive, with the engine's memory released; a restore names a new engine
/// (any engine identity recorded for the wake among it) beside the same
/// server, with every restore fact. A recorded member of the server's group
/// the evidence no longer names (a child that exited) stays recorded: only
/// the launch's verified stop proves it gone.
pub(super) fn restart_park_evidence(
    park: bool,
    recorded: &[ProcessIdentity],
    observed: &[ProcessIdentity],
    facts: &[capyctl_domain::completion::Milestone],
) -> Option<Vec<ProcessIdentity>> {
    use capyctl_domain::completion::{lmcache_members, same_lmcache, Milestone};
    if !same_lmcache(recorded, observed) {
        return None;
    }
    let mut held = canonical_members(observed).ok()?;
    let unnamed: Vec<_> = lmcache_members(recorded)
        .into_iter()
        .filter(|member| !held.contains(member))
        .collect();
    held.extend(unnamed);
    let held = canonical_members(&held).ok()?;
    if park {
        let released = facts.contains(&Milestone::MemoryReleased)
            && facts
                .iter()
                .all(|f| matches!(f, Milestone::MemoryReleased | Milestone::Quiesced));
        (released && observed.iter().all(ProcessIdentity::is_lmcache)).then_some(held)
    } else {
        let fresh_engine = observed.iter().any(|i| i.role == "api")
            && recorded
                .iter()
                .filter(|i| !i.is_lmcache())
                .all(|i| observed.contains(i));
        (fresh_engine && facts == super::park::ResidencyKind::Restore.facts()).then_some(held)
    }
}

/// The binding of `source` and its association record `identities` from now
/// on (ADR 0033 §7: the engine a restart-park stopped or started).
pub(super) fn reassociate(
    tx: &Transaction<'_>,
    source: &Plan,
    identities: &[ProcessIdentity],
) -> Result<(), LifecycleError> {
    let mut association = association(tx, source)?.ok_or(LifecycleError::Conflict)?;
    let held = identity_dtos(&canonical_members(identities)?);
    one(tx.execute(
        "UPDATE runtime_bindings SET identities_json=?2 WHERE id=?1 AND state='live'",
        params![source.binding_id, encode(&held)?],
    )?)?;
    association.held = Some(held);
    one(tx.execute(
        "UPDATE owned_launch_associations SET association_json=?2 WHERE step_id=?1",
        params![source.step_id, encode(&association)?],
    )?)?;
    Ok(())
}

impl crate::Store {
    /// ADR 0033 §7 (lifecycle design §4.3): record the api identity of the
    /// engine a restart-park's restore `step_id` started on `binding_id`'s
    /// running server, before that engine runs, so a stop finds it whatever
    /// becomes of the restore. Accepted only while that restore step is armed
    /// on the binding, once; the same identity again is a replay.
    pub fn record_wake_identity(
        &self,
        s: &CoordinatorSession,
        fence: &DeploymentFence,
        binding_id: &str,
        step_id: &str,
        identity: &ProcessIdentity,
    ) -> Result<(), LifecycleError> {
        if identity.role != "api"
            || identity.pid == 0
            || identity.boot_id.trim().is_empty()
            || identity.start_ticks == 0
        {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let armed: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                JOIN runtime_bindings b ON b.id=s.binding_id
               WHERE s.id=?1 AND s.binding_id=?2 AND s.state='armed' AND s.session_id=?3
                 AND o.kind='restore' AND b.state='live' AND b.deployment_id=?4 AND b.revision=?5)",
            params![
                step_id,
                binding_id,
                s.id(),
                fence.deployment_id,
                fence.revision
            ],
            |r| r.get(0),
        )?;
        if !armed {
            return Err(LifecycleError::Conflict);
        }
        let source: String = tx
            .query_row(
                "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                  WHERE s.binding_id=?1 AND o.kind='initialize' AND s.state='completed'",
                [binding_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(LifecycleError::Conflict)?;
        let (p, e, _) = load(&tx, &source)?;
        if p.fence() != *fence || park_mode(&tx, &e, binding_id)? != ParkMode::EngineStop {
            return Err(LifecycleError::Conflict);
        }
        let association = association(&tx, &p)?.ok_or(LifecycleError::Conflict)?;
        let mut recorded = identities(association.held());
        if recorded.contains(identity) {
            return Ok(());
        }
        // Only the server's group stands while parked; one wake engine.
        if recorded.iter().any(|i| !i.is_lmcache()) {
            return Err(LifecycleError::Conflict);
        }
        recorded.push(identity.clone());
        reassociate(&tx, &p, &recorded)?;
        tx.commit()?;
        Ok(())
    }

    /// ADR 0033 §§3, 9 (found live on SGLang: a launch whose server failed
    /// its listener check launched its engine without LMCache, and its Ready
    /// was refused `lifecycle conflict` because the binding still named the
    /// stopped server beside the engine): the LMCache server of
    /// `binding_id`'s launch, stopped with every process proven gone by the
    /// host before any engine of the launch was spawned (`server_start_failed`
    /// or `listener_exposed` under `auto`), leaves the binding's record, and
    /// a `lmcache_server_departed` journal entry names it, in one
    /// transaction. Accepted only while the launch's Initialize is armed in
    /// this session under `fence`, before its association, with no engine
    /// process recorded and every server identity the binding records among
    /// `server`; a binding that records none is a replay. The gone evidence
    /// is the caller's (the store observes no process): a caller that has
    /// not proven each of `server` gone keeps the record as it is.
    pub fn depart_lmcache_server(
        &self,
        s: &CoordinatorSession,
        fence: &DeploymentFence,
        binding_id: &str,
        server: &[ProcessIdentity],
    ) -> Result<(), LifecycleError> {
        if server.is_empty() || !server.iter().all(ProcessIdentity::is_lmcache) {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let step: String = tx
            .query_row(
                "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                  WHERE s.binding_id=?1 AND s.state='armed' AND o.kind='initialize'",
                [binding_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(LifecycleError::Conflict)?;
        let (p, _, _) = load(&tx, &step)?;
        current(&tx, s, &p, false)?;
        if p.fence() != *fence || association(&tx, &p)?.is_some() {
            return Err(LifecycleError::Conflict);
        }
        let stored: String = tx
            .query_row(
                "SELECT identities_json FROM runtime_bindings WHERE id=?1 AND state!='released'",
                [binding_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(LifecycleError::Conflict)?;
        let recorded = identities(&decode::<Vec<IdentityDto>>(&stored)?);
        if recorded.iter().any(|identity| !identity.is_lmcache()) {
            return Err(LifecycleError::Conflict);
        }
        if recorded.is_empty() {
            return Ok(());
        }
        if recorded.iter().any(|identity| !server.contains(identity)) {
            return Err(LifecycleError::Conflict);
        }
        one(tx.execute(
            "UPDATE runtime_bindings SET identities_json='[]' WHERE id=?1 AND state!='released'",
            [binding_id],
        )?)?;
        let named: Vec<String> = recorded
            .iter()
            .map(|identity| format!("{} {}", identity.role, identity.pid))
            .collect();
        tx.execute(
            "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,NULL,?2,'lmcache_server_departed',?3)",
            params![
                ulid::Ulid::new().to_string(),
                p.operation_id,
                format!(
                    "deployment {} instance {}: the LMCache server ({}) stopped with a verified \
                     exit before the engine was spawned and left the launch's record; the \
                     engine starts without LMCache",
                    p.deployment_id,
                    p.instance_index,
                    named.join(", ")
                ),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// ADR 0033 §3 (lifecycle design §5): journal `lmcache_server_exited` once
/// for `binding_id`.
pub(super) fn server_exited(
    tx: &Transaction<'_>,
    binding_id: &str,
    detail: &str,
) -> Result<bool, LifecycleError> {
    let (deployment, instance, generation, host) = identity(tx, binding_id)?;
    let recorded: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM management_events WHERE kind='lmcache_server_exited'
            AND deployment_id=?1 AND json_extract(payload_json,'$.binding_id')=?2)",
        params![deployment, binding_id],
        |r| r.get(0),
    )?;
    if recorded || host.is_empty() {
        return Ok(false);
    }
    let launch = launch_of(&deployment, instance, generation, binding_id, &host);
    lmcache_server_exited_in(tx, &launch, detail).map_err(event_error)?;
    Ok(true)
}

impl crate::Store {
    /// Lifecycle design §10: what the store planned for `binding_id`'s
    /// LMCache, `None` for a binding planned before ADR 0033.
    pub fn launch_lmcache(
        &self,
        binding_id: &str,
    ) -> Result<Option<LaunchLmcachePlan>, LifecycleError> {
        read_plan(&self.conn, binding_id)
    }

    /// Lifecycle design §10: the grant (ports and charge) of `binding_id`,
    /// `None` when the store planned LMCache off for it.
    pub fn launch_lmcache_grant(
        &self,
        binding_id: &str,
    ) -> Result<Option<LmcacheGrant>, LifecycleError> {
        Ok(self
            .launch_lmcache(binding_id)?
            .and_then(|plan| plan.grant().copied()))
    }

    /// ADR 0033 §§3, 8, 9 (lifecycle design §10): record what the host
    /// reported for `binding_id`'s LMCache: on, or off with its reason and
    /// detail, journaling `lmcache_off` once for the launch (none for the
    /// deployment's own `disabled`). Called before the launch's Ready
    /// completion, so its steady charge drops LMCache's when it was off.
    pub fn record_launch_lmcache(
        &self,
        s: &CoordinatorSession,
        binding_id: &str,
        outcome: &LmcacheOutcome,
        now: i64,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let retained: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND state!='released')",
            [binding_id],
            |r| r.get(0),
        )?;
        if !retained {
            return Err(LifecycleError::Conflict);
        }
        record_outcome(&tx, binding_id, outcome, now)?;
        tx.commit()?;
        Ok(())
    }

    /// ADR 0033 §6, A1 (lifecycle design §7): the server overhead beyond its
    /// pool (its whole resident set, file-backed pages included, beyond the
    /// pool) and the server's own GPU memory measured for `binding_id`'s
    /// launch, kept per (deployment, revision, host) as the largest seen;
    /// later launches of the revision there charge them in place of the
    /// allowances. Zero is no reading: it never lowers a figure, and a
    /// figure never read keeps its allowance
    /// ([`capyctl_config::lmcache::LmcachePlan::charge_measured`]). A13:
    /// from a restart-park's arm until a GPU reading shows the stopped
    /// engine's KV released, the GPU reading is not recorded (it counts that
    /// KV), and such a reading releases it ([`LmcacheMeasured`]).
    pub fn record_lmcache_overhead(
        &self,
        s: &CoordinatorSession,
        binding_id: &str,
        overhead_bytes: i64,
        device_bytes: i64,
        now: i64,
    ) -> Result<LmcacheMeasured, LifecycleError> {
        if overhead_bytes < 0 || device_bytes < 0 || now < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let measured = record_overhead(&tx, binding_id, overhead_bytes, device_bytes, now)?;
        tx.commit()?;
        Ok(measured)
    }

    /// ADR 0033 §§6, 12, A1 (lifecycle design §7): what `host` measured of
    /// the LMCache server of its launch `step_id` (the launch's owned handle)
    /// of `deployment` at `generation`: the server group's resident set and
    /// its GPU memory (zero: none read). The resident set beyond the granted
    /// pool is the overhead, recorded as [`Self::record_lmcache_overhead`]
    /// does. A sample naming no retained launch of that host with a server
    /// changes nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn record_host_lmcache_measurement(
        &self,
        s: &CoordinatorSession,
        host: &str,
        deployment: &str,
        generation: i64,
        step_id: &str,
        resident_bytes: i64,
        device_bytes: i64,
        now: i64,
    ) -> Result<LmcacheMeasured, LifecycleError> {
        if resident_bytes < 0 || device_bytes < 0 || now < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let launch: Option<(String, i64)> = tx
            .query_row(
                "SELECT l.binding_id,l.pool_bytes FROM lifecycle_steps st
                   JOIN launch_lmcache l ON l.binding_id=st.binding_id
                  WHERE st.id=?1 AND st.deployment_id=?2 AND l.generation=?3 AND l.host_id=?4
                    AND l.planned='on'",
                params![step_id, deployment, generation, host],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((binding, pool)) = launch else {
            return Ok(LmcacheMeasured::default());
        };
        let overhead = resident_bytes.saturating_sub(pool).max(0);
        let measured = record_overhead(&tx, &binding, overhead, device_bytes, now)?;
        tx.commit()?;
        Ok(measured)
    }

    /// ADR 0033 §8 (lifecycle design §3): release a launch whose engine failed
    /// before Ready with the LMCache connector, in one transaction with the
    /// sticky fallback for its revision on its host's profile, the
    /// `lmcache_off` event (`engine_start_failed`, the redacted log tail) and
    /// the instance's closed admission. Under `lmcache: required` the launch
    /// fails `lmcache_unavailable:engine_start_failed` and nothing starts
    /// again; otherwise it fails `launch_failed` and a start of the same
    /// fence is accepted with LMCache off. A start the store refuses (no
    /// capacity, a full queue) leaves the instance failed; the sticky row
    /// makes its next start go without LMCache.
    #[allow(clippy::too_many_arguments)]
    pub fn release_failed_launch_lmcache(
        &self,
        s: &CoordinatorSession,
        step_id: &str,
        evidence: &capyctl_domain::completion::CleanupEvidence,
        now: i64,
        ttl: i64,
        detail: &str,
    ) -> Result<LmcacheFallback, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (p, e, _) = load(&tx, step_id)?;
        let plan = read_plan(&tx, &p.binding_id)?.ok_or(LifecycleError::Conflict)?;
        if plan.grant().is_none() {
            return Err(LifecycleError::Conflict);
        }
        let code = if plan.required() {
            LmcacheOffReason::EngineStartFailed.required_refusal()
        } else {
            "launch_failed".to_owned()
        };
        super::failed_launch::release_with_code(&tx, s, step_id, evidence, now, ttl, &code)?;
        let host: String = tx.query_row(
            "SELECT host_id FROM launch_lmcache WHERE binding_id=?1",
            [&p.binding_id],
            |r| r.get(0),
        )?;
        let detail = capyctl_domain::diagnostics::bound_chars(detail, 2048);
        tx.execute(
            "INSERT OR IGNORE INTO lmcache_fallbacks(deployment_id,revision,host_id,profile_name,profile_fingerprint,detail,recorded_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                p.deployment_id,
                p.revision,
                host,
                profile_name(&tx, &p.deployment_id, p.revision, &host)?,
                super::startup::installation(&e),
                detail,
                now
            ],
        )?;
        record_outcome(
            &tx,
            &p.binding_id,
            &LmcacheOutcome::Off {
                reason: LmcacheOffReason::EngineStartFailed,
                detail: detail.clone(),
            },
            now,
        )?;
        // ADR 0011 decision 4: the failed instance closes its own admission.
        tx.execute(
            "UPDATE deployment_instances SET admission_enabled=0 WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
            params![p.deployment_id, p.instance_index, p.generation],
        )?;
        if plan.required() {
            tx.commit()?;
            return Ok(LmcacheFallback::Failed(code));
        }
        let fence = p.fence();
        let outcome = match now.checked_add(e.request_deadline_ms) {
            Some(deadline) => {
                tx.execute("SAVEPOINT lmcache_relaunch", [])?;
                match Self::accept_start_in_transaction(
                    &tx,
                    s,
                    &fence,
                    now,
                    deadline,
                    false,
                    Some(LmcacheOffReason::EngineStartFailed),
                ) {
                    Ok(start) => {
                        tx.execute("RELEASE lmcache_relaunch", [])?;
                        LmcacheFallback::Relaunched(start)
                    }
                    Err(LifecycleError::Sql(error)) => return Err(LifecycleError::Sql(error)),
                    Err(_) => {
                        tx.execute("ROLLBACK TO lmcache_relaunch", [])?;
                        tx.execute("RELEASE lmcache_relaunch", [])?;
                        LmcacheFallback::Failed(code)
                    }
                }
            }
            None => LmcacheFallback::Failed(code),
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// ADR 0033 §§9, 10: record the `lmcache` and `retention` standalone's
    /// embedded host `host` runs under now. Recorded at every start, before
    /// anything is placed or started: the embedded host publishes no
    /// approved document, and LMCache is planned from this row rather than
    /// from the settings a revision was resolved under.
    pub fn record_embedded_host_settings(
        &self,
        host: &str,
        lmcache: lc::HostLmcache,
        retention: lc::Retention,
        now: i64,
    ) -> Result<(), LifecycleError> {
        if host.is_empty() || host.len() > 128 || now < 0 {
            return Err(LifecycleError::Invalid);
        }
        let name = |value: serde_json::Value| value.as_str().map(str::to_owned);
        let (Some(lmcache), Some(retention)) = (
            name(serde_json::json!(lmcache)),
            name(serde_json::json!(retention)),
        ) else {
            return Err(LifecycleError::Invalid);
        };
        self.conn.execute(
            "INSERT INTO embedded_host_settings(host_id,lmcache,retention,recorded_at_ms) VALUES(?1,?2,?3,?4)
             ON CONFLICT(host_id) DO UPDATE SET lmcache=excluded.lmcache,retention=excluded.retention,recorded_at_ms=excluded.recorded_at_ms",
            params![host, lmcache, retention, now],
        )?;
        Ok(())
    }

    /// ADR 0033 §§9, 10 (lifecycle design §9): a role starts on `host` with
    /// `lmcache: off` or `retention: zero` (`reason`): every instance there
    /// whose retained launch runs a server is stopped with it (drained and
    /// verified gone, as any stop), `lmcache_off` journaled once with the
    /// reason. A Ready one is owed a start without LMCache
    /// ([`Self::relaunch_retired_lmcache`]); a parked one is released like an
    /// idle stop and stays eligible for on-demand activation (T10).
    /// Idempotent: a repeat replays each stop. Returns the stop operations.
    pub fn retire_lmcache_on_host(
        &self,
        s: &CoordinatorSession,
        host: &str,
        reason: LmcacheOffReason,
        now: i64,
    ) -> Result<Vec<String>, LifecycleError> {
        if !matches!(
            reason,
            LmcacheOffReason::HostOff | LmcacheOffReason::RetentionZero
        ) || now < 0
        {
            return Err(LifecycleError::Invalid);
        }
        let rows: Vec<(String, String, u32)> = {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            check_session(&tx, s)?;
            let rows = tx
                .prepare(
                    "SELECT l.binding_id,l.deployment_id,l.instance_index FROM launch_lmcache l
                       JOIN runtime_bindings b ON b.id=l.binding_id
                      WHERE l.host_id=?1 AND b.state='live' AND l.planned='on' AND COALESCE(l.state,'on')='on'
                      ORDER BY l.deployment_id,l.instance_index",
                )?
                .query_map([host], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            tx.commit()?;
            rows
        };
        let mut stops = Vec::new();
        for (binding, deployment, instance) in rows {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            check_session(&tx, s)?;
            let Ok((source, e, _)) = super::park::launch(&tx, &deployment, instance) else {
                continue;
            };
            if source.binding_id != binding {
                continue;
            }
            let observed: String = tx.query_row(
                "SELECT observed_state FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2",
                params![deployment, instance],
                |r| r.get(0),
            )?;
            let parked = match observed.as_str() {
                "ready" => false,
                "parked" => true,
                _ => continue,
            };
            let revision: i64 = tx.query_row(
                "SELECT revision FROM deployments WHERE id=?1",
                [&deployment],
                |r| r.get(0),
            )?;
            let stop = Self::accept_instance_stop_in_transaction(
                &tx,
                s,
                if parked { "idle" } else { RETIRE_PRINCIPAL },
                &source.fence(),
                &format!("lmcache-retire:{binding}"),
                now,
                now.saturating_add(e.request_deadline_ms),
                &super::cleanup::StopCommand {
                    scope: Some(super::cleanup::instance_scope(&deployment, instance)),
                    revision: Some(revision),
                },
            );
            let stop = match stop {
                Ok(stop) => stop,
                Err(LifecycleError::Sql(error)) => return Err(LifecycleError::Sql(error)),
                // Refused for now (another operation of the instance holds
                // it); the next role start or publication asks again.
                Err(_) => continue,
            };
            if !parked {
                tx.execute(
                    "INSERT OR IGNORE INTO lmcache_relaunches(deployment_id,instance_index,generation,reason,stop_operation_id)
                     VALUES(?1,?2,?3,?4,?5)",
                    params![deployment, instance, source.generation, reason.code(), stop.operation_id],
                )?;
            }
            let launch = launch_of(&deployment, instance, source.generation, &binding, host);
            if off_event(&tx, &launch, reason, "")? {
                tx.execute(
                    "UPDATE launch_lmcache SET off_event=1 WHERE binding_id=?1",
                    [&binding],
                )?;
            }
            super::park::journal(
                &tx,
                &stop.operation_id,
                "lmcache_retired",
                &format!(
                    "deployment {deployment}: instance {instance} ran an LMCache server and the role \
                     started with {}; it stops with its server{}",
                    match reason {
                        LmcacheOffReason::HostOff => "`lmcache: off`",
                        _ => "`retention: zero`",
                    },
                    if parked {
                        " like an idle stop and stays eligible for on-demand activation"
                    } else {
                        " and starts again without LMCache"
                    }
                ),
            )?;
            tx.commit()?;
            stops.push(stop.operation_id);
        }
        Ok(stops)
    }

    /// ADR 0033 §§9, 10 (lifecycle design §9): start again, without LMCache,
    /// each instance [`Self::retire_lmcache_on_host`] stopped while Ready,
    /// once its stop completed and it holds nothing. An instance an operator
    /// acted on since (stopped, suspended, started) loses its marker; one
    /// whose host is outside `eligible` (`None`: every host) waits. Returns
    /// the starts accepted.
    pub fn relaunch_retired_lmcache(
        &self,
        s: &CoordinatorSession,
        now: i64,
        eligible: Option<&std::collections::BTreeSet<String>>,
    ) -> Result<Vec<Start>, LifecycleError> {
        type Due = (String, u32, i64, Option<i64>, i64, Option<String>, String);
        let due: Vec<Due> = {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            check_session(&tx, s)?;
            let due = tx
                .prepare(
                    "SELECT m.deployment_id,m.instance_index,m.generation,i.revision,i.generation,i.host_id,o.state
                       FROM lmcache_relaunches m
                       JOIN deployment_instances i ON i.deployment_id=m.deployment_id AND i.instance_index=m.instance_index
                       JOIN operations o ON o.id=m.stop_operation_id",
                )?
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            tx.commit()?;
            due
        };
        let mut started = Vec::new();
        for (deployment, instance, stopped_at, revision, generation, host, stop_state) in due {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            check_session(&tx, s)?;
            let forget = |tx: &Transaction<'_>| {
                tx.execute(
                    "DELETE FROM lmcache_relaunches WHERE deployment_id=?1 AND instance_index=?2",
                    params![deployment, instance],
                )
            };
            match stop_state.as_str() {
                "succeeded" => {}
                "failed" => {
                    forget(&tx)?;
                    tx.commit()?;
                    continue;
                }
                _ => continue,
            }
            // Someone acted on the instance since: its own choice stands.
            let free: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM deployment_instances i JOIN deployments d ON d.id=i.deployment_id
                    WHERE i.deployment_id=?1 AND i.instance_index=?2 AND i.observed_state='stopped'
                      AND i.operator_stopped=0 AND d.admin_stopped=0 AND d.suspended=0
                      AND i.generation>?3)
                  AND NOT EXISTS(SELECT 1 FROM runtime_bindings WHERE deployment_id=?1 AND instance_index=?2 AND state!='released')
                  AND NOT EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND instance_index=?2 AND state NOT IN ('succeeded','failed'))",
                params![deployment, instance, stopped_at],
                |r| r.get(0),
            )?;
            let Some(revision) = revision.filter(|_| free) else {
                let busy: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND instance_index=?2 AND state NOT IN ('succeeded','failed'))
                       OR EXISTS(SELECT 1 FROM runtime_bindings WHERE deployment_id=?1 AND instance_index=?2 AND state!='released')",
                    params![deployment, instance],
                    |r| r.get(0),
                )?;
                if !busy {
                    forget(&tx)?;
                    tx.commit()?;
                }
                continue;
            };
            if let (Some(eligible), Some(host)) = (eligible, &host) {
                if !eligible.contains(host) {
                    continue;
                }
            }
            let fence = DeploymentFence {
                deployment_id: deployment.clone(),
                revision,
                generation,
            };
            let Ok((_, e)) = effective(&tx, &fence) else {
                forget(&tx)?;
                tx.commit()?;
                continue;
            };
            let Some(deadline) = now.checked_add(e.request_deadline_ms) else {
                continue;
            };
            tx.execute("SAVEPOINT lmcache_relaunch", [])?;
            match Self::accept_start_in_transaction(&tx, s, &fence, now, deadline, false, None) {
                Ok(start) => {
                    tx.execute("RELEASE lmcache_relaunch", [])?;
                    forget(&tx)?;
                    tx.commit()?;
                    started.push(start);
                }
                Err(LifecycleError::Sql(error)) => return Err(LifecycleError::Sql(error)),
                // Refused for now (capacity, the queue): asked again on the
                // next pass; the instance stays stopped meanwhile.
                Err(_) => {
                    tx.execute("ROLLBACK TO lmcache_relaunch", [])?;
                    tx.execute("RELEASE lmcache_relaunch", [])?;
                    tx.commit()?;
                }
            }
        }
        Ok(started)
    }

    /// ADR 0033 §§5, 12 (lifecycle design §§5, 8): this session's Ready and
    /// parked embedded launches that run a server, for the exit scan and the
    /// metrics read. A launch that no longer validates is omitted.
    pub fn local_lmcache_launches(
        &self,
        s: &CoordinatorSession,
    ) -> Result<Vec<LocalLmcacheLaunch>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let ids: Vec<(String, bool)> = tx
            .prepare(
                "SELECT s.id,d.observed_state='parked' FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                 JOIN runtime_bindings b ON b.id=s.binding_id AND b.state='live'
                 JOIN launch_lmcache l ON l.binding_id=b.id AND l.planned='on' AND COALESCE(l.state,'on')='on'
                 JOIN instance_runtime d ON d.id=s.deployment_id AND d.instance_index=b.instance_index
                 WHERE o.kind='initialize' AND s.state='completed' AND s.session_id=?1
                   AND d.desired_state='ready' AND d.observed_state IN ('ready','parked') AND d.suspended=0
                   AND NOT EXISTS(SELECT 1 FROM remote_binding_ingress r WHERE r.binding_id=b.id)
                   AND NOT EXISTS(SELECT 1 FROM lifecycle_runs pr JOIN operations po ON po.id=pr.operation_id
                        WHERE pr.deployment_id=s.deployment_id AND pr.instance_index=b.instance_index
                          AND po.kind IN ('park','restore') AND pr.state IN ('queued','running','uncertain'))
                 ORDER BY o.accepted_at,o.id",
            )?
            .query_map([s.id()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut launches = Vec::new();
        for (id, parked) in ids {
            let Ok((p, _, state)) = load(&tx, &id) else {
                continue;
            };
            if state != "completed" {
                continue;
            }
            let Some(association) = association(&tx, &p)? else {
                continue;
            };
            let Some(plan) = read_plan(&tx, &p.binding_id)? else {
                continue;
            };
            let Some(grant) = plan.grant() else {
                continue;
            };
            let server =
                capyctl_domain::completion::lmcache_members(&identities(association.held()));
            if server.is_empty() {
                continue;
            }
            launches.push(LocalLmcacheLaunch {
                fence: p.fence(),
                instance_index: p.instance_index,
                binding_id: p.binding_id.clone(),
                step_id: p.step_id.clone(),
                server,
                http_port: grant.http_port,
                pool_bytes: grant.pool_bytes,
                parked,
            });
        }
        Ok(launches)
    }

    /// ADR 0033 §7 (lifecycle design §4.1): how the instance's current
    /// launch parks.
    pub fn park_mode(&self, deployment: &str, instance: u32) -> Result<ParkMode, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let (p, e, _) = super::park::launch(&tx, deployment, instance)?;
        park_mode(&tx, &e, &p.binding_id)
    }
}

/// The principal a role start's retirement of a Ready instance stops it
/// under (ADR 0033 §§9, 10).
pub const RETIRE_PRINCIPAL: &str = "system:lmcache_retire";

/// What one measurement of a launch's server did (ADR 0033 §6, A1, A13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LmcacheMeasured {
    /// The revision's recorded figures grew.
    pub grew: bool,
    /// A13: the server may still hold a stopped engine's KV, of this size:
    /// the sample's GPU reading was not recorded. A later GPU reading below
    /// it, taken while the instance is Ready, is the evidence that releases
    /// it; a caller that skips samples no larger than the last still sends
    /// such a reading.
    pub held_kv_bytes: Option<i64>,
}

/// The figures one launch's server measurements sent the store this run, so
/// a caller writes the store only for a sample that may change something
/// (ADR 0033 §6, A1, A13). The host figure is the one the caller sends: the
/// overhead beyond the pool, or the resident set it is taken from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MeasurementSeen {
    host_bytes: i64,
    device_bytes: i64,
    /// A13: the stopped engine's KV the server may still hold, as the store
    /// last said.
    held_kv_bytes: Option<i64>,
}

impl MeasurementSeen {
    /// Whether a sample may change what the store holds: a larger host
    /// figure; a larger GPU reading; or, while the server may still hold a
    /// stopped engine's KV, a GPU reading below that KV (the evidence that
    /// releases it), a larger one counting that KV and changing nothing.
    pub fn wants(&self, host_bytes: i64, device_bytes: i64) -> bool {
        host_bytes > self.host_bytes
            || match self.held_kv_bytes {
                Some(held) => device_bytes > 0 && device_bytes < held,
                None => device_bytes > self.device_bytes,
            }
    }

    /// Remember a sample the store took and what it said of it.
    pub fn sent(&mut self, host_bytes: i64, device_bytes: i64, measured: LmcacheMeasured) {
        self.host_bytes = self.host_bytes.max(host_bytes);
        if measured.held_kv_bytes.is_none() {
            self.device_bytes = self.device_bytes.max(device_bytes);
        }
        self.held_kv_bytes = measured.held_kv_bytes;
    }
}

/// ADR 0033 §6: grow the measurement of `binding_id`'s revision on its host
/// to `overhead_bytes` and `device_bytes` where they exceed it; a binding
/// that is released or planned off changes nothing. A13 (found live on vLLM,
/// 2026-10-23: a reading taken after a wake while the server still held the
/// old engine's KV became the revision's device figure, so every later
/// launch charged about 1.14 GiB too much): while the server may hold a
/// stopped engine's KV, the GPU reading counts that KV and is not recorded,
/// unless it is the reading that releases it ([`release_held_kv`]).
fn record_overhead(
    tx: &Transaction<'_>,
    binding_id: &str,
    overhead_bytes: i64,
    device_bytes: i64,
    now: i64,
) -> Result<LmcacheMeasured, LifecycleError> {
    type Launch = (String, i64, String, u32, Option<String>, i64);
    let launch: Option<Launch> = tx
        .query_row(
            "SELECT l.deployment_id,l.revision,l.host_id,l.instance_index,l.held_kv_state,l.held_kv_bytes
               FROM launch_lmcache l JOIN runtime_bindings b ON b.id=l.binding_id
              WHERE l.binding_id=?1 AND l.planned='on' AND b.state!='released'",
            [binding_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()?;
    let Some((deployment, revision, host, instance, held_state, held_kv)) = launch else {
        return Ok(LmcacheMeasured::default());
    };
    let mut device_bytes = device_bytes;
    let mut held_kv_bytes = None;
    if let Some(state) = held_state {
        let release = HeldKv {
            binding_id,
            deployment: &deployment,
            instance,
            charged: state == "held",
            held_kv_bytes: held_kv,
        };
        if !release_held_kv(tx, &release, device_bytes)? {
            device_bytes = 0;
            held_kv_bytes = Some(held_kv);
        }
    }
    let prior = measured(tx, &deployment, revision, &host)?;
    if (overhead_bytes == 0 && device_bytes == 0)
        || prior.is_some_and(|(o, d)| o >= overhead_bytes && d >= device_bytes)
    {
        return Ok(LmcacheMeasured {
            grew: false,
            held_kv_bytes,
        });
    }
    tx.execute(
        "INSERT INTO lmcache_measurements(deployment_id,revision,host_id,overhead_bytes,device_bytes,binding_id,measured_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(deployment_id,revision,host_id) DO UPDATE SET
           overhead_bytes=MAX(overhead_bytes,excluded.overhead_bytes),
           device_bytes=MAX(device_bytes,excluded.device_bytes),
           binding_id=excluded.binding_id,measured_at_ms=excluded.measured_at_ms",
        params![deployment, revision, host, overhead_bytes, device_bytes, binding_id, now],
    )?;
    Ok(LmcacheMeasured {
        grew: true,
        held_kv_bytes,
    })
}

/// A launch whose server may still hold a stopped engine's KV.
struct HeldKv<'a> {
    binding_id: &'a str,
    deployment: &'a str,
    instance: u32,
    /// `held`: the Ready charge keeps the KV (the restart-park completed).
    charged: bool,
    held_kv_bytes: i64,
}

/// ADR 0033 §§6, 7, amendment A13: whether `device_bytes`, a GPU reading of
/// the server, proves the stopped engine's KV released: a reading (zero is
/// none) below that KV cache, which a server still holding it cannot show,
/// taken while the instance is Ready on this launch with no lifecycle run
/// open. On that evidence the launch leaves the state and, when its Ready
/// charge kept the KV, its owner moves from that charge to the one without
/// it, journaled. An owner holding anything else keeps it all: what it
/// holds is not the charge the evidence speaks to.
fn release_held_kv(
    tx: &Transaction<'_>,
    held: &HeldKv<'_>,
    device_bytes: i64,
) -> Result<bool, LifecycleError> {
    if device_bytes <= 0 || device_bytes >= held.held_kv_bytes {
        return Ok(false);
    }
    let ready: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2
            AND observed_state='ready')",
        params![held.deployment, held.instance],
        |r| r.get(0),
    )?;
    if !ready || super::park::open_run(tx, held.deployment, held.instance)?.is_some() {
        return Ok(false);
    }
    let (p, e, _) = match super::park::launch(tx, held.deployment, held.instance) {
        Ok(launch) => launch,
        Err(LifecycleError::Conflict) => return Ok(false),
        Err(error) => return Err(error),
    };
    if p.binding_id != held.binding_id {
        return Ok(false);
    }
    let charged = if held.charged {
        let before = super::park::charged_footprints(tx, &p, &e)?.ready;
        let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
        if !ledger
            .owners
            .get(&p.owner())
            .is_some_and(|current| super::park::same(current, &before))
        {
            return Ok(false);
        }
        true
    } else {
        false
    };
    one(tx.execute(
        "UPDATE launch_lmcache SET held_kv_state=NULL WHERE binding_id=?1 AND held_kv_state IS NOT NULL",
        [held.binding_id],
    )?)?;
    if charged {
        let after = super::park::charged_footprints(tx, &p, &e)?.ready;
        one(tx.execute(
            "UPDATE resource_owners SET footprint_json=?2 WHERE owner_id=?1",
            params![
                p.owner(),
                resource_ledger::encode(&after).map_err(resource)?
            ],
        )?)?;
        resource_ledger::advance_completion_epoch(tx)?;
        super::park::journal(
            tx,
            &p.operation_id,
            "lmcache_kv_released",
            &format!(
                "deployment {} instance {}: the LMCache server's GPU reading ({} MiB) is below \
                 the stopped engine's KV cache it held since the restart-park ({} MiB): that KV \
                 is released and the Ready charge no longer holds it",
                p.deployment_id,
                p.instance_index,
                device_bytes >> 20,
                held.held_kv_bytes >> 20
            ),
        )?;
    }
    Ok(true)
}

#[cfg(test)]
#[path = "lmcache_tests.rs"]
mod tests;

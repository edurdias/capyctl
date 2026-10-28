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
use capyctl_config::instances::{Placement, PlacementStrategy};
use capyctl_domain::resources::MemoryLimit;
use capyctl_scheduler::device_choice::DeviceOption;
use capyctl_scheduler::placement::{fits_group, place, HostCandidate, HostKvRemedy, Strategy};
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
    /// The instance was placed and fenced for a fresh start, with the change
    /// of tensor-parallel size its `topology: auto` journals once the start
    /// is accepted (ADR 0035 §1a).
    Placed(DeploymentFence, Option<TopologyChange>),
    /// No allowed host can take it now; the closed diagnostic, each host's
    /// reason naming the limit it hit, and the diagnostic as `last_error`
    /// records it (the code, naming the host-KV limit when that alone
    /// refused every host).
    Unplaceable(&'static str, String, String),
}

/// [`Prepared::Unplaceable`] for a refusal first judged `code`, as `refused`
/// (judged again with parked capacity reclaimable) explains it.
fn unplaced(
    code: &'static str,
    refused: &capyctl_scheduler::placement::Unplaceable,
    hosts: &[HostCandidate],
    owner: &str,
) -> Prepared {
    let reason = if refused.code() == code {
        refused.reason(hosts, owner)
    } else {
        code.to_owned()
    };
    Prepared::Unplaceable(code, refused.detail(hosts, owner), reason)
}

pub(super) fn strategy(placement: &Placement) -> Strategy {
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
            // ADR 0033 §6: with the LMCache charge the start was granted.
            super::startup::steady(tx, &plan, &e)?,
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
    // ADR 0018 §4: each host's row also names the runtime profile the
    // revision resolved with there, so a retiring profile is excluded below.
    let hosts: Vec<(String, String, Option<String>)> = tx
        .prepare(
            "SELECT h.host_id, h.effective_json,
                    COALESCE(json_extract(h.source_json,'$.runtime_profile'),
                             (SELECT json_extract(s.config_json,'$.runtime_profile') FROM managed_configuration_sources s
                               WHERE s.deployment_id=h.deployment_id AND s.revision=h.revision))
               FROM host_effective_revisions h
              WHERE h.deployment_id=?1 AND h.revision=?2 AND h.outcome='resolved' ORDER BY h.host_id",
        )?
        .query_map(params![deployment_id, revision], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<Result<_, _>>()?;
    let ledger = read_snapshot(tx).map_err(resource)?;
    let pending = pending_charges(tx)?;
    let mut candidates = Vec::new();
    for (host, raw, profile) in hosts {
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
                reserve_absorbs_unmanaged: d.memory
                    == capyctl_config::effective::DomainMemory::Device,
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
        //
        // ADR 0028 §5, §11: a group's binding names only its head's ingress,
        // so every member host is read from the group's members: a member not
        // yet settled on its own host's evidence (reserved, dispatching,
        // launched or uncertain) holds that host as a retained launch does.
        // Uncertainty keeps accounting: an uncertain member still occupies it.
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
                              AND (?4=0 OR (?4=1 AND i.deployment_id=?2)))
                     OR EXISTS(SELECT 1 FROM group_members m
                            WHERE m.host_id=?1 AND m.state!='settled'
                              AND NOT (m.deployment_id=?2 AND m.instance_index=?3)
                              AND (?4=0 OR (?4=1 AND m.deployment_id=?2))))",
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
            super::startup::startup_footprint(tx, deployment_id, revision, &host, &e)?;
        let (device_options, preferred_device) =
            device_options(tx, deployment_id, revision, instance, &host)?;
        // ADR 0035 §1a: under `topology: auto` this host's N is chosen now,
        // from its cards' managed limits, and only that N's sets are options.
        let pick = match e.topology.as_ref().and_then(|t| t.auto.as_ref()) {
            Some(table) => auto_pick(
                tx,
                deployment_id,
                revision,
                &e,
                table,
                &limits,
                device_options,
                preferred_device,
            )?,
            None => AutoPick {
                tensor_parallel: e.local_tensor_parallel(),
                options: device_options,
                preferred: preferred_device,
                refusal: None,
            },
        };
        // ADR 0030 §1: a decisions revision is placed only on a host that
        // declared `decisions` (ADR 0017), as its latest session recorded it;
        // the live session's own declaration still gates the launch before
        // anything is sent. The embedded host is this build and serves them.
        // ADR 0031 §9: likewise a revision on a custom engine needs
        // `custom_engines`; a decisions custom engine needs both.
        let declared = |capability: &str| {
            tx.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_id=?1)
                     OR EXISTS(SELECT 1 FROM host_versions v, json_each(v.capabilities_json) c
                                WHERE v.host_id=?1 AND c.value=?2)",
                params![host, capability],
                |r| r.get::<_, bool>(0),
            )
        };
        let serves_api = (e.api.is_chat() || declared(capyctl_config::decisions::HOST_CAPABILITY)?)
            && (e.profile.engine != capyctl_config::engine_policy::Engine::Custom
                || declared(capyctl_config::custom::CAPABILITY)?);
        // ADR 0039 §§4, 15: a native TensorFold revision is placed only on a
        // host that declared `tensorfold_native`, and a host with `retention:
        // zero` refuses its compaction and disk-writing options.
        let native_gate = if e.profile.engine == capyctl_config::engine_policy::Engine::Tensorfold
            && e.profile.line.is_native()
        {
            use capyctl_config::tensorfold_native::{placement_refusal, HOST_CAPABILITY};
            let (_, host_retention) = host_lmcache_settings(tx, &host, &e)?;
            let has_native = declared(HOST_CAPABILITY)?;
            let args: Vec<String> = e
                .profile
                .args
                .iter()
                .chain(e.engine_config.extra_args())
                .cloned()
                .collect();
            placement_refusal(&args, host_retention, |name| {
                name == HOST_CAPABILITY && has_native
            })
        } else {
            None
        };
        // ADR 0033 §§9, 10, 13: `lmcache: required` and `retention: zero`
        // are placed only where they can run, judged again here against the
        // host's current published settings and latest declaration; the host
        // is refused with the typed code, not as a generic ineligible one.
        let gate = {
            use capyctl_config::lmcache::{placement_refusal, CAPABILITY, RETENTION_CAPABILITY};
            let (host_lmcache, host_retention) = host_lmcache_settings(tx, &host, &e)?;
            let has_lmcache = declared(CAPABILITY)?;
            let has_retention = declared(RETENTION_CAPABILITY)?;
            placement_refusal(
                e.lmcache,
                e.retention,
                host_lmcache,
                host_retention,
                |name| match name {
                    CAPABILITY => has_lmcache,
                    RETENTION_CAPABILITY => has_retention,
                    _ => false,
                },
            )
        }
        .map(|code| (code, None))
        .or(native_gate)
        // ADR 0035 §1a: `topology: auto` refused on this host for its GPUs.
        .or(pick.refusal)
        // ADR 0035 §11: a launch on several GPUs goes only to a host that
        // declared `local_tensor_parallel`; the live session's declaration
        // still gates it before anything is sent.
        .or((pick.tensor_parallel > 1
            && !declared(capyctl_config::topology::LOCAL_TP_CAPABILITY)?)
        .then_some((capyctl_config::topology::LOCAL_TP_CAPABILITY_MISSING, None)));
        // ADR 0033 §6, ADR 0025 amendment of 2026-10-23: a refusal by the
        // host-KV limit names the setting to raise (standalone's own on its
        // embedded host, the only host not enrolled) and what to lower: the
        // LMCache pool when the need holds more than the revision declares.
        let host_kv_remedy = {
            let kv = |f: &PhaseFootprint| -> i64 {
                f.allocations
                    .iter()
                    .map(|a| a.host_kv_bytes)
                    .fold(0, i64::saturating_add)
            };
            let declared = kv(&super::phase(&e.resources.cold, ResourcePhase::Cold));
            HostKvRemedy {
                standalone: !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_id=?1)",
                    [&host],
                    |r| r.get::<_, bool>(0),
                )?,
                lmcache_pool: kv(&footprint) > declared,
                declared: declared > 0,
            }
        };
        candidates.push(HostCandidate {
            gate: gate.as_ref().map(|(code, _)| *code),
            gate_detail: gate.and_then(|(_, detail)| detail),
            host_kv_remedy,
            device_options: pick.options,
            preferred_device: pick.preferred,
            occupied,
            whole_host,
            // ADR 0018 §4: nor while the deployment's profile is retiring on
            // this host, or absent from the host's current approved document.
            // Exclusion only: nothing already placed is stopped or released.
            eligible: serves_api
                && eligible.is_none_or(|set| set.contains(&host))
                && match &profile {
                    Some(profile) => {
                        crate::profile_retirement::profile_placeable(tx, &host, profile)?
                    }
                    None => true,
                },
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

/// ADR 0035 §§1a, 11, 13: the placement refusals of a deployment on several
/// GPUs of one host, reported with their own code like ADR 0033's gates,
/// never as a generic capacity block.
pub(super) const PLACEMENT_REFUSALS: [&str; 4] = [
    capyctl_config::topology::LOCAL_TP_CAPABILITY_MISSING,
    capyctl_scheduler::placement::INSUFFICIENT_GPUS,
    capyctl_scheduler::placement::INSUFFICIENT_DEVICE_MEMORY,
    "checkpoint_digest_pending",
];

/// What one host offers an instance once its tensor-parallel size is known.
struct AutoPick {
    /// The size it runs at on this host: the revision's own, or (ADR 0035
    /// §1a) the one `topology: auto` chose here.
    tensor_parallel: u32,
    /// The GPU sets of that size the host resolved the revision on.
    options: Vec<DeviceOption>,
    /// The instance's last set, when it is one of them.
    preferred: Option<String>,
    /// The closed refusal of this host and what it names, when `auto` can
    /// choose no size here.
    refusal: Option<(&'static str, Option<String>)>,
}

/// ADR 0035 §1a: choose N for a `topology: auto` revision on one host (`e`,
/// the host's own row, carries the candidate table): the smallest candidate
/// whose per-GPU charge, with the startup peak measured at that N when one
/// is recorded, every GPU of some set of N identical GPUs holds within its
/// managed limit now (`limits`, the host's current policy), never its free
/// memory: a busy card is evicted for at that N, it never pushes N up. Then
/// only that N's sets are placement options, and the last set only when it
/// is one of them. Weights not yet measured choose no N: the host is refused
/// `checkpoint_digest_pending`, the condition an unmeasured checkpoint waits
/// on (ADR 0014 §7). No size fitting refuses the host
/// `insufficient_device_memory` or `insufficient_gpus`, naming what would
/// fit ([`capyctl_config::effective::auto_refusal`]).
#[allow(clippy::too_many_arguments)]
fn auto_pick(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
    e: &capyctl_config::effective::EffectiveDeployment,
    table: &capyctl_config::effective::AutoTable,
    limits: &[MemoryLimit],
    mut options: Vec<DeviceOption>,
    preferred: Option<String>,
) -> Result<AutoPick, LifecycleError> {
    use capyctl_config::effective::{auto_gpus, auto_refusal, choose_tensor_parallel, AutoChoice};
    let mut gpus = auto_gpus(&e.host);
    if gpus.is_empty() {
        // A unified host has one GPU domain: `auto` is N = 1 there.
        return Ok(AutoPick {
            tensor_parallel: 1,
            options,
            preferred,
            refusal: None,
        });
    }
    for gpu in &mut gpus {
        if let Some(limit) = e
            .host
            .devices
            .get(&gpu.id)
            .and_then(|device| limits.iter().find(|l| l.domain == device.domain))
        {
            gpu.managed_limit = limit.managed_bytes;
        }
    }
    let mut peaks = std::collections::BTreeMap::new();
    for candidate in &table.candidates {
        let n = candidate.tensor_parallel;
        if let Some(peak) = super::startup::measured_device_peak(tx, deployment_id, revision, e, n)?
        {
            peaks.insert(n, peak);
        }
    }
    let measured = |n: u32| peaks.get(&n).copied();
    match choose_tensor_parallel(table, &gpus, &measured) {
        AutoChoice::Chosen {
            tensor_parallel, ..
        } => {
            if !options.is_empty() {
                options.retain(|o| o.domains.len() == tensor_parallel as usize);
            }
            let preferred = preferred.filter(|p| options.iter().any(|o| &o.device == p));
            Ok(AutoPick {
                tensor_parallel,
                options,
                preferred,
                refusal: None,
            })
        }
        AutoChoice::Unmeasured => Ok(AutoPick {
            tensor_parallel: 1,
            options,
            preferred,
            refusal: Some((
                "checkpoint_digest_pending",
                Some("topology: auto chooses its tensor-parallel size once the checkpoint's weights are measured".into()),
            )),
        }),
        AutoChoice::NoFit => {
            let (code, text) = auto_refusal(table, &gpus, &measured);
            Ok(AutoPick {
                tensor_parallel: 1,
                options,
                preferred,
                refusal: Some((code, Some(text))),
            })
        }
    }
}

/// ADR 0033 §§9, 10: `host`'s `lmcache` and `retention` now: as its current
/// approved publication states them, or for standalone's embedded host (it
/// publishes no approved document) as the role last started with them. The
/// settings a revision was resolved under (`e.host`) are read only for a
/// host with neither, which no running role is.
pub(super) fn host_lmcache_settings(
    tx: &rusqlite::Connection,
    host: &str,
    e: &capyctl_config::effective::EffectiveDeployment,
) -> Result<
    (
        capyctl_config::lmcache::HostLmcache,
        capyctl_config::lmcache::Retention,
    ),
    LifecycleError,
> {
    let published: Option<String> = tx
        .query_row(
            "SELECT config_json FROM approved_host_publications WHERE host_id=?1",
            [host],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(document) = published {
        return Ok(capyctl_config::lmcache::host_settings(
            &serde_json::from_str(&document).map_err(|_| LifecycleError::CorruptStoredData)?,
        ));
    }
    let embedded: Option<(String, String)> = tx
        .query_row(
            "SELECT lmcache,retention FROM embedded_host_settings WHERE host_id=?1",
            [host],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match embedded {
        Some((lmcache, retention)) => Ok((
            serde_json::from_value(serde_json::Value::String(lmcache))
                .map_err(|_| LifecycleError::CorruptStoredData)?,
            serde_json::from_value(serde_json::Value::String(retention))
                .map_err(|_| LifecycleError::CorruptStoredData)?,
        )),
        None => Ok((e.host.lmcache, e.host.retention)),
    }
}

/// ADR 0019 (discrete GPU design §7): the GPUs of `host` the instance may
/// run on, each with the startup footprint it resolved to there, and the GPU
/// it last ran on when that was on this host (ADR 0013 §4 step 5). Empty when
/// the host offered no GPU choice for the revision (a unified host, a pinned
/// device, explicit resources). ADR 0035 §2: on several GPUs each option is a
/// set, keyed by its GPUs in rank order, and the preference is the last set.
fn device_options(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
    instance: u32,
    host: &str,
) -> Result<(Vec<DeviceOption>, Option<String>), LifecycleError> {
    let rows: Vec<(String, String)> = tx
        .prepare(
            "SELECT device,effective_json FROM host_device_effective_revisions
              WHERE deployment_id=?1 AND revision=?2 AND host_id=?3 ORDER BY device",
        )?
        .query_map(params![deployment_id, revision, host], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut options = Vec::with_capacity(rows.len());
    for (device, raw) in rows {
        let on_device =
            decode_effective_snapshot(&raw).map_err(|_| LifecycleError::CorruptStoredData)?;
        let domains = capyctl_config::instances::set_ids(&device)
            .into_iter()
            .map(|id| on_device.host.devices.get(id).map(|p| p.domain.clone()))
            .collect::<Option<Vec<_>>>()
            .ok_or(LifecycleError::CorruptStoredData)?;
        let (footprint, _) =
            super::startup::startup_footprint(tx, deployment_id, revision, host, &on_device)?;
        options.push(DeviceOption {
            device,
            domains,
            footprint,
        });
    }
    if options.is_empty() {
        return Ok((options, None));
    }
    let preferred: Option<String> = tx
        .query_row(
            "SELECT COALESCE(device_set,device) FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2 AND host_id=?3",
            params![deployment_id, instance, host],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok((options, preferred))
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
    // MG14: measurement can refuse every host's provisional revision.
    // Check its recorded reason before placement turns the empty candidate
    // set into a generic capacity block. Retained and joined launches above
    // keep their existing behavior.
    crate::checkpoint_digests::admit_start(tx, deployment_id, current)?;
    // ADR 0028 §5: a group has fixed hosts; placement only validates that
    // its member fits on every one of them.
    if let Some(shape) = super::worker::group_shape(tx, deployment_id, current)? {
        return prepare_group(
            tx,
            deployment_id,
            instance,
            current,
            generation,
            &shape.hosts,
            eligible,
        );
    }
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
                Err(reclaimed) => {
                    return Ok(unplaced(unplaceable.code(), &reclaimed, &hosts, &owner))
                }
            }
        }
    };
    // ADR 0019: on a multi-GPU host, the revision as resolved on the GPU
    // placement chose; the instance records it with its host.
    let (raw, _) = frozen_on_device(
        tx,
        deployment_id,
        current,
        Some(&chosen.host_id),
        chosen.device.as_deref(),
    )?;
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
    // ADR 0035 §2: the chosen set in rank order and its size N, beside the
    // GPU of rank 0 (`device`, the single GPU's key as before). The set's key
    // is what its frozen revision is kept under on the host.
    let n = e.local_tensor_parallel();
    let (device, device_set) = match chosen.device.as_deref() {
        Some(key) if n > 1 => (
            capyctl_config::instances::set_ids(key)
                .first()
                .map(|id| (*id).to_owned()),
            Some(key.to_owned()),
        ),
        Some(key) => (Some(key.to_owned()), None),
        None if n > 1 => (
            None,
            Some(capyctl_config::instances::set_key(
                &e.selected_devices
                    .iter()
                    .map(|claim| claim.id.clone())
                    .collect::<Vec<_>>(),
            )),
        ),
        None => (None, None),
    };
    // ADR 0035 §1a: a start under `topology: auto` that chose a different N
    // than the instance's previous start journals both sizes.
    let (previous, auto): (Option<u32>, bool) = tx.query_row(
        "SELECT i.tensor_parallel,
                EXISTS(SELECT 1 FROM host_effective_revisions h
                        WHERE h.deployment_id=i.deployment_id AND h.revision=?3 AND h.host_id=?4
                          AND json_type(h.effective_json,'$.topology.auto')='object')
           FROM deployment_instances i WHERE i.deployment_id=?1 AND i.instance_index=?2",
        params![deployment_id, instance, current, chosen.host_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let changed = previous
        .filter(|previous| auto && *previous != n)
        .map(|previous| TopologyChange {
            instance,
            generation,
            host: chosen.host_id.clone(),
            previous,
            chosen: n,
            devices: e.selected_devices.iter().map(|d| d.id.clone()).collect(),
        });
    one(tx.execute(
        "UPDATE deployment_instances SET revision=?3,generation=?4,host_id=?5,device_json=?6,device=?7,
                device_set=?8,tensor_parallel=?9,placed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE deployment_id=?1 AND instance_index=?2",
        params![
            deployment_id,
            instance,
            current,
            generation,
            chosen.host_id,
            devices,
            device,
            device_set,
            n
        ],
    )?)?;
    Ok(Prepared::Placed(
        DeploymentFence {
            deployment_id: deployment_id.into(),
            revision: current,
            generation,
        },
        changed,
    ))
}

/// ADR 0035 §1a: an instance under `topology: auto` whose start chose a
/// tensor-parallel size other than its previous start's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TopologyChange {
    instance: u32,
    generation: i64,
    host: String,
    previous: u32,
    chosen: u32,
    devices: Vec<String>,
}

impl TopologyChange {
    /// Journal `topology_auto_changed` on the start `operation` that made the
    /// choice, with the management event of the same name, in the start's
    /// own transaction (ADR 0035 §13: an event, no exit code).
    pub(crate) fn record(
        &self,
        tx: &Transaction<'_>,
        deployment_id: &str,
        operation: &str,
    ) -> Result<(), LifecycleError> {
        let local = |id: &str| {
            id.rsplit('/')
                .next()
                .filter(|_| id.starts_with("host/"))
                .unwrap_or(id)
                .to_owned()
        };
        let devices: Vec<String> = self.devices.iter().map(|id| local(id)).collect();
        super::park::journal(
            tx,
            operation,
            "topology_auto_changed",
            &format!(
                "deployment {deployment_id}: instance {} under topology: auto chose tensor_parallel {} on host {} ({}), where its previous start chose {}",
                self.instance,
                self.chosen,
                self.host,
                devices.join(", "),
                self.previous
            ),
        )?;
        crate::events::append_event(
            tx,
            &crate::events::EventMetadata::TopologyAutoChanged {
                deployment_id: deployment_id.into(),
                instance_index: self.instance,
                generation: self.generation,
                host_id: self.host.clone(),
                previous_tensor_parallel: self.previous,
                tensor_parallel: self.chosen,
                devices,
            },
        )
        .map(drop)
        .map_err(|_| LifecycleError::CorruptStoredData)
    }
}

/// ADR 0028 §5: place group instance `instance` on its named `hosts` (rank
/// order): each host is judged for its own member's reservation (the cold
/// footprint the group's activation charges it), as that host's admission
/// sees it, and the start is accepted only when every host takes its member.
/// Nothing chooses a host and no host is recorded on the instance: the
/// group's activation reserves every member on its own host.
fn prepare_group(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instance: u32,
    revision: i64,
    generation: Option<i64>,
    hosts: &[String],
    eligible: Eligible<'_>,
) -> Result<Prepared, LifecycleError> {
    let owners: std::collections::BTreeMap<String, String> = hosts
        .iter()
        .enumerate()
        .map(|(rank, host)| {
            (
                host.clone(),
                crate::groups::member_owner_id(deployment_id, instance, rank as u32),
            )
        })
        .collect();
    let members = |reclaim_parked: bool| -> Result<Vec<HostCandidate>, LifecycleError> {
        let mut hosts = candidates(
            tx,
            deployment_id,
            revision,
            instance,
            eligible,
            reclaim_parked,
        )?;
        hosts.retain(|c| owners.contains_key(&c.host_id));
        for c in &mut hosts {
            let (raw, _) = frozen_on_host(tx, deployment_id, revision, Some(&c.host_id))?;
            let e =
                decode_effective_snapshot(&raw).map_err(|_| LifecycleError::CorruptStoredData)?;
            c.footprint = super::phase(&e.resources.cold, ResourcePhase::Cold);
            c.whole_host = false;
            c.device_options.clear();
        }
        Ok(hosts)
    };
    if let Err(unplaceable) = fits_group(&members(false)?, &owners) {
        // SPEC §6.5 (W5): only when nothing fits without it is parked
        // capacity counted as reclaimable.
        let reclaimed = members(true)?;
        if let Err(still) = fits_group(&reclaimed, &owners) {
            let head = hosts
                .first()
                .and_then(|h| owners.get(h))
                .cloned()
                .unwrap_or_default();
            return Ok(unplaced(unplaceable.code(), &still, &reclaimed, &head));
        }
    }
    let generation = match generation {
        Some(generation) => generation,
        None => draw_generation(tx, deployment_id)?,
    };
    one(tx.execute(
        "UPDATE deployment_instances SET revision=?3,generation=?4,
                placed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE deployment_id=?1 AND instance_index=?2",
        params![deployment_id, instance, revision, generation],
    )?)?;
    Ok(Prepared::Placed(
        DeploymentFence {
            deployment_id: deployment_id.into(),
            revision,
            generation,
        },
        None,
    ))
}

/// Record a capacity diagnostic (`reason`, a closed code with its detail
/// where it has one), and keep the start pending until `until`.
pub(crate) fn defer(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instance: u32,
    reason: &str,
    until: Option<i64>,
) -> Result<(), LifecycleError> {
    tx.execute(
        "UPDATE deployment_instances SET last_error=?3,
                pending_start_until_ms=CASE WHEN ?4 IS NULL THEN pending_start_until_ms ELSE ?4 END
          WHERE deployment_id=?1 AND instance_index=?2",
        params![
            deployment_id,
            instance,
            format!("placement: {reason}"),
            until
        ],
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
    /// ADR 0013 §4 step 1: the allowed hosts that resolved the deployment's
    /// current revision, the ones placement considers. Owner decision
    /// 2026-09-25: a start refused because none of them is eligible names them.
    pub fn resolved_hosts(&self, deployment_id: &str) -> Result<Vec<String>, LifecycleError> {
        Ok(self
            .conn
            .prepare(
                "SELECT h.host_id FROM host_effective_revisions h JOIN deployments d ON d.id=h.deployment_id
                  WHERE h.deployment_id=?1 AND h.revision=d.revision AND h.outcome='resolved' ORDER BY h.host_id",
            )?
            .query_map([deployment_id], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    }

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
        // SPEC §14: why the first instance that fit nowhere was refused.
        let mut detail: Option<String> = None;
        // Owner decision 2026-09-23: whether every instance that fit nowhere
        // was refused only because its solo first start needs an empty host.
        let mut needs_empty_host = true;
        // Owner decision 2026-09-25: whether every instance that fit nowhere
        // was refused only because no allowed host is eligible now. That is
        // reported as `HostIneligible`, never masked as a capacity block.
        let mut only_ineligible = true;
        // ADR 0033 §§9, 10, 13: the gate code every instance that fit nowhere
        // was refused with, when they all were; reported as that code, never
        // masked as a capacity block or a generic ineligible host.
        let mut gated: Option<Option<&'static str>> = None;
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
                    match Self::accept_start_in_transaction(
                        tx, s, &fence, now, deadline, true, None,
                    ) {
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
                Prepared::Placed(fence, changed) => {
                    match Self::accept_start_in_transaction(
                        tx, s, &fence, now, deadline, true, None,
                    )
                    .and_then(|start| {
                        if let Some(changed) = &changed {
                            changed.record(tx, deployment_id, &start.operation_id)?;
                        }
                        Ok(start)
                    }) {
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
                Prepared::Unplaceable(code, why, reason) => {
                    blocked = true;
                    detail.get_or_insert(why);
                    needs_empty_host &= code == "startup_requires_empty_host";
                    only_ineligible &= code == "host_ineligible";
                    let gate = (capyctl_config::lmcache::PLACEMENT_REFUSALS.contains(&code)
                        || capyctl_config::tensorfold_native::PLACEMENT_REFUSALS.contains(&code)
                        || PLACEMENT_REFUSALS.contains(&code))
                    .then_some(code);
                    gated = match gated {
                        None => Some(gate),
                        Some(seen) => Some(seen.filter(|_| seen == gate)),
                    };
                    let until = (scope != StartScope::OnDemand).then_some(deadline);
                    if !filling {
                        defer(tx, deployment_id, instance, &reason, until)?;
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
        let gate = gated.flatten();
        match first {
            Some(start) => Ok(start),
            None if blocked && refusal.is_none() && needs_empty_host => {
                Err(LifecycleError::StartupRequiresEmptyHost)
            }
            None if blocked && refusal.is_none() && only_ineligible => {
                Err(LifecycleError::HostIneligible)
            }
            None if blocked && refusal.is_none() && gate.is_some() => {
                Err(LifecycleError::PlacementRefused(format!(
                    "{}: no allowed host can run the deployment as declared ({})",
                    gate.unwrap_or_default(),
                    detail.unwrap_or_default()
                )))
            }
            None if blocked && refusal.is_none() => Err(LifecycleError::CapacityBlocked(detail)),
            None => Err(refusal.unwrap_or(LifecycleError::CapacityBlocked(detail))),
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

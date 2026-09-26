//! ADR 0007: resident floors for admission, attributed by process identity.
//!
//! Found live 2026-09-23 (matrix M33, host-a): a vLLM wake beside a ready
//! SGLang 14B was refused `insufficient resources` although 46 + 24 GiB fit
//! the 97 GiB limit. The host's published availability already excluded what
//! the ready engine held, and admission charged its whole reservation again,
//! because no admission path credited resident memory: every engine resident
//! on a host was counted twice.
//!
//! A floor is a verified lower bound: the memory the host sampled for the
//! exact processes (pid, boot id and start ticks) of a runtime this store
//! recorded, beside the same availability sample, capped at the owner's
//! reservation. Only a Ready owner with no lifecycle run in flight is
//! credited: its footprint is settled, so the sample reflects it, and a
//! transitioning owner keeps its full charge. A settled parked owner whose
//! processes are sampled alive is credited what they hold on a discrete
//! host's `device` and `distinct` domains; it keeps its full charge on a
//! `unified` domain. The candidate is
//! never credited. A domain whose floors would exceed the memory in use
//! gets none (fail closed), as does anything without a sample.
//!
//! ADR 0019: a discrete host holds an engine's weights in a device domain and
//! its host pages in a system domain, so each allocation of a footprint is
//! credited from the figure sampled for its own domain's kind: GPU bytes on a
//! `device` domain, anonymous and shared pages on a `distinct` one, their sum on a
//! `unified` one. Skipping a multi-domain owner would bring back the double
//! counting of M33 on every discrete host.
use std::collections::BTreeMap;

use mllm_config::effective::DomainMemory;
use mllm_config::resource_controls::ResourceControls;
use mllm_domain::resources::{
    LedgerSnapshot, MemoryObservation, ProcessResident, ResidentFloor, ResourcePhase,
};
use rusqlite::Connection;
use serde::Deserialize;

#[derive(Deserialize)]
struct Identity {
    pid: u32,
    boot_id: String,
    start_ticks: u64,
}

/// The memory kind of each domain the host policy `controls` declares.
pub(crate) fn domain_kinds(controls: &ResourceControls) -> BTreeMap<String, DomainMemory> {
    controls
        .domains
        .iter()
        .map(|(id, domain)| (id.clone(), domain.memory))
        .collect()
}

/// The figure of `resident` that counts against a domain of `kind`.
fn credited_bytes(resident: &ProcessResident, kind: DomainMemory) -> i64 {
    match kind {
        DomainMemory::Device => resident.device_bytes,
        DomainMemory::Distinct => resident.host_bytes,
        // ADR 0007: one pool holds both.
        DomainMemory::Unified => resident.bytes,
    }
}

/// The floors `residents` support for the owners `scoped` holds, for an
/// admission of `candidate` judged against `observations`, each allocation
/// credited by the memory kind `kinds` gives its domain.
pub(crate) fn resident_floors(
    conn: &Connection,
    scoped: &LedgerSnapshot,
    candidate: &str,
    observations: &[MemoryObservation],
    residents: &[ProcessResident],
    kinds: &BTreeMap<String, DomainMemory>,
) -> rusqlite::Result<Vec<ResidentFloor>> {
    if residents.is_empty() {
        return Ok(Vec::new());
    }
    let sampled: BTreeMap<(u32, &str, u64), &ProcessResident> = residents
        .iter()
        .map(|p| ((p.pid, p.boot_id.as_str(), p.start_ticks), p))
        .collect();
    let rows: Vec<(String, u32, String)> = conn
        .prepare(
            "SELECT b.deployment_id,b.instance_index,b.identities_json FROM runtime_bindings b
               JOIN deployment_instances i ON i.deployment_id=b.deployment_id AND i.instance_index=b.instance_index
              WHERE b.state='live' AND i.observed_state IN ('ready','parked')
                AND NOT EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id
                       AND r.instance_index=i.instance_index AND r.state IN ('queued','running','uncertain'))
              ORDER BY b.deployment_id,b.instance_index",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut floors = Vec::new();
    for (deployment, instance, identities) in rows {
        let owner = crate::instances::instance_owner_id(&deployment, instance);
        if owner == candidate {
            continue;
        }
        let Some(footprint) = scoped.owners.get(&owner) else {
            continue;
        };
        // Found live on a 16 GB discrete GPU: a parked engine's residue on
        // the card and its host_backed copy in host RAM are in use too, and
        // charging them again stopped every model parked beside a start. A
        // settled park is credited on `device` and `distinct` domains only; a
        // `unified` domain keeps the parked charge (ADR 0007 unchanged).
        let parked = match footprint.phase {
            ResourcePhase::Ready => false,
            ResourcePhase::Parked => true,
            _ => continue,
        };
        let Ok(identities) = serde_json::from_str::<Vec<Identity>>(&identities) else {
            continue;
        };
        let processes: Vec<&ProcessResident> = identities
            .iter()
            .filter_map(|i| {
                sampled
                    .get(&(i.pid, i.boot_id.as_str(), i.start_ticks))
                    .copied()
            })
            .collect();
        for allocation in &footprint.allocations {
            let Some(observation) = observations.iter().find(|o| o.domain == allocation.domain)
            else {
                continue;
            };
            // A domain the policy does not describe has no known source for
            // its figure: no credit (fail closed).
            let Some(&kind) = kinds.get(&allocation.domain) else {
                continue;
            };
            if parked && kind == DomainMemory::Unified {
                continue;
            }
            let resident = processes
                .iter()
                .try_fold(0_i64, |sum, p| sum.checked_add(credited_bytes(p, kind)));
            let bytes = resident.unwrap_or(0).min(allocation.bytes);
            if bytes > 0 {
                floors.push(ResidentFloor {
                    owner: owner.clone(),
                    domain: allocation.domain.clone(),
                    bytes,
                    sampled_at_ms: observation.sampled_at_ms,
                });
            }
        }
    }
    // Never credit more than the domain has in use.
    for observation in observations {
        let in_use = observation.capacity_bytes - observation.available_bytes;
        let credited = floors
            .iter()
            .filter(|f| f.domain == observation.domain)
            .try_fold(0_i64, |sum, f| sum.checked_add(f.bytes));
        if credited.is_none_or(|total| total > in_use) {
            floors.retain(|f| f.domain != observation.domain);
        }
    }
    Ok(floors)
}

/// A park's or a wake's own charge as a floor (found live on a 16 GB discrete
/// GPU): the memory an owner holds is already in use, so the host's free
/// memory does not cover it a second time. The M27 rule skips the free-memory
/// check only when a transition adds nothing; a host_backed park adds the
/// weights copy on the system domain while keeping its GPU charge, and without
/// this its whole footprint was charged against the card's free memory again,
/// as was a wake's parked residue. Each held allocation on a `device` or
/// `distinct` domain is credited, capped at what the domain has in use beside
/// the floors already credited there, so the transition is charged only what
/// it adds. A `unified` domain is left exactly as it was (ADR 0007).
pub(crate) fn credit_own_charge(
    floors: &mut Vec<ResidentFloor>,
    owner: &str,
    held: &mllm_domain::resources::PhaseFootprint,
    observations: &[MemoryObservation],
    kinds: &BTreeMap<String, DomainMemory>,
) {
    for allocation in &held.allocations {
        if !matches!(
            kinds.get(&allocation.domain),
            Some(DomainMemory::Device | DomainMemory::Distinct)
        ) {
            continue;
        }
        let Some(observation) = observations.iter().find(|o| o.domain == allocation.domain) else {
            continue;
        };
        floors.retain(|f| !(f.owner == owner && f.domain == allocation.domain));
        let credited = floors
            .iter()
            .filter(|f| f.domain == allocation.domain)
            .try_fold(0_i64, |sum, f| sum.checked_add(f.bytes));
        let in_use = observation.capacity_bytes - observation.available_bytes;
        let Some(room) = credited.and_then(|c| in_use.checked_sub(c)) else {
            continue;
        };
        let bytes = allocation.bytes.min(room);
        if bytes > 0 {
            floors.push(ResidentFloor {
                owner: owner.to_owned(),
                domain: allocation.domain.clone(),
                bytes,
                sampled_at_ms: observation.sampled_at_ms,
            });
        }
    }
}

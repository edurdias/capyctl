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
//! parked or transitioning owner keeps its full charge. The candidate is
//! never credited. A domain whose floors would exceed the memory in use
//! gets none (fail closed), as does anything without a sample.
use std::collections::BTreeMap;

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

/// The floors `residents` support for the owners `scoped` holds, for an
/// admission of `candidate` judged against `observations`.
pub(crate) fn resident_floors(
    conn: &Connection,
    scoped: &LedgerSnapshot,
    candidate: &str,
    observations: &[MemoryObservation],
    residents: &[ProcessResident],
) -> rusqlite::Result<Vec<ResidentFloor>> {
    if residents.is_empty() {
        return Ok(Vec::new());
    }
    let sampled: BTreeMap<(u32, &str, u64), i64> = residents
        .iter()
        .map(|p| ((p.pid, p.boot_id.as_str(), p.start_ticks), p.bytes))
        .collect();
    let rows: Vec<(String, u32, String)> = conn
        .prepare(
            "SELECT b.deployment_id,b.instance_index,b.identities_json FROM runtime_bindings b
               JOIN deployment_instances i ON i.deployment_id=b.deployment_id AND i.instance_index=b.instance_index
              WHERE b.state='live' AND i.observed_state='ready'
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
        let [allocation] = footprint.allocations.as_slice() else {
            continue;
        };
        if footprint.phase != ResourcePhase::Ready {
            continue;
        }
        let Some(observation) = observations.iter().find(|o| o.domain == allocation.domain) else {
            continue;
        };
        let Ok(identities) = serde_json::from_str::<Vec<Identity>>(&identities) else {
            continue;
        };
        let resident = identities
            .iter()
            .filter_map(|i| sampled.get(&(i.pid, i.boot_id.as_str(), i.start_ticks)))
            .try_fold(0_i64, |sum, bytes| sum.checked_add(*bytes));
        let bytes = resident.unwrap_or(0).min(allocation.bytes);
        if bytes > 0 {
            floors.push(ResidentFloor {
                owner,
                domain: allocation.domain.clone(),
                bytes,
                sampled_at_ms: observation.sampled_at_ms,
            });
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

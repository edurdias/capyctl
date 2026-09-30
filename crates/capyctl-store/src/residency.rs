//! Read-only residency sources for a runtime binding.
//!
//! These answer two questions an observer cannot answer from the host alone: which
//! lifecycle milestones this incarnation has actually committed, and how much
//! registered work the controller still owns. Both are plain reads. Neither grants
//! authority, arms a step, or settles a lease.

use capyctl_domain::completion::Milestone;
use rusqlite::Connection;

use crate::lifecycle::LifecycleError;

/// A runtime performs a bounded number of transitions before it is released, and a
/// release restarts the sequence. A binding that has accumulated more committed
/// evidence rows than this is not a runtime this observer can reason about.
const MAX_EVIDENCE_ROWS: usize = 4096;
const MAX_EVIDENCE_BYTES: usize = 64 * 1024;

#[derive(serde::Deserialize)]
struct Milestones {
    milestones: Vec<Fact>,
}

/// Deliberately a separate, minimal shape. Residency reads only the milestone list,
/// so it must not fail when an unrelated evidence field is added, and must not gain
/// access to identities, tokens or receipts it has no reason to see.
#[derive(serde::Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Fact {
    Quiesced,
    MemoryReleased,
    AllocationsRestored,
    WeightsUsable,
    CacheValid,
    ModelUsable,
}

impl From<Fact> for Milestone {
    fn from(f: Fact) -> Self {
        match f {
            Fact::Quiesced => Self::Quiesced,
            Fact::MemoryReleased => Self::MemoryReleased,
            Fact::AllocationsRestored => Self::AllocationsRestored,
            Fact::WeightsUsable => Self::WeightsUsable,
            Fact::CacheValid => Self::CacheValid,
            Fact::ModelUsable => Self::ModelUsable,
        }
    }
}

/// Committed milestones for one incarnation, in commit order.
///
/// Ordering is by `committed_epoch`, which is the order the evidence was actually
/// committed, not the order steps were planned. A planned or armed step has decided
/// nothing, so only completed steps with committed evidence are read.
///
/// The incarnation must match the binding. A caller holding a stale incarnation is
/// asking about a runtime that no longer exists, and receives `NotFound` rather than
/// the current runtime's history.
fn milestones(
    conn: &Connection,
    binding_id: &str,
    incarnation: &str,
) -> Result<Vec<Milestone>, LifecycleError> {
    if binding_id.is_empty() || incarnation.is_empty() {
        return Err(LifecycleError::Invalid);
    }
    let matches: i64 = conn.query_row(
        "SELECT COUNT(*) FROM runtime_bindings WHERE id=?1 AND incarnation=?2",
        (binding_id, incarnation),
        |r| r.get(0),
    )?;
    if matches != 1 {
        return Err(LifecycleError::NotFound);
    }
    let mut statement = conn.prepare(
        "SELECT e.evidence_json FROM lifecycle_evidence e \
         JOIN lifecycle_steps s ON s.id=e.step_id \
         WHERE s.binding_id=?1 AND s.state='completed' \
         ORDER BY e.committed_epoch ASC, s.ordinal ASC",
    )?;
    let rows = statement.query_map((binding_id,), |r| r.get::<_, String>(0))?;
    let mut facts = Vec::new();
    for (seen, row) in rows.enumerate() {
        if seen >= MAX_EVIDENCE_ROWS {
            return Err(LifecycleError::CorruptStoredData);
        }
        let json = row?;
        if json.len() > MAX_EVIDENCE_BYTES {
            return Err(LifecycleError::CorruptStoredData);
        }
        let parsed: Milestones =
            serde_json::from_str(&json).map_err(|_| LifecycleError::CorruptStoredData)?;
        facts.extend(parsed.milestones.into_iter().map(Milestone::from));
    }
    Ok(facts)
}

/// Request leases the controller still owns for this deployment.
///
/// Both `inflight` and `uncertain` leases count. Uncertain work has not been proven
/// to have terminated, and treating it as finished is exactly the unevidenced
/// release the lifecycle rules forbid.
fn outstanding(conn: &Connection, deployment_id: &str) -> Result<usize, LifecycleError> {
    if deployment_id.is_empty() {
        return Err(LifecycleError::Invalid);
    }
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1",
        (deployment_id,),
        |r| r.get(0),
    )?;
    usize::try_from(count).map_err(|_| LifecycleError::CorruptStoredData)
}

impl crate::Store {
    /// Committed lifecycle milestones for this incarnation, in commit order.
    pub fn committed_milestones(
        &self,
        binding_id: &str,
        incarnation: &str,
    ) -> Result<Vec<Milestone>, LifecycleError> {
        milestones(&self.conn, binding_id, incarnation)
    }

    /// Registered request leases still outstanding for this deployment.
    pub fn outstanding_requests(&self, deployment_id: &str) -> Result<usize, LifecycleError> {
        outstanding(&self.conn, deployment_id)
    }
}

#[cfg(test)]
mod tests;

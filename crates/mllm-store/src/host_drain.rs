//! SPEC §4.3: explicit draining of one host's deployments, as distinct from an
//! ordinary role restart.
//!
//! A drain names every deployment that still holds a runtime on the host; the
//! caller then stops each through the ordinary Stop, whose cleanup completes only
//! on evidence that the recorded processes are gone. Nothing here releases or
//! stops anything by itself.
//!
//! Ordering (router review item 14): a drain of an enrolled host first writes
//! its intent in the same transaction that enumerates the host's instances
//! (`begin_host_drain`), so the host is out of placement before any Stop is
//! issued; it then records the Stops (`record_host_drain`), enumerates again as
//! a safety net, and completes the intent (`complete_host_drain`). The host is
//! held while an intent is open or any recorded Stop is unsettled.
use crate::{Store, StoreError};
use rusqlite::params;

/// One deployment instance holding a runtime on the drained host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrainCandidate {
    pub deployment_id: String,
    /// The deployment's declared revision, which the stop command names.
    pub revision: i64,
    /// ADR 0013 §5: the instance on the drained host; a drain stops only it,
    /// never the deployment's instances on other hosts.
    pub instance: u32,
}

/// Which host a drain addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrainHost<'a> {
    /// An enrolled remote host, by its host id.
    Remote(&'a str),
    /// The host embedded in this process (standalone).
    Embedded,
}

/// The most deployments one drain names; a host carrying more is refused rather
/// than half drained.
const MAX_CANDIDATES: usize = 4096;

impl Store {
    /// Every model deployment with a runtime binding on `host` that has not been
    /// released, whatever its state: an uncertain launch still occupies the host
    /// and is exactly what a drain must account for.
    pub fn drain_candidates(&self, host: DrainHost<'_>) -> Result<Vec<DrainCandidate>, StoreError> {
        let remote = match host {
            DrainHost::Remote(id) => Some(id),
            DrainHost::Embedded => None,
        };
        let mut query = self.conn.prepare(
            "SELECT DISTINCT d.id, d.revision, b.instance_index FROM runtime_bindings b
             JOIN deployments d ON d.id=b.deployment_id
             WHERE b.state!='released' AND d.kind='model' AND (
               (?1 IS NULL AND NOT EXISTS(SELECT 1 FROM remote_binding_ingress r WHERE r.binding_id=b.id))
               OR EXISTS(SELECT 1 FROM remote_binding_ingress r WHERE r.binding_id=b.id AND r.host_id=?1))
             ORDER BY d.id, b.instance_index LIMIT ?2",
        )?;
        let candidates = query
            .query_map(params![remote, (MAX_CANDIDATES + 1) as i64], |r| {
                Ok(DrainCandidate {
                    deployment_id: r.get(0)?,
                    revision: r.get(1)?,
                    instance: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if candidates.len() > MAX_CANDIDATES {
            return Err(StoreError::Conflict);
        }
        Ok(candidates)
    }

    /// Router review item 14: open the drain `key` of the enrolled host `host`
    /// and name its instances, in one transaction. From the commit on, the host
    /// takes no new placements (`host_drain_pending`), so every instance placed
    /// on it before is in the returned list and none is placed after, even
    /// before the first Stop exists. A retried drain under the same key reopens
    /// its intent. Intents already completed on the host are pruned here.
    pub fn begin_host_drain(
        &self,
        host: &str,
        key: &str,
        now_ms: i64,
    ) -> Result<Vec<DrainCandidate>, StoreError> {
        if host.is_empty() || key.is_empty() || now_ms < 0 {
            return Err(StoreError::Conflict);
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM host_drain_intents WHERE host_id=?1 AND drain_key!=?2
               AND completed_at_ms IS NOT NULL",
            params![host, key],
        )?;
        tx.execute(
            "INSERT INTO host_drain_intents(host_id,drain_key,recorded_at_ms,completed_at_ms)
               VALUES(?1,?2,?3,NULL)
             ON CONFLICT(host_id,drain_key) DO UPDATE SET completed_at_ms=NULL",
            params![host, key, now_ms],
        )?;
        let candidates = self.drain_candidates(DrainHost::Remote(host))?;
        tx.commit()?;
        Ok(candidates)
    }

    /// Router review item 14: the drain `key` of `host` has issued and recorded
    /// every Stop it will. Every open intent on the host is completed: this
    /// drain's enumeration ran after each of them was written, so it named
    /// every instance they would have. The host stays held until the recorded
    /// Stops settle. Completing an unknown drain changes nothing.
    pub fn complete_host_drain(
        &self,
        host: &str,
        key: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        if host.is_empty() || key.is_empty() || now_ms < 0 {
            return Err(StoreError::Conflict);
        }
        let tx = self.conn.unchecked_transaction()?;
        let known: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM host_drain_intents WHERE host_id=?1 AND drain_key=?2)",
            params![host, key],
            |r| r.get(0),
        )?;
        if known {
            tx.execute(
                "UPDATE host_drain_intents SET completed_at_ms=?2
                   WHERE host_id=?1 AND completed_at_ms IS NULL",
                params![host, now_ms],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Owner decision 4 (2026-09-22): record the durable marker of a drain of
    /// `host`, naming every Stop it issued. The drain is pending while any of
    /// them is not terminal (`host_drain_pending`), however long the host stays
    /// offline; the marker never releases, stops or settles anything itself.
    /// Recording the same operations again (a retried drain) changes nothing.
    /// Rows of drains already settled on this host are removed here, so the
    /// table holds only what can still matter.
    pub fn record_host_drain(
        &self,
        host: &str,
        operations: &[String],
        now_ms: i64,
    ) -> Result<(), StoreError> {
        if host.is_empty()
            || operations.is_empty()
            || operations.len() > MAX_CANDIDATES
            || now_ms < 0
        {
            return Err(StoreError::Conflict);
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            &format!(
                "DELETE FROM host_drains WHERE host_id=?1 AND operation_id IN
                   (SELECT id FROM operations WHERE state IN ({TERMINAL}))"
            ),
            params![host],
        )?;
        for operation in operations {
            let known: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)",
                params![operation],
                |r| r.get(0),
            )?;
            if !known {
                return Err(StoreError::Conflict);
            }
            tx.execute(
                "INSERT OR IGNORE INTO host_drains(host_id,operation_id,recorded_at_ms) VALUES(?1,?2,?3)",
                params![host, operation, now_ms],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Owner decision 4: whether a drain of `host` still has a Stop that has not
    /// settled, or (router review item 14) an intent not yet completed. A host
    /// with a pending drain is not a placement candidate (ADR 0013 §4 step 1,
    /// W12 eligibility), whether or not it is connected.
    pub fn host_drain_pending(&self, host: &str) -> Result<bool, StoreError> {
        Ok(self.conn.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM host_drains h JOIN operations o ON o.id=h.operation_id
                   WHERE h.host_id=?1 AND o.state NOT IN ({TERMINAL}))
                 OR EXISTS(SELECT 1 FROM host_drain_intents
                   WHERE host_id=?1 AND completed_at_ms IS NULL)"
            ),
            params![host],
            |r| r.get(0),
        )?)
    }

    /// Every host with a pending drain (see `host_drain_pending`).
    pub fn hosts_with_pending_drain(
        &self,
    ) -> Result<std::collections::BTreeSet<String>, StoreError> {
        let mut query = self.conn.prepare(&format!(
            "SELECT h.host_id FROM host_drains h JOIN operations o ON o.id=h.operation_id
               WHERE o.state NOT IN ({TERMINAL})
             UNION
             SELECT host_id FROM host_drain_intents WHERE completed_at_ms IS NULL"
        ))?;
        let hosts = query
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(hosts)
    }
}

/// The operation states in which a Stop has settled.
const TERMINAL: &str = "'succeeded','failed','cancelled'";

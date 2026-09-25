//! ADR 0018 §4: removing a published runtime profile from a host, in two
//! phases, so no placement slips in between the check and the removal. The
//! retirement row is written in the transaction that names the instances
//! still using the profile; placement excludes (host, profile) from then on
//! (`profile_placeable`). Nothing here stops, releases or settles anything:
//! stops go through the ordinary path, and a retirement is confirmed only on
//! their evidence (spec design rule 4).
use crate::host_drain::DrainCandidate;
use crate::{Store, StoreError};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

/// One instance of the profile holding a runtime on the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetirementCandidate {
    pub deployment_id: String,
    pub name: String,
    pub revision: i64,
    pub instance: u32,
}

impl RetirementCandidate {
    /// The ordinary stop this instance needs (the drain path's shape).
    pub fn drain(&self) -> DrainCandidate {
        DrainCandidate {
            deployment_id: self.deployment_id.clone(),
            revision: self.revision,
            instance: self.instance,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetirementStart {
    /// Nothing uses the profile on the host: confirmed.
    Clear,
    /// Refused without drain; the retirement was cancelled in the same
    /// transaction, so placements on the profile resume.
    InUse(Vec<RetirementCandidate>),
    /// Drain requested: these need stopping; the retirement stands.
    Draining(Vec<RetirementCandidate>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetirementProgress {
    /// Stops still unsettled, or runtimes still held: keep waiting.
    Waiting(Vec<String>),
    /// Every stop succeeded and nothing holds a runtime: confirmed.
    Settled,
    /// A stop ended without success; the retirement ended unconfirmed.
    Holding(Vec<String>),
    /// The deadline passed first; the retirement ended unconfirmed.
    Expired(Vec<String>),
    /// No retirement under this key (cancelled, expired or replaced).
    Gone,
}

/// The most instances one retirement names; more is refused, never half done.
const MAX_CANDIDATES: usize = 4096;

/// The profile name a binding's revision was resolved with on `host`.
const PROFILE_OF: &str = "COALESCE(
    (SELECT json_extract(h.source_json,'$.runtime_profile') FROM host_effective_revisions h
      WHERE h.deployment_id=b.deployment_id AND h.revision=b.revision AND h.host_id=?1 AND h.source_json IS NOT NULL),
    (SELECT json_extract(s.config_json,'$.runtime_profile') FROM managed_configuration_sources s
      WHERE s.deployment_id=b.deployment_id AND s.revision=b.revision))";

fn candidates(
    conn: &Connection,
    host: &str,
    profile: &str,
) -> Result<Vec<RetirementCandidate>, StoreError> {
    let sql = format!(
        "SELECT DISTINCT d.id, d.name, b.revision, b.instance_index FROM runtime_bindings b
           JOIN deployments d ON d.id=b.deployment_id
          WHERE b.state!='released' AND d.kind='model'
            AND (EXISTS(SELECT 1 FROM deployment_instances i WHERE i.deployment_id=b.deployment_id
                          AND i.instance_index=b.instance_index AND i.host_id=?1)
                 OR EXISTS(SELECT 1 FROM remote_binding_ingress r WHERE r.binding_id=b.id AND r.host_id=?1))
            AND {PROFILE_OF}=?2
          ORDER BY d.id, b.instance_index LIMIT ?3"
    );
    let found = conn
        .prepare(&sql)?
        .query_map(params![host, profile, (MAX_CANDIDATES + 1) as i64], |r| {
            Ok(RetirementCandidate {
                deployment_id: r.get(0)?,
                name: r.get(1)?,
                revision: r.get(2)?,
                instance: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if found.len() > MAX_CANDIDATES {
        return Err(StoreError::Conflict);
    }
    Ok(found)
}

/// ADR 0018 §4: whether a new instance of `profile` may be placed on `host`.
/// Not while a retirement of it stands, nor when the host's approved
/// publication exists and no longer carries the profile. A host with no
/// publication row (the embedded host) is judged by retirements alone.
pub(crate) fn profile_placeable(
    tx: &Transaction<'_>,
    host: &str,
    profile: &str,
) -> Result<bool, rusqlite::Error> {
    let retiring: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM profile_retirements WHERE host_id=?1 AND profile=?2)",
        params![host, profile],
        |r| r.get(0),
    )?;
    if retiring {
        return Ok(false);
    }
    let published: Option<String> = tx
        .query_row(
            "SELECT config_json FROM approved_host_publications WHERE host_id=?1",
            [host],
            |r| r.get(0),
        )
        .optional()?;
    Ok(published.is_none_or(|json| {
        serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .is_some_and(|doc| doc["runtime_profiles"].get(profile).is_some())
    }))
}

fn valid(host: &str, profile: &str, key: &str) -> bool {
    !host.is_empty()
        && host.len() <= 128
        && !profile.is_empty()
        && profile.len() <= 64
        && !key.is_empty()
        && key.len() <= 128
}

impl Store {
    /// ADR 0018 §4, phase one: write the retirement and name the instances
    /// that use the profile on the host, in one transaction. A retried
    /// request under the same key reuses its row; another key while one
    /// stands is refused (`Conflict`).
    pub fn begin_profile_retirement(
        &self,
        host: &str,
        profile: &str,
        key: &str,
        now_ms: i64,
        deadline_ms: i64,
        drain: bool,
    ) -> Result<RetirementStart, StoreError> {
        if !valid(host, profile, key) || now_ms < 0 || deadline_ms < now_ms {
            return Err(StoreError::Conflict);
        }
        let tx = self.conn.unchecked_transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT retire_key FROM profile_retirements WHERE host_id=?1 AND profile=?2",
                params![host, profile],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(other) if other != key => return Err(StoreError::Conflict),
            Some(_) => {}
            None => {
                tx.execute(
                    "INSERT INTO profile_retirements(host_id,profile,retire_key,state,recorded_at_ms,deadline_ms)
                     VALUES(?1,?2,?3,'retiring',?4,?5)",
                    params![host, profile, key, now_ms, deadline_ms],
                )?;
            }
        }
        let named = candidates(&tx, host, profile)?;
        let start = if named.is_empty() {
            tx.execute(
                "UPDATE profile_retirements SET state='confirmed' WHERE host_id=?1 AND profile=?2",
                params![host, profile],
            )?;
            RetirementStart::Clear
        } else if drain {
            RetirementStart::Draining(named)
        } else {
            tx.execute(
                "DELETE FROM profile_retirements WHERE host_id=?1 AND profile=?2",
                params![host, profile],
            )?;
            RetirementStart::InUse(named)
        };
        tx.commit()?;
        Ok(start)
    }

    /// The instances of `profile` holding a runtime on `host` now.
    pub fn profile_candidates(
        &self,
        host: &str,
        profile: &str,
    ) -> Result<Vec<RetirementCandidate>, StoreError> {
        candidates(&self.conn, host, profile)
    }

    /// Record the ordinary stops a drained retirement issued.
    pub fn record_profile_retirement_stops(
        &self,
        host: &str,
        profile: &str,
        key: &str,
        operations: &[String],
    ) -> Result<(), StoreError> {
        if operations.is_empty() || !valid(host, profile, key) {
            return Err(StoreError::Conflict);
        }
        let tx = self.conn.unchecked_transaction()?;
        let ours: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM profile_retirements WHERE host_id=?1 AND profile=?2 AND retire_key=?3)",
            params![host, profile, key],
            |r| r.get(0),
        )?;
        if !ours {
            return Err(StoreError::Conflict);
        }
        for operation in operations {
            tx.execute(
                "INSERT OR IGNORE INTO profile_retirement_stops(host_id,profile,operation_id) VALUES(?1,?2,?3)",
                params![host, profile, operation],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// ADR 0018 §4: confirmed only when every recorded stop succeeded and a
    /// fresh enumeration finds nothing; a stop that ended otherwise, or the
    /// deadline, ends the retirement unconfirmed. Never confirms on a guess.
    pub fn profile_retirement_progress(
        &self,
        host: &str,
        profile: &str,
        key: &str,
        now_ms: i64,
    ) -> Result<RetirementProgress, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let row: Option<(String, i64)> = tx
            .query_row(
                "SELECT state, deadline_ms FROM profile_retirements WHERE host_id=?1 AND profile=?2 AND retire_key=?3",
                params![host, profile, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((state, deadline)) = row else {
            return Ok(RetirementProgress::Gone);
        };
        if state == "confirmed" {
            return Ok(RetirementProgress::Settled);
        }
        let named: Vec<String> = candidates(&tx, host, profile)?
            .into_iter()
            .map(|c| c.name)
            .collect();
        let unsuccessful: i64 = tx.query_row(
            "SELECT COUNT(*) FROM profile_retirement_stops s JOIN operations o ON o.id=s.operation_id
              WHERE s.host_id=?1 AND s.profile=?2 AND o.state IN ('failed','cancelled')",
            params![host, profile],
            |r| r.get(0),
        )?;
        let open: i64 = tx.query_row(
            "SELECT COUNT(*) FROM profile_retirement_stops s JOIN operations o ON o.id=s.operation_id
              WHERE s.host_id=?1 AND s.profile=?2 AND o.state NOT IN ('succeeded','failed','cancelled')",
            params![host, profile],
            |r| r.get(0),
        )?;
        let progress = if unsuccessful > 0 {
            tx.execute(
                "DELETE FROM profile_retirements WHERE host_id=?1 AND profile=?2",
                params![host, profile],
            )?;
            RetirementProgress::Holding(named)
        } else if open == 0 && named.is_empty() {
            tx.execute(
                "UPDATE profile_retirements SET state='confirmed' WHERE host_id=?1 AND profile=?2",
                params![host, profile],
            )?;
            RetirementProgress::Settled
        } else if now_ms >= deadline {
            tx.execute(
                "DELETE FROM profile_retirements WHERE host_id=?1 AND profile=?2",
                params![host, profile],
            )?;
            RetirementProgress::Expired(named)
        } else {
            RetirementProgress::Waiting(named)
        };
        tx.commit()?;
        Ok(progress)
    }

    /// Cancel a retirement (placements on the profile resume). Idempotent.
    pub fn cancel_profile_retirement(
        &self,
        host: &str,
        profile: &str,
        key: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM profile_retirements WHERE host_id=?1 AND profile=?2 AND retire_key=?3",
            params![host, profile, key],
        )?;
        Ok(())
    }

    /// The standing retirement of (host, profile): key, state, deadline.
    pub fn profile_retirement(
        &self,
        host: &str,
        profile: &str,
    ) -> Result<Option<(String, String, i64)>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT retire_key, state, deadline_ms FROM profile_retirements WHERE host_id=?1 AND profile=?2",
                params![host, profile],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    /// ADR 0018 §4 (owner decision 2026-09-25): a retirement abandoned past its
    /// deadline (a server that stopped mid-removal) ends unconfirmed, so it
    /// cannot hold a profile out of placement for ever. Returns what ended.
    /// Only a retirement still `retiring` is abandoned: a confirmed one stays
    /// (keeping the profile out of placement) until the host's re-publication
    /// drops the profile, which clears it (`republish_host_configuration`).
    pub fn expire_profile_retirements(
        &self,
        now_ms: i64,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let ended: Vec<(String, String)> = tx
            .prepare("SELECT host_id, profile FROM profile_retirements WHERE state='retiring' AND deadline_ms<=?1")?
            .query_map([now_ms], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        tx.execute(
            "DELETE FROM profile_retirements WHERE state='retiring' AND deadline_ms<=?1",
            [now_ms],
        )?;
        tx.commit()?;
        Ok(ended)
    }
}

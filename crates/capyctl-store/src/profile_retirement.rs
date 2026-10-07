//! ADR 0018 §4: removing a published runtime profile from a host, in two
//! phases, so no placement slips in between the check and the removal. The
//! retirement row is written in the transaction that names the instances
//! still using the profile; placement excludes (host, profile) from then on
//! (`profile_placeable`). Nothing here stops, releases or settles anything:
//! stops go through the ordinary path, and a retirement is confirmed only on
//! their evidence (spec design rule 4).
use crate::host_drain::DrainCandidate;
use crate::{Store, StoreError};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

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

/// The profile name revision `revision` of deployment `deployment` (SQL
/// expressions) was resolved with on `host` (`?1`).
fn profile_of(deployment: &str, revision: &str) -> String {
    format!(
        "COALESCE(
    (SELECT json_extract(h.source_json,'$.runtime_profile') FROM host_effective_revisions h
      WHERE h.deployment_id={deployment} AND h.revision={revision} AND h.host_id=?1 AND h.source_json IS NOT NULL),
    (SELECT json_extract(s.config_json,'$.runtime_profile') FROM managed_configuration_sources s
      WHERE s.deployment_id={deployment} AND s.revision={revision}))"
    )
}

/// ADR 0028 §5, §11: a group's binding names only its head's ingress, so a
/// group also uses the profile on every host where one of its members is not
/// yet settled on that host's own evidence (an uncertain member included).
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
            AND {}=?2
         UNION
         SELECT d.id, d.name, COALESCE(i.revision,d.revision), m.instance_index FROM group_members m
           JOIN deployments d ON d.id=m.deployment_id
           JOIN deployment_instances i ON i.deployment_id=m.deployment_id AND i.instance_index=m.instance_index
          WHERE m.host_id=?1 AND m.state!='settled' AND d.kind='model'
            AND {}=?2
          ORDER BY 1, 4 LIMIT ?3",
        profile_of("b.deployment_id", "b.revision"),
        profile_of("m.deployment_id", "COALESCE(i.revision,d.revision)"),
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
/// publication exists and no longer carries the profile. Standalone's
/// embedded host has no approved publication; its embedded publication
/// (review decision I3) plays that part. A host with neither is judged by
/// retirements alone.
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
    if let Some(json) = published {
        return Ok(serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .is_some_and(|doc| doc["runtime_profiles"].get(profile).is_some()));
    }
    let embedded: Option<String> = tx
        .query_row(
            "SELECT profiles_json FROM embedded_host_publications WHERE host_id=?1",
            [host],
            |r| r.get(0),
        )
        .optional()?;
    Ok(embedded.is_none_or(|json| {
        serde_json::from_str::<Vec<String>>(&json)
            .ok()
            .is_some_and(|listed| listed.iter().any(|p| p == profile))
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
    /// that use the profile on the host, in one transaction. See
    /// [`Store::begin_profile_retirement_keyed`]; this drops the key.
    pub fn begin_profile_retirement(
        &self,
        host: &str,
        profile: &str,
        key: &str,
        now_ms: i64,
        deadline_ms: i64,
        drain: bool,
    ) -> Result<RetirementStart, StoreError> {
        self.begin_profile_retirement_keyed(host, profile, key, now_ms, deadline_ms, drain)
            .map(|(_, start)| start)
    }

    /// ADR 0018 §4, phase one, returning the key the retirement stands under.
    /// review decision I1: a retirement keeps the key it was first written
    /// under until it is cleared, so a retried request (under any key, from a
    /// CLI that lost its answer, a host that reconnected, or a server that
    /// restarted) resumes it rather than conflicting. A standing retirement
    /// that still has instances to stop is resumed as a draining one: it is
    /// never cancelled by a retry that did not ask to drain. Its recorded
    /// time and deadline never change, so its stops replay the same receipts.
    pub fn begin_profile_retirement_keyed(
        &self,
        host: &str,
        profile: &str,
        key: &str,
        now_ms: i64,
        deadline_ms: i64,
        drain: bool,
    ) -> Result<(String, RetirementStart), StoreError> {
        if !valid(host, profile, key) || now_ms < 0 || deadline_ms < now_ms {
            return Err(StoreError::Conflict);
        }
        let tx = self.conn.unchecked_transaction()?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT retire_key, state FROM profile_retirements WHERE host_id=?1 AND profile=?2",
                params![host, profile],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let named = candidates(&tx, host, profile)?;
        let (key, standing) = match existing {
            // Confirmed, yet something holds a runtime again (placement has
            // excluded the profile since confirmation, so only a binding the
            // confirmation could not see): the old confirmation proves
            // nothing now. Start over under the new key, with fresh stops.
            Some((_, state)) if state == "confirmed" && !named.is_empty() => {
                tx.execute(
                    "DELETE FROM profile_retirements WHERE host_id=?1 AND profile=?2",
                    params![host, profile],
                )?;
                (key.to_owned(), false)
            }
            Some((standing, _)) => (standing, true),
            None => (key.to_owned(), false),
        };
        if !standing {
            tx.execute(
                "INSERT INTO profile_retirements(host_id,profile,retire_key,state,recorded_at_ms,deadline_ms)
                 VALUES(?1,?2,?3,'retiring',?4,?5)",
                params![host, profile, key, now_ms, deadline_ms],
            )?;
        }
        let start = if named.is_empty() {
            tx.execute(
                "UPDATE profile_retirements SET state='confirmed' WHERE host_id=?1 AND profile=?2",
                params![host, profile],
            )?;
            RetirementStart::Clear
        } else if drain || standing {
            RetirementStart::Draining(named)
        } else {
            tx.execute(
                "DELETE FROM profile_retirements WHERE host_id=?1 AND profile=?2",
                params![host, profile],
            )?;
            RetirementStart::InUse(named)
        };
        tx.commit()?;
        Ok((key, start))
    }

    /// ADR 0018 §4, §5 (review decisions I2, I3): record the profiles
    /// standalone's embedded host publishes, the same rules as a server's
    /// publications. At `startup` the list is taken as it is (a server takes a
    /// host's startup publication the same way). Live, a profile the previous
    /// list carried leaves it only with a confirmed retirement; otherwise the
    /// whole update is refused and the previous list stays. Either way the
    /// confirmed retirement of every profile the new list does not carry is
    /// cleared, so the name can be registered again.
    pub fn publish_embedded_profiles(
        &self,
        host: &str,
        profiles: &[String],
        now_ms: i64,
        startup: bool,
    ) -> Result<(), crate::host_publication::RepublishRefusal> {
        use crate::host_publication::RepublishRefusal;
        if host.is_empty() || host.len() > 128 || now_ms < 0 {
            return Err(RepublishRefusal::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if !startup {
            let previous: Option<String> = tx
                .query_row(
                    "SELECT profiles_json FROM embedded_host_publications WHERE host_id=?1",
                    [host],
                    |r| r.get(0),
                )
                .optional()?;
            let previous: Vec<String> = match previous {
                Some(json) => serde_json::from_str(&json).map_err(|_| RepublishRefusal::Store)?,
                None => Vec::new(),
            };
            for dropped in previous.iter().filter(|p| !profiles.contains(p)) {
                let confirmed: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM profile_retirements WHERE host_id=?1 AND profile=?2 AND state='confirmed')",
                    params![host, dropped],
                    |r| r.get(0),
                )?;
                if !confirmed {
                    return Err(RepublishRefusal::NotRetired(dropped.clone()));
                }
            }
        }
        let listed = serde_json::to_string(profiles).map_err(|_| RepublishRefusal::Invalid)?;
        tx.execute(
            "INSERT INTO embedded_host_publications(host_id,profiles_json,recorded_at_ms) VALUES(?1,?2,?3)
             ON CONFLICT(host_id) DO UPDATE SET profiles_json=excluded.profiles_json,recorded_at_ms=excluded.recorded_at_ms",
            params![host, listed, now_ms],
        )?;
        let document = serde_json::json!({
            "runtime_profiles": profiles
                .iter()
                .map(|p| (p.clone(), serde_json::Value::Bool(true)))
                .collect::<serde_json::Map<_, _>>()
        });
        crate::host_publication::clear_unlisted_confirmed(&tx, host, &document)?;
        tx.commit()?;
        Ok(())
    }

    /// The profiles the embedded host published last, if it ever did.
    pub fn embedded_profiles(&self, host: &str) -> Result<Option<Vec<String>>, StoreError> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT profiles_json FROM embedded_host_publications WHERE host_id=?1",
                [host],
                |r| r.get(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).map_err(|_| StoreError::Conflict))
            .transpose()
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

    /// When the retirement under `key` was first written, and its deadline.
    /// Both are fixed for its life, so a retried retirement derives the same
    /// stop requests from them.
    pub fn profile_retirement_span(
        &self,
        host: &str,
        profile: &str,
        key: &str,
    ) -> Result<Option<(i64, i64)>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT recorded_at_ms, deadline_ms FROM profile_retirements WHERE host_id=?1 AND profile=?2 AND retire_key=?3",
                params![host, profile, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
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

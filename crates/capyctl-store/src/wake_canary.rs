//! Owner decision 2026-10-09: the wake canary of a single launch, and the
//! stop a wake that did not complete owes.
//!
//! At a launch's first readiness the controller asks the engine one fixed
//! greedy completion (the group canary's prompt and length, ADR 0028 §12) and
//! records what it generated here, keyed by the instance's generation and the
//! launch's incarnation, so a controller restart between readiness and a wake
//! keeps it and a new launch never reads an earlier launch's answer. After
//! every wake the same probe is compared with it; a wake whose answer differs
//! is `wake_mismatch`, recorded here with its operation. Found live
//! 2026-10-15: a wake whose weight reload failed (`wake_reload_failed`) or
//! that ended without its evidence (`wake_uncertain`) is never re-armed
//! either, so every single launch's wake that ended uncertain owes the
//! instance's stop, retried until accepted, also across a restart.
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::{Store, StoreError};

/// The closed code a single launch's wake fails with when its canary differs
/// from the reference recorded at its first readiness.
pub const WAKE_MISMATCH: &str = "wake_mismatch";
/// The closed code a single launch's wake fails with when the engine
/// answered that its weight reload failed.
pub const WAKE_RELOAD_FAILED: &str = "wake_reload_failed";
/// The closed code a single launch's wake fails with when it ended without
/// its evidence: its deadline passed, a step's outcome is unknown, or its
/// canary was not answered.
pub const WAKE_UNCERTAIN: &str = "wake_uncertain";

/// One launch's canary reference: the fixed prompt, the token ids the engine
/// generated (empty when it answered none) and the text it generated (empty
/// when it answered none). At least one of the two is non-empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredWakeCanary {
    pub prompt: String,
    pub tokens: Vec<u32>,
    pub text: String,
}

/// ADR 0030 §4: the comparison rule of a decision reference: the same
/// choice, and every probability within this absolute distance of the
/// reference's.
pub const DECISION_TOLERANCE: f64 = 0.05;

/// ADR 0030 §4: a decision launch's canary reference: the fixed decision
/// probe's version, the choice it answered, one probability per option in
/// the order answered, and the comparison rule's tolerance.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredDecisionCanary {
    pub probe_version: u32,
    pub choice: String,
    pub probabilities: Vec<(String, f64)>,
    pub tolerance: f64,
}

/// ADR 0036 §5: the kind of the fixed embedding readiness probe.
pub const EMBEDDING_PROBE_KIND: &str = "embedding";

/// ADR 0036 §§5, 6: an embeddings launch's canary reference: the probe's
/// kind and version, the vector's dimension, the vector itself (stored as
/// little-endian float32 bytes) and the comparison rule's tolerance, a
/// relative distance `‖v − r‖ / ‖r‖`.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEmbeddingCanary {
    pub probe_kind: String,
    pub probe_version: u32,
    pub dimension: u32,
    pub vector: Vec<f32>,
    pub tolerance: f64,
}

/// ADR 0036 §6: the outcome of comparing an embedding probe's answer with
/// a launch's reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddingCanaryOutcome {
    /// The same dimension, and the relative distance is within tolerance.
    Matches,
    /// A changed dimension, a non-finite value, or a distance beyond the
    /// tolerance: `wake_mismatch`.
    Differs,
    /// The reference was recorded by another probe kind or version: it is
    /// never compared (ADR 0036 §5).
    NotComparable,
}

/// ADR 0036 §6: compare the answer `observed` of the probe `probe_kind`
/// version `probe_version` with `reference`. A pure function so the
/// controller can call it after every wake and restart.
pub fn compare_embedding_canary(
    reference: &StoredEmbeddingCanary,
    probe_kind: &str,
    probe_version: u32,
    observed: &[f32],
) -> EmbeddingCanaryOutcome {
    if reference.probe_kind != probe_kind || reference.probe_version != probe_version {
        return EmbeddingCanaryOutcome::NotComparable;
    }
    if observed.len() != reference.vector.len() || observed.len() != reference.dimension as usize {
        return EmbeddingCanaryOutcome::Differs;
    }
    let mut difference = 0.0f64;
    let mut norm = 0.0f64;
    for (v, r) in observed.iter().zip(&reference.vector) {
        let (v, r) = (f64::from(*v), f64::from(*r));
        difference += (v - r) * (v - r);
        norm += r * r;
    }
    let distance = difference.sqrt() / norm.sqrt();
    // A NaN (a non-finite answer, or a zero reference) never passes.
    if distance.is_finite() && distance <= reference.tolerance {
        EmbeddingCanaryOutcome::Matches
    } else {
        EmbeddingCanaryOutcome::Differs
    }
}

/// ADR 0030 §4, ADR 0036 §6: one launch's canary reference, by the probe
/// that proved its readiness: a greedy completion, a decision or an
/// embedding.
#[derive(Clone, Debug, PartialEq)]
pub enum WakeCanaryReference {
    Completion(StoredWakeCanary),
    Decision(StoredDecisionCanary),
    Embedding(StoredEmbeddingCanary),
}

/// ADR 0036 §6: the vector as little-endian float32 bytes.
fn vector_bytes(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn vector_from_bytes(bytes: &[u8]) -> Vec<f32> {
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionJson {
    choice: String,
    probabilities: Vec<(String, f64)>,
}

/// The stored columns of a reference: kind, probe version, prompt,
/// tokens_json, text, decision_json, tolerance, dimension and vector. A
/// malformed reference records nothing (`Conflict`).
type Columns = (
    &'static str,
    u32,
    String,
    String,
    String,
    Option<String>,
    Option<f64>,
    Option<u32>,
    Option<Vec<u8>>,
);

fn columns(reference: &WakeCanaryReference) -> Result<Columns, StoreError> {
    match reference {
        WakeCanaryReference::Completion(c) => {
            if c.tokens.is_empty() && c.text.is_empty() {
                return Err(StoreError::Conflict);
            }
            let tokens = serde_json::to_string(&c.tokens).map_err(|_| StoreError::Conflict)?;
            Ok((
                "completion",
                1,
                c.prompt.clone(),
                tokens,
                c.text.clone(),
                None,
                None,
                None,
                None,
            ))
        }
        WakeCanaryReference::Decision(d) => {
            // ADR 0030 §4: a reference the comparison rule could not use
            // records nothing.
            let probability_ok = |p: f64| p.is_finite() && (0.0..=1.0).contains(&p);
            if d.probe_version < 1
                || d.probabilities.is_empty()
                || !d.probabilities.iter().any(|(name, _)| *name == d.choice)
                || !d.probabilities.iter().all(|(_, p)| probability_ok(*p))
                || !d.tolerance.is_finite()
                || d.tolerance < 0.0
            {
                return Err(StoreError::Conflict);
            }
            let json = serde_json::to_string(&DecisionJson {
                choice: d.choice.clone(),
                probabilities: d.probabilities.clone(),
            })
            .map_err(|_| StoreError::Conflict)?;
            Ok((
                "decision",
                d.probe_version,
                format!("decision/v{}", d.probe_version),
                "[]".into(),
                String::new(),
                Some(json),
                Some(d.tolerance),
                None,
                None,
            ))
        }
        WakeCanaryReference::Embedding(e) => {
            // ADR 0036 §§5, 6: only a ready answer (finite values, norm
            // above 1e-6) of the embedding probe is a reference the
            // comparison rule could use.
            let norm = e
                .vector
                .iter()
                .map(|x| f64::from(*x) * f64::from(*x))
                .sum::<f64>()
                .sqrt();
            if e.probe_kind != EMBEDDING_PROBE_KIND
                || e.probe_version < 1
                || e.dimension < 1
                || e.vector.len() != e.dimension as usize
                || !e.vector.iter().all(|x| x.is_finite())
                || !(norm.is_finite() && norm > 1e-6)
                || !e.tolerance.is_finite()
                || e.tolerance < 0.0
            {
                return Err(StoreError::Conflict);
            }
            Ok((
                "embedding",
                e.probe_version,
                format!("embedding/v{}", e.probe_version),
                "[]".into(),
                String::new(),
                None,
                Some(e.tolerance),
                Some(e.dimension),
                Some(vector_bytes(&e.vector)),
            ))
        }
    }
}

/// A single-launch instance whose wake ended uncertain and whose stop is
/// still owed: the wake is at its current generation and it still reads
/// desired ready (an accepted stop draws a new generation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WakeStop {
    pub deployment_id: String,
    pub instance_index: u32,
    pub generation: i64,
    /// The wake that ended uncertain.
    pub operation_id: String,
}

impl Store {
    /// Record the canary reference of the launch `incarnation` at
    /// `generation`, once per launch. Returns whether this call recorded it:
    /// an earlier reference of the same launch is kept (`false`). A reference
    /// of another launch at the same generation is replaced, and every
    /// reference of an earlier generation of the instance is dropped. A
    /// generation that is not the instance's current one records nothing
    /// (`Conflict`), nor does an empty completion answer or a malformed
    /// decision reference (ADR 0030 §4).
    pub fn record_wake_canary(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        incarnation: &str,
        reference: &WakeCanaryReference,
        now: i64,
    ) -> Result<bool, StoreError> {
        let (kind, probe_version, prompt, tokens, text, decision, tolerance, dimension, vector) =
            columns(reference)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT generation FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2",
                params![deployment_id, instance_index],
                |r| r.get(0),
            )
            .optional()?;
        if current != Some(generation) {
            return Err(StoreError::Conflict);
        }
        tx.execute(
            "DELETE FROM wake_canaries WHERE deployment_id=?1 AND instance_index=?2 AND generation<?3",
            params![deployment_id, instance_index, generation],
        )?;
        let recorded = tx.execute(
            "INSERT INTO wake_canaries(deployment_id,instance_index,generation,incarnation,prompt,
                tokens_json,text,recorded_at_ms,kind,probe_version,decision_json,tolerance,
                dimension,vector)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(deployment_id,instance_index,generation) DO UPDATE SET
                incarnation=excluded.incarnation,prompt=excluded.prompt,
                tokens_json=excluded.tokens_json,text=excluded.text,
                kind=excluded.kind,probe_version=excluded.probe_version,
                decision_json=excluded.decision_json,tolerance=excluded.tolerance,
                dimension=excluded.dimension,vector=excluded.vector,
                recorded_at_ms=excluded.recorded_at_ms,mismatch_operation_id=NULL
             WHERE wake_canaries.incarnation<>excluded.incarnation",
            params![
                deployment_id,
                instance_index,
                generation,
                incarnation,
                prompt,
                tokens,
                text,
                now.max(0),
                kind,
                probe_version,
                decision,
                tolerance,
                dimension,
                vector
            ],
        )? == 1;
        tx.commit()?;
        Ok(recorded)
    }

    /// The canary reference recorded for the launch `incarnation` at
    /// `generation`; `None` when that launch recorded none.
    pub fn wake_canary(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        incarnation: &str,
    ) -> Result<Option<WakeCanaryReference>, StoreError> {
        type Row = (
            String,
            u32,
            String,
            String,
            String,
            Option<String>,
            Option<f64>,
            Option<u32>,
            Option<Vec<u8>>,
        );
        let row: Option<Row> = self
            .conn
            .query_row(
                "SELECT kind,probe_version,prompt,tokens_json,text,decision_json,tolerance,dimension,vector
                   FROM wake_canaries
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND incarnation=?4",
                params![deployment_id, instance_index, generation, incarnation],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            kind,
            probe_version,
            prompt,
            tokens,
            text,
            decision,
            tolerance,
            dimension,
            vector,
        )) = row
        else {
            return Ok(None);
        };
        let reference = match (kind.as_str(), decision, tolerance) {
            ("completion", _, _) => WakeCanaryReference::Completion(StoredWakeCanary {
                prompt,
                tokens: serde_json::from_str(&tokens).map_err(|_| StoreError::Conflict)?,
                text,
            }),
            ("decision", Some(decision), Some(tolerance)) => {
                let d: DecisionJson =
                    serde_json::from_str(&decision).map_err(|_| StoreError::Conflict)?;
                WakeCanaryReference::Decision(StoredDecisionCanary {
                    probe_version,
                    choice: d.choice,
                    probabilities: d.probabilities,
                    tolerance,
                })
            }
            ("embedding", None, Some(tolerance)) => {
                let (Some(dimension), Some(bytes)) = (dimension, vector) else {
                    return Err(StoreError::Conflict);
                };
                if bytes.len() != dimension as usize * 4 {
                    return Err(StoreError::Conflict);
                }
                WakeCanaryReference::Embedding(StoredEmbeddingCanary {
                    probe_kind: EMBEDDING_PROBE_KIND.into(),
                    probe_version,
                    dimension,
                    vector: vector_from_bytes(&bytes),
                    tolerance,
                })
            }
            _ => return Err(StoreError::Conflict),
        };
        Ok(Some(reference))
    }

    /// The wake `operation_id` of the launch at `generation` answered its
    /// canary differently: record it (the first one is kept) and name
    /// [`WAKE_MISMATCH`] in the instance's status. Nothing else changes; the
    /// caller marks the wake uncertain and stops the instance.
    pub fn record_wake_mismatch(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        operation_id: &str,
    ) -> Result<(), StoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let marked = tx.execute(
            "UPDATE wake_canaries SET mismatch_operation_id=COALESCE(mismatch_operation_id,?4)
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
            params![deployment_id, instance_index, generation, operation_id],
        )?;
        if marked != 1 {
            return Err(StoreError::Conflict);
        }
        tx.execute(
            "UPDATE deployment_instances SET last_error=?4
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
            params![deployment_id, instance_index, generation, WAKE_MISMATCH],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The wake of the launch at `generation` did not complete for `code`
    /// ([`WAKE_RELOAD_FAILED`] or [`WAKE_UNCERTAIN`]; a canary that differs
    /// is [`Self::record_wake_mismatch`]): name it in the instance's status.
    /// Nothing else changes; the caller marks the wake uncertain and stops
    /// the instance. A generation that is not the instance's current one
    /// records nothing (`Conflict`).
    pub fn record_wake_failure(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        code: &str,
    ) -> Result<(), StoreError> {
        if ![WAKE_RELOAD_FAILED, WAKE_UNCERTAIN].contains(&code) {
            return Err(StoreError::Conflict);
        }
        let marked = self.conn.execute(
            "UPDATE deployment_instances SET last_error=?4
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
            params![deployment_id, instance_index, generation, code],
        )?;
        if marked != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    /// Every single-launch instance (no engine group at its generation)
    /// whose wake ended uncertain at its current generation and that still
    /// reads desired ready: its stop is still owed. A wake that ended
    /// uncertain is never re-armed, so nothing else would settle it. The
    /// coordinator retries each one's stop until it is accepted. A group's
    /// wake owes its group stop instead (`group_residency_stops_due`).
    pub fn wake_stops_due(&self) -> Result<Vec<WakeStop>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT i.deployment_id,i.instance_index,i.generation,r.operation_id
               FROM deployment_instances i
               JOIN lifecycle_runs r ON r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.generation=i.generation
               JOIN operations o ON o.id=r.operation_id
              WHERE r.state='uncertain' AND o.kind='restore' AND i.desired_state='ready'
                AND NOT EXISTS(SELECT 1 FROM group_plans g
                     WHERE g.deployment_id=i.deployment_id AND g.instance_index=i.instance_index AND g.generation=i.generation)
              ORDER BY i.deployment_id,i.instance_index,r.operation_id",
        )?;
        let due = statement
            .query_map([], |r| {
                Ok(WakeStop {
                    deployment_id: r.get(0)?,
                    instance_index: r.get(1)?,
                    generation: r.get(2)?,
                    operation_id: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(due)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_instance(generation: i64) -> Store {
        let store = Store::open_in_memory().unwrap();
        set_instance(&store, generation);
        store
    }

    fn set_instance(store: &Store, generation: i64) {
        store
            .conn
            .execute_batch("PRAGMA foreign_keys=OFF;")
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO deployment_instances(deployment_id,instance_index,generation,desired_state,observed_state)
                 VALUES('d',0,?1,'ready','ready')
                 ON CONFLICT(deployment_id,instance_index) DO UPDATE SET generation=excluded.generation",
                [generation],
            )
            .unwrap();
    }

    fn reference(tokens: &[u32], text: &str) -> WakeCanaryReference {
        WakeCanaryReference::Completion(StoredWakeCanary {
            prompt: "Say ready.".into(),
            tokens: tokens.to_vec(),
            text: text.into(),
        })
    }

    fn decision(choice: &str, probabilities: &[(&str, f64)]) -> WakeCanaryReference {
        WakeCanaryReference::Decision(StoredDecisionCanary {
            probe_version: 1,
            choice: choice.into(),
            probabilities: probabilities
                .iter()
                .map(|(name, p)| (name.to_string(), *p))
                .collect(),
            tolerance: DECISION_TOLERANCE,
        })
    }

    // T43, T20 (ADR 0030 §4): a decision reference round-trips with its
    // choice, probabilities in order, version and tolerance, scoped to its
    // launch and generation, and is recorded once per launch.
    #[test]
    fn a_decision_reference_round_trips_once_per_launch() {
        let store = store_with_instance(1);
        let first = decision("yes", &[("yes", 0.75), ("no", 0.25)]);
        assert!(store
            .record_wake_canary("d", 0, 1, "inc-1", &first, 10)
            .unwrap());
        assert!(!store
            .record_wake_canary("d", 0, 1, "inc-1", &decision("no", &[("no", 1.0)]), 11)
            .unwrap());
        assert_eq!(
            store.wake_canary("d", 0, 1, "inc-1").unwrap(),
            Some(first.clone())
        );
        assert_eq!(store.wake_canary("d", 0, 1, "inc-2").unwrap(), None);
        assert_eq!(store.wake_canary("d", 0, 0, "inc-1").unwrap(), None);
        // Another launch replaces it, also with another kind of reference.
        assert!(store
            .record_wake_canary("d", 0, 1, "inc-1b", &reference(&[1], ""), 12)
            .unwrap());
        assert_eq!(store.wake_canary("d", 0, 1, "inc-1").unwrap(), None);
        assert!(store
            .record_wake_canary("d", 0, 1, "inc-1c", &first, 13)
            .unwrap());
        assert_eq!(store.wake_canary("d", 0, 1, "inc-1c").unwrap(), Some(first));
    }

    // T43 (ADR 0030 §4): a decision reference the comparison rule could not
    // use records nothing.
    #[test]
    fn a_malformed_decision_reference_is_refused() {
        let store = store_with_instance(1);
        let with = |f: fn(&mut StoredDecisionCanary)| {
            let WakeCanaryReference::Decision(mut d) =
                decision("yes", &[("yes", 0.6), ("no", 0.4)])
            else {
                unreachable!()
            };
            f(&mut d);
            WakeCanaryReference::Decision(d)
        };
        for bad in [
            with(|d| d.probabilities.clear()),
            with(|d| d.choice = "maybe".into()),
            with(|d| d.probabilities[1].1 = f64::NAN),
            with(|d| d.probabilities[1].1 = 1.5),
            with(|d| d.probabilities[1].1 = -0.1),
            with(|d| d.tolerance = f64::INFINITY),
            with(|d| d.tolerance = -0.01),
        ] {
            assert!(matches!(
                store.record_wake_canary("d", 0, 1, "inc-1", &bad, 10),
                Err(StoreError::Conflict)
            ));
        }
        assert_eq!(store.wake_canary("d", 0, 1, "inc-1").unwrap(), None);
    }

    // T20 (owner decision 2026-10-09): one reference per launch; the first
    // recording is kept, and it is read back only for the same launch.
    #[test]
    fn a_launch_keeps_its_first_reference() {
        let store = store_with_instance(1);
        assert!(store
            .record_wake_canary("d", 0, 1, "inc-1", &reference(&[1, 2], "ok"), 10)
            .unwrap());
        assert!(!store
            .record_wake_canary("d", 0, 1, "inc-1", &reference(&[9], "no"), 11)
            .unwrap());
        assert_eq!(
            store.wake_canary("d", 0, 1, "inc-1").unwrap(),
            Some(reference(&[1, 2], "ok"))
        );
        assert_eq!(store.wake_canary("d", 0, 1, "inc-2").unwrap(), None);
        // An answer with neither tokens nor text, or a stale generation,
        // records nothing.
        assert!(store
            .record_wake_canary("d", 0, 1, "inc-1", &reference(&[], ""), 12)
            .is_err());
        assert!(store
            .record_wake_canary("d", 0, 0, "inc-0", &reference(&[1], ""), 12)
            .is_err());
    }

    // T20 (owner decision 2026-10-09): a new launch records a new reference;
    // the earlier launch's is gone.
    #[test]
    fn a_new_launch_records_a_new_reference() {
        let store = store_with_instance(1);
        store
            .record_wake_canary("d", 0, 1, "inc-1", &reference(&[1, 2], ""), 10)
            .unwrap();
        // Another launch at the same generation replaces it.
        assert!(store
            .record_wake_canary("d", 0, 1, "inc-1b", &reference(&[3], ""), 11)
            .unwrap());
        assert_eq!(store.wake_canary("d", 0, 1, "inc-1").unwrap(), None);
        // A later generation drops every earlier one.
        set_instance(&store, 2);
        assert!(store
            .record_wake_canary("d", 0, 2, "inc-2", &reference(&[], "text only"), 12)
            .unwrap());
        assert_eq!(store.wake_canary("d", 0, 1, "inc-1b").unwrap(), None);
        assert_eq!(
            store.wake_canary("d", 0, 2, "inc-2").unwrap(),
            Some(reference(&[], "text only"))
        );
        let rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM wake_canaries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    // T20 (owner decision 2026-10-09): a controller restart between first
    // readiness and a wake keeps the reference.
    #[test]
    fn a_reopened_store_keeps_the_reference() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("srv.sqlite3");
        {
            let store = Store::open(&path).unwrap();
            set_instance(&store, 4);
            store
                .record_wake_canary("d", 0, 4, "inc-4", &reference(&[5, 6, 7], "ok"), 10)
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.wake_canary("d", 0, 4, "inc-4").unwrap(),
            Some(reference(&[5, 6, 7], "ok"))
        );
    }

    // T20 (owner decision 2026-10-09): a mismatch names `wake_mismatch` in
    // the status; a mismatch of a launch that recorded no reference is
    // refused.
    #[test]
    fn a_mismatch_names_its_code() {
        let store = store_with_instance(3);
        store
            .record_wake_canary("d", 0, 3, "inc-3", &reference(&[1], ""), 10)
            .unwrap();
        store.record_wake_mismatch("d", 0, 3, "op-wake").unwrap();
        assert_eq!(last_error(&store).as_deref(), Some(WAKE_MISMATCH));
        set_instance(&store, 4);
        assert!(store.record_wake_mismatch("d", 0, 4, "op-2").is_err());
    }

    fn last_error(store: &Store) -> Option<String> {
        store
            .conn
            .query_row(
                "SELECT last_error FROM deployment_instances WHERE deployment_id='d'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// One operation `id` of `kind` on instance 0 at `generation`, its run
    /// in `state`.
    fn run(store: &Store, id: &str, kind: &str, generation: i64, state: &str) {
        let action = if kind == "restore" {
            "activate"
        } else {
            "park"
        };
        store
            .conn
            .execute(
                "INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,'d',?2,'running')",
                params![id, kind],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json,instance_index)
                 VALUES(?1,'d',1,?2,'s',?3,?4,0,'{}',0)",
                params![id, generation, action, state],
            )
            .unwrap();
    }

    // T20 (found live 2026-10-15): every single launch's wake that ended
    // uncertain owes the instance's stop, whatever its code; a park, a
    // running wake, an earlier generation and a group do not.
    #[test]
    fn an_uncertain_wake_owes_the_stop_until_a_new_generation() {
        let store = store_with_instance(3);
        run(&store, "op-park", "park", 3, "uncertain");
        run(&store, "op-old", "restore", 2, "uncertain");
        run(&store, "op-wake", "restore", 3, "running");
        assert!(store.wake_stops_due().unwrap().is_empty());
        store
            .conn
            .execute(
                "UPDATE lifecycle_runs SET state='uncertain' WHERE operation_id='op-wake'",
                [],
            )
            .unwrap();
        let owed = vec![WakeStop {
            deployment_id: "d".into(),
            instance_index: 0,
            generation: 3,
            operation_id: "op-wake".into(),
        }];
        assert_eq!(store.wake_stops_due().unwrap(), owed);
        // The code names it in the status; only the closed wake codes, and
        // only at the current generation.
        store
            .record_wake_failure("d", 0, 3, WAKE_RELOAD_FAILED)
            .unwrap();
        assert_eq!(last_error(&store).as_deref(), Some(WAKE_RELOAD_FAILED));
        store
            .record_wake_failure("d", 0, 3, WAKE_UNCERTAIN)
            .unwrap();
        assert_eq!(last_error(&store).as_deref(), Some(WAKE_UNCERTAIN));
        assert!(store.record_wake_failure("d", 0, 3, "other").is_err());
        assert!(store
            .record_wake_failure("d", 0, 2, WAKE_UNCERTAIN)
            .is_err());
        // A group's wake owes its group stop instead.
        store
            .conn
            .execute(
                "INSERT INTO group_plans(deployment_id,instance_index,generation,plan_json,rendezvous_host,rendezvous_port,state)
                 VALUES('d',0,3,'{}','h',1,'active')",
                [],
            )
            .unwrap();
        assert!(store.wake_stops_due().unwrap().is_empty());
        store.conn.execute("DELETE FROM group_plans", []).unwrap();
        assert_eq!(store.wake_stops_due().unwrap(), owed);
        // A stop draws a new generation: nothing is owed any more.
        set_instance(&store, 4);
        assert!(store.wake_stops_due().unwrap().is_empty());
    }

    fn embedding(vector: &[f32]) -> StoredEmbeddingCanary {
        StoredEmbeddingCanary {
            probe_kind: EMBEDDING_PROBE_KIND.into(),
            probe_version: 1,
            dimension: vector.len() as u32,
            vector: vector.to_vec(),
            tolerance: 0.02,
        }
    }

    // T47, T20 (ADR 0036 §6): an embedding reference round-trips with its
    // kind, version, dimension, exact vector and tolerance, stored as
    // little-endian float32 bytes, once per launch.
    #[test]
    fn an_embedding_reference_round_trips_as_float32_bytes() {
        let store = store_with_instance(1);
        let first = WakeCanaryReference::Embedding(embedding(&[0.1, -0.25, 3.5e-7, 1.0]));
        assert!(store
            .record_wake_canary("d", 0, 1, "inc-1", &first, 10)
            .unwrap());
        assert!(!store
            .record_wake_canary(
                "d",
                0,
                1,
                "inc-1",
                &WakeCanaryReference::Embedding(embedding(&[1.0])),
                11
            )
            .unwrap());
        assert_eq!(store.wake_canary("d", 0, 1, "inc-1").unwrap(), Some(first));
        let (dimension, bytes): (u32, Vec<u8>) = store
            .conn
            .query_row("SELECT dimension,vector FROM wake_canaries", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(dimension, 4);
        assert_eq!(&bytes[12..16], &1.0f32.to_le_bytes());
    }

    // T47 (ADR 0036 §§5, 6): a reference the comparison could not use
    // records nothing.
    #[test]
    fn a_malformed_embedding_reference_records_nothing() {
        let store = store_with_instance(1);
        let bad = |f: fn(&mut StoredEmbeddingCanary)| {
            let mut e = embedding(&[0.6, 0.8]);
            f(&mut e);
            WakeCanaryReference::Embedding(e)
        };
        for reference in [
            bad(|e| e.probe_kind = "decision".into()),
            bad(|e| e.probe_version = 0),
            bad(|e| e.dimension = 3),
            bad(|e| e.vector = vec![]),
            bad(|e| e.vector = vec![f32::NAN, 1.0]),
            bad(|e| e.vector = vec![0.0, 0.0]),
            bad(|e| e.tolerance = -0.1),
            bad(|e| e.tolerance = f64::INFINITY),
        ] {
            assert!(matches!(
                store.record_wake_canary("d", 0, 1, "inc", &reference, 1),
                Err(StoreError::Conflict)
            ));
        }
        assert_eq!(store.wake_canary("d", 0, 1, "inc").unwrap(), None);
    }

    // T47, T20 (ADR 0036 §6): a relative distance within the tolerance
    // passes, beyond it fails, a changed dimension or a non-finite answer
    // fails, and a reference of another kind or version is not compared.
    #[test]
    fn the_embedding_comparison_uses_relative_distance() {
        let reference = embedding(&[0.6, 0.8]);
        let compare = |v: &[f32]| compare_embedding_canary(&reference, "embedding", 1, v);
        assert_eq!(compare(&[0.6, 0.8]), EmbeddingCanaryOutcome::Matches);
        // ‖v − r‖ / ‖r‖ = 0.019.
        assert_eq!(compare(&[0.619, 0.8]), EmbeddingCanaryOutcome::Matches);
        // 0.021: beyond 0.02.
        assert_eq!(compare(&[0.621, 0.8]), EmbeddingCanaryOutcome::Differs);
        // Relative: a reference twice as long allows twice the distance.
        let long = embedding(&[1.2, 1.6]);
        assert_eq!(
            compare_embedding_canary(&long, "embedding", 1, &[1.238, 1.6]),
            EmbeddingCanaryOutcome::Matches
        );
        assert_eq!(compare(&[0.6, 0.8, 0.0]), EmbeddingCanaryOutcome::Differs);
        assert_eq!(compare(&[0.6]), EmbeddingCanaryOutcome::Differs);
        assert_eq!(compare(&[f32::NAN, 0.8]), EmbeddingCanaryOutcome::Differs);
        assert_eq!(
            compare(&[f32::INFINITY, 0.8]),
            EmbeddingCanaryOutcome::Differs
        );
        assert_eq!(
            compare_embedding_canary(&reference, "embedding", 2, &[0.6, 0.8]),
            EmbeddingCanaryOutcome::NotComparable
        );
        assert_eq!(
            compare_embedding_canary(&reference, "decision", 1, &[0.0, 1.0]),
            EmbeddingCanaryOutcome::NotComparable
        );
    }
}

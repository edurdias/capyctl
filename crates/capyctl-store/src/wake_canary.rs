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
    /// (`Conflict`), nor does an empty answer.
    pub fn record_wake_canary(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        incarnation: &str,
        reference: &StoredWakeCanary,
        now: i64,
    ) -> Result<bool, StoreError> {
        if reference.tokens.is_empty() && reference.text.is_empty() {
            return Err(StoreError::Conflict);
        }
        let tokens = serde_json::to_string(&reference.tokens).map_err(|_| StoreError::Conflict)?;
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
                tokens_json,text,recorded_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(deployment_id,instance_index,generation) DO UPDATE SET
                incarnation=excluded.incarnation,prompt=excluded.prompt,
                tokens_json=excluded.tokens_json,text=excluded.text,
                recorded_at_ms=excluded.recorded_at_ms,mismatch_operation_id=NULL
             WHERE wake_canaries.incarnation<>excluded.incarnation",
            params![
                deployment_id,
                instance_index,
                generation,
                incarnation,
                reference.prompt,
                tokens,
                reference.text,
                now.max(0)
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
    ) -> Result<Option<StoredWakeCanary>, StoreError> {
        let row: Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT prompt,tokens_json,text FROM wake_canaries
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND incarnation=?4",
                params![deployment_id, instance_index, generation, incarnation],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(prompt, tokens, text)| {
            Ok(StoredWakeCanary {
                prompt,
                tokens: serde_json::from_str(&tokens).map_err(|_| StoreError::Conflict)?,
                text,
            })
        })
        .transpose()
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

    fn reference(tokens: &[u32], text: &str) -> StoredWakeCanary {
        StoredWakeCanary {
            prompt: "Say ready.".into(),
            tokens: tokens.to_vec(),
            text: text.into(),
        }
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
}

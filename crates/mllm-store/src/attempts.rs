//! How many times one configuration of a deployment has been attempted.
//!
//! ADR 0011 decision 5: the state machine retries a failed deployment and gives up
//! after a budget. The count is keyed to the fence, not the deployment, so a new
//! revision or generation is a fresh configuration with a fresh budget — otherwise a
//! deployment that exhausted its attempts could never be restarted.

use rusqlite::{params, OptionalExtension};

use crate::lifecycle::DeploymentFence;
use crate::StoreError;

/// Attempts recorded against one configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    pub attempts: i64,
    pub last_attempt_ms: i64,
}

impl crate::Store {
    /// Count one attempt and return the running total.
    pub fn record_attempt(
        &self,
        fence: &DeploymentFence,
        now_ms: i64,
    ) -> Result<AttemptRecord, StoreError> {
        if now_ms < 0 {
            return Err(StoreError::Conflict);
        }
        self.conn.execute(
            "INSERT INTO deployment_attempts(deployment_id,revision,generation,attempts,last_attempt_ms)
             VALUES(?1,?2,?3,1,?4)
             ON CONFLICT(deployment_id,revision,generation)
             DO UPDATE SET attempts = attempts + 1, last_attempt_ms = ?4",
            params![fence.deployment_id, fence.revision, fence.generation, now_ms],
        )?;
        self.attempts(fence)?.ok_or(StoreError::Conflict)
    }

    /// Forget what this configuration has been charged.
    ///
    /// ADR 0011 decision 5: a success is terminal and resets the attempts. The
    /// budget bounds how many times one configuration is attempted before it is
    /// given up on, so a configuration that reached Ready must not carry its
    /// earlier failures into the next time it is started. Only this configuration
    /// is cleared; another generation's history is its own.
    pub fn clear_attempts(&self, fence: &DeploymentFence) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM deployment_attempts
             WHERE deployment_id=?1 AND revision=?2 AND generation=?3",
            params![fence.deployment_id, fence.revision, fence.generation],
        )?;
        Ok(())
    }

    /// What has been recorded against this configuration, if anything.
    pub fn attempts(&self, fence: &DeploymentFence) -> Result<Option<AttemptRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT attempts,last_attempt_ms FROM deployment_attempts
                 WHERE deployment_id=?1 AND revision=?2 AND generation=?3",
                params![fence.deployment_id, fence.revision, fence.generation],
                |row| {
                    Ok(AttemptRecord {
                        attempts: row.get(0)?,
                        last_attempt_ms: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use crate::lifecycle::DeploymentFence;

    fn fence(generation: i64) -> DeploymentFence {
        DeploymentFence {
            deployment_id: "dep".into(),
            revision: 1,
            generation,
        }
    }

    /// Attempts accumulate against one configuration and carry the time of the last
    /// one, which is what the cooldown is measured from.
    #[test]
    fn attempts_accumulate_and_carry_their_time() {
        let store = crate::Store::open_in_memory().unwrap();
        store
            .conn
            .execute("INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('dep','dep','model','ready',1,0,1,1)", [])
            .unwrap();

        assert!(store.attempts(&fence(1)).unwrap().is_none(), "nothing yet");

        let first = store.record_attempt(&fence(1), 1_000).unwrap();
        assert_eq!(first.attempts, 1);
        assert_eq!(first.last_attempt_ms, 1_000);

        let second = store.record_attempt(&fence(1), 2_500).unwrap();
        assert_eq!(second.attempts, 2);
        assert_eq!(second.last_attempt_ms, 2_500);
    }

    /// ADR 0011 decision 5: a success is terminal and resets the attempts, so the
    /// next start of this configuration has its whole budget again. Another
    /// generation's count is untouched.
    #[test]
    fn a_success_clears_only_its_own_configuration() {
        let store = crate::Store::open_in_memory().unwrap();
        store
            .conn
            .execute("INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('dep','dep','model','ready',1,0,1,1)", [])
            .unwrap();
        store.record_attempt(&fence(1), 1_000).unwrap();
        store.record_attempt(&fence(1), 2_000).unwrap();
        store.record_attempt(&fence(2), 2_500).unwrap();

        store.clear_attempts(&fence(1)).unwrap();

        assert!(store.attempts(&fence(1)).unwrap().is_none());
        assert_eq!(store.attempts(&fence(2)).unwrap().unwrap().attempts, 1);
        // Clearing what is already clear is not an error: the worker clears on
        // every success, including the first.
        store.clear_attempts(&fence(1)).unwrap();
        assert_eq!(
            store.record_attempt(&fence(1), 3_000).unwrap().attempts,
            1,
            "the budget starts again"
        );
    }

    /// A new generation is a new configuration: it does not inherit the failures of
    /// the one before it, or a deployment could never be restarted after exhausting
    /// its budget.
    #[test]
    fn a_new_generation_starts_fresh() {
        let store = crate::Store::open_in_memory().unwrap();
        store
            .conn
            .execute("INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('dep','dep','model','ready',1,0,1,1)", [])
            .unwrap();
        store.record_attempt(&fence(1), 1_000).unwrap();
        store.record_attempt(&fence(1), 2_000).unwrap();

        let fresh = store.record_attempt(&fence(2), 3_000).unwrap();
        assert_eq!(fresh.attempts, 1, "generation 2 starts from zero");
        assert_eq!(
            store.attempts(&fence(1)).unwrap().unwrap().attempts,
            2,
            "generation 1's history is preserved"
        );
    }
}

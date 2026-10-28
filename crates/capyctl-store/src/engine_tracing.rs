//! ADR 0037 §7: each launch's engine tracing decision, frozen when it
//! launched (`launch_engine_tracing`, one row per runtime binding). The
//! router's lane reads whether the launch traces; status reads the decision.

use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::dispatch::CoordinatorSession;
use crate::lifecycle::LifecycleError;

/// The off reasons a row may hold (`capyctl_adapters::engine_tracing::
/// EngineTracingOff::code`; the table's CHECK holds the same list).
pub const OFF_REASONS: [&str; 5] = [
    "host_off",
    "engine_unsupported",
    "not_installed",
    "group",
    "retention_zero",
];

/// One launch's engine tracing as status shows it: `on`, or `off` with its
/// reason.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InstanceEngineTracing {
    /// `on` or `off`.
    pub state: String,
    /// Off only: the closed reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl crate::Store {
    /// ADR 0037 §7: record the engine tracing decision of retained binding
    /// `binding_id` (`state`: `on` or an off reason's code), before Ready. The
    /// first record stands: the decision was frozen when the launch started.
    pub fn record_launch_engine_tracing(
        &self,
        s: &CoordinatorSession,
        binding_id: &str,
        state: &str,
    ) -> Result<(), LifecycleError> {
        let (state, reason) = match state {
            "on" => ("on", None),
            reason if OFF_REASONS.contains(&reason) => ("off", Some(reason)),
            _ => return Err(LifecycleError::Invalid),
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        crate::lifecycle::completion::check_session(&tx, s)?;
        let retained: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND state!='released')",
            [binding_id],
            |r| r.get(0),
        )?;
        if !retained {
            return Err(LifecycleError::Conflict);
        }
        tx.execute(
            "INSERT INTO launch_engine_tracing(binding_id,state,reason) VALUES(?1,?2,?3)
             ON CONFLICT(binding_id) DO NOTHING",
            params![binding_id, state, reason],
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// ADR 0037 §7: whether binding `binding_id`'s launch runs with engine
/// tracing. A launch without a record (recorded before engine tracing) does
/// not.
pub(crate) fn binding_traces(
    tx: &Transaction<'_>,
    binding_id: &str,
) -> Result<bool, rusqlite::Error> {
    tx.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM launch_engine_tracing WHERE binding_id=?1 AND state='on')",
    )?
    .query_row([binding_id], |r| r.get(0))
}

/// ADR 0037 §13: the engine tracing of the instance's retained launch, if it
/// has one with a recorded decision.
pub(crate) fn instance_engine_tracing(
    tx: &Transaction<'_>,
    deployment: &str,
    index: u32,
) -> Result<Option<InstanceEngineTracing>, rusqlite::Error> {
    tx.prepare_cached(
        "SELECT t.state,t.reason FROM launch_engine_tracing t
           JOIN runtime_bindings b ON b.id=t.binding_id
          WHERE b.deployment_id=?1 AND b.instance_index=?2 AND b.state!='released'
          ORDER BY b.rowid DESC LIMIT 1",
    )?
    .query_row(params![deployment, index], |r| {
        Ok(InstanceEngineTracing {
            state: r.get(0)?,
            reason: r.get(1)?,
        })
    })
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    // T48 (ADR 0037 §§7, 13): the table holds `on`, or `off` with one closed
    // reason; the lane reads whether the launch traces and status reads the
    // instance's retained launch, never a released one.
    #[test]
    fn the_decision_is_kept_per_launch_and_read_for_lane_and_status() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        crate::migrations::apply(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state,instance_index) VALUES
               ('b-on','d',1,'i-on','managed','{}','[]','live',0),
               ('b-off','d',1,'i-off','managed','{}','[]','live',1),
               ('b-old','d',1,'i-old','managed','{}','[]','released',2),
               ('b-none','d',1,'i-none','managed','{}','[]','live',3);
             INSERT INTO launch_engine_tracing(binding_id,state,reason) VALUES
               ('b-on','on',NULL),('b-off','off','retention_zero'),('b-old','on',NULL);",
        )
        .unwrap();
        for (state, reason) in [
            ("on", Some("host_off")),
            ("off", None),
            ("off", Some("off")),
            ("maybe", None),
        ] {
            assert!(
                conn.execute(
                    "INSERT INTO launch_engine_tracing(binding_id,state,reason) VALUES('b-none',?1,?2)",
                    params![state, reason],
                )
                .is_err(),
                "{state} {reason:?}"
            );
        }
        let tx = conn.unchecked_transaction().unwrap();
        assert!(binding_traces(&tx, "b-on").unwrap());
        assert!(!binding_traces(&tx, "b-off").unwrap());
        assert!(!binding_traces(&tx, "b-none").unwrap());
        let status = |index| instance_engine_tracing(&tx, "d", index).unwrap();
        assert_eq!(
            status(0),
            Some(InstanceEngineTracing {
                state: "on".into(),
                reason: None
            })
        );
        assert_eq!(
            status(1),
            Some(InstanceEngineTracing {
                state: "off".into(),
                reason: Some("retention_zero".into())
            })
        );
        assert_eq!(status(2), None, "a released launch shows nothing");
        assert_eq!(status(3), None, "a launch recorded before engine tracing");
        // The JSON status carries.
        assert_eq!(
            serde_json::to_value(status(0).unwrap()).unwrap(),
            serde_json::json!({"state": "on"})
        );
        assert_eq!(
            serde_json::to_value(status(1).unwrap()).unwrap(),
            serde_json::json!({"state": "off", "reason": "retention_zero"})
        );
    }
}

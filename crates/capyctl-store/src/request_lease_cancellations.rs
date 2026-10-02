//! SPEC §10 (amended 2026-10-01): leases of requests whose client hung up.
//! The router closed the engine connection, which asks the engine to abort,
//! but a closed socket is not proof the engine stopped. The lease stays
//! `inflight` and charged until the engine's own counters read quiescent at
//! or after the hang-up; only then does it close.

use rusqlite::{params, Transaction, TransactionBehavior};

use crate::dispatch::{check_session, CoordinatorSession, DispatchError};
use crate::StoreError;

/// A retained binding with at least one cancelling lease of this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancellingBinding {
    pub binding_id: String,
    pub deployment_id: String,
    pub leases: usize,
    pub newest_cancelled_at_ms: i64,
}

// The binding-to-lease join of `binding_outstanding_leases`: a lease on the
// instance lane a retained binding serves, of this session, still in flight.
const CANCELLING: &str = "FROM request_lease_cancellations c
     JOIN request_leases l ON l.id=c.lease_id
     JOIN runtime_bindings b ON b.deployment_id=l.deployment_id
       AND b.instance_index=l.instance_index AND b.state!='released'
     WHERE l.session_id=?1 AND l.disposition='inflight'";

impl crate::Store {
    /// Bindings of this session with at least one cancelling lease.
    pub fn cancelling_bindings(
        &self,
        session: &CoordinatorSession,
    ) -> Result<Vec<CancellingBinding>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT b.id,b.deployment_id,COUNT(*),MAX(c.cancelled_at_ms) {CANCELLING}
             GROUP BY b.id,b.deployment_id ORDER BY b.id"
        ))?;
        let rows = statement.query_map([session.id()], |r| {
            let leases: i64 = r.get(2)?;
            Ok(CancellingBinding {
                binding_id: r.get(0)?,
                deployment_id: r.get(1)?,
                leases: usize::try_from(leases).unwrap_or(0),
                newest_cancelled_at_ms: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Close this session's cancelling leases on the binding cancelled at or
    /// before `asked_at_ms`; journals one event with `receipt`. Returns the count.
    pub fn settle_cancelled_leases(
        &self,
        session: &CoordinatorSession,
        binding_id: &str,
        asked_at_ms: i64,
        receipt: &str,
    ) -> Result<usize, StoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        match check_session(&tx, session) {
            Ok(()) => {}
            Err(DispatchError::Sql(error)) => return Err(error.into()),
            Err(_) => return Ok(0),
        }
        let deployment: Option<String> = tx
            .query_row(
                "SELECT deployment_id FROM runtime_bindings WHERE id=?1 AND state!='released'",
                [binding_id],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                error => Err(error),
            })?;
        let Some(deployment_id) = deployment else {
            return Ok(0);
        };
        let count = tx.execute(
            "DELETE FROM request_leases WHERE id IN (SELECT l.id FROM request_lease_cancellations c
               JOIN request_leases l ON l.id=c.lease_id
               JOIN runtime_bindings b ON b.deployment_id=l.deployment_id
                 AND b.instance_index=l.instance_index
               WHERE b.id=?2 AND l.session_id=?1 AND l.disposition='inflight'
                 AND c.cancelled_at_ms<=?3)",
            params![session.id(), binding_id, asked_at_ms],
        )?;
        if count > 0 {
            crate::events::append_event(
                &tx,
                &crate::events::EventMetadata::RequestCancellationAcknowledged {
                    deployment_id,
                    binding_id: binding_id.to_owned(),
                    count,
                    receipt: receipt.to_owned(),
                },
            )
            .map_err(|error| match error {
                crate::events::EventWriteError::Sql(error) => StoreError::Sql(error),
                _ => StoreError::Conflict,
            })?;
        }
        tx.commit()?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use crate::dispatch::{
        CoordinatorSession, DispatchRequest, DispatchTicket, LeaseWrite, LeaseWriteOutcome,
    };
    use crate::{AcceptDeployment, Store};
    use capyctl_domain::{DeploymentId, LifecycleState, OperationId};

    struct Fixture {
        _directory: tempfile::TempDir,
        path: std::path::PathBuf,
        store: Store,
        session: CoordinatorSession,
        dep: String,
        binding: String,
        ticket: DispatchTicket,
    }

    // Fixture-only readiness and binding: production readiness and bindings
    // come from completion evidence.
    fn ready_instance_with_lease() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let store = Store::open(&path).unwrap();
        let session = store.begin_coordinator_session().unwrap();
        let id = DeploymentId::new();
        store
            .accept_deployment(AcceptDeployment {
                id,
                name: "a".into(),
                kind: "model".into(),
                route_model_id: Some("a".into()),
                desired_state: LifecycleState::Ready,
                schema_version: 1,
                idempotency_key: "a".into(),
                initial_operation_id: OperationId("op-a".into()),
            })
            .unwrap();
        let dep = id.to_string();
        store
            .set_observed_state(&dep, LifecycleState::Ready)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE deployments SET dispatch_enabled=1 WHERE id=?1",
                [&dep],
            )
            .unwrap();
        let binding = "01K00000000000000000000B01".to_owned();
        store
            .conn
            .execute(
                "INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state,instance_index) VALUES(?1,?2,1,'incarnation','managed','{}','[]','live',0)",
                rusqlite::params![binding, dep],
            )
            .unwrap();
        let ticket = store
            .grant_dispatch(
                &session,
                DispatchRequest {
                    deployment_id: &dep,
                    revision: 1,
                    generation: 1,
                    max_per_deployment: 2,
                    max_total: 4,
                },
            )
            .unwrap();
        Fixture {
            _directory: directory,
            path,
            store,
            session,
            dep,
            binding,
            ticket,
        }
    }

    fn cancel(f: &Fixture) {
        let outcome = f
            .store
            .apply_request_lease_batch(&f.session, &[LeaseWrite::Cancel(f.ticket.clone())])
            .unwrap();
        assert!(matches!(
            outcome[..],
            [Ok(LeaseWriteOutcome::Settled(true))]
        ));
    }

    // T17, SPEC §10 (amended): a cancelling lease is still charged: every drain
    // that waits for in-flight work waits for it.
    #[test]
    fn a_cancelling_lease_keeps_every_drain_waiting() {
        let f = ready_instance_with_lease();
        cancel(&f);
        assert_eq!(f.store.switch_outstanding_leases(&f.dep, 0, 1).unwrap(), 1);
        assert_eq!(
            f.store.binding_outstanding_leases(&f.binding).unwrap(),
            Some(1)
        );
        let cancelling = f.store.cancelling_bindings(&f.session).unwrap();
        assert_eq!(cancelling.len(), 1);
        assert_eq!(cancelling[0].binding_id, f.binding);
        assert_eq!(cancelling[0].deployment_id, f.dep);
        assert_eq!(cancelling[0].leases, 1);
    }

    // T17: only cancellations at or before the quiescence question close.
    #[test]
    fn settling_closes_only_leases_cancelled_before_the_question() {
        let f = ready_instance_with_lease();
        cancel(&f);
        let at = f.store.cancelling_bindings(&f.session).unwrap()[0].newest_cancelled_at_ms;
        assert_eq!(
            f.store
                .settle_cancelled_leases(&f.session, &f.binding, at - 1, "r")
                .unwrap(),
            0
        );
        assert_eq!(
            f.store
                .settle_cancelled_leases(&f.session, &f.binding, at, "r")
                .unwrap(),
            1
        );
        assert_eq!(
            f.store.binding_outstanding_leases(&f.binding).unwrap(),
            Some(0)
        );
        assert!(f.store.cancelling_bindings(&f.session).unwrap().is_empty());
        let events = f.store.events_after(None, 100).unwrap().events;
        let acknowledged: Vec<_> = events
            .iter()
            .filter(|e| e.kind == "request_cancellation_acknowledged")
            .collect();
        assert_eq!(acknowledged.len(), 1);
        assert_eq!(
            acknowledged[0].deployment_id.as_deref(),
            Some(f.dep.as_str())
        );
    }

    // T17: a lease that was never cancelled is not closed by quiescence.
    #[test]
    fn settling_leaves_an_uncancelled_lease_charged() {
        let f = ready_instance_with_lease();
        assert_eq!(
            f.store
                .settle_cancelled_leases(&f.session, &f.binding, i64::MAX, "r")
                .unwrap(),
            0
        );
        assert_eq!(
            f.store.binding_outstanding_leases(&f.binding).unwrap(),
            Some(1)
        );
    }

    // T17 T38: after a restart the lease is uncertain; its cancellation row
    // never closes it.
    #[test]
    fn a_cancelling_lease_is_uncertain_after_a_restart() {
        let f = ready_instance_with_lease();
        cancel(&f);
        let Fixture {
            _directory,
            path,
            store,
            binding,
            dep,
            ..
        } = f;
        drop(store);
        let store = Store::open(&path).unwrap();
        let next = store.begin_coordinator_session().unwrap();
        assert!(store.cancelling_bindings(&next).unwrap().is_empty());
        assert_eq!(
            store
                .settle_cancelled_leases(&next, &binding, i64::MAX, "r")
                .unwrap(),
            0
        );
        let pending = store.pending_dispatches(&dep).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending.iter().all(|p| p.uncertain), "{pending:?}");
    }
}

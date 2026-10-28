//! SPEC §10 (amended 2026-10-01): leases of requests whose client hung up.
//! The router closed the engine connection, which asks the engine to abort,
//! but a closed socket is not proof the engine stopped. The lease stays
//! `inflight` and charged until the engine's own counters read quiescent at
//! or after the hang-up; only then does it close.
//!
//! SPEC §10 (amended 2026-10-21): only a cancellation the engine answered
//! first (`engine_answered`: a stream's first output, or a complete error
//! answer) settles on that reading. An engine can read idle while a request
//! it never answered is still on its way to its scheduler, so an unanswered
//! cancellation is settled as below, at the instance's verified exit.
//!
//! ADR 0031 §6, SPEC §10: an engine that reports no idle signal (a custom
//! engine without `idle`) can never read quiescent. Its cancelling leases are
//! never settled while it runs: no quiescence question is asked of it, and
//! only the verified exit of its instance at the next stop or restart settles
//! them (`cleanup.rs`). Drains do not wait for them; they stay charged. Both
//! kinds are "settled at stop" ([`settled_at_stop`]).
//!
//! ADR 0032 §4: on an SGLang lane with the per-request abort, the router
//! aborts a cancelled request by CapyCTL's request ID and keeps reading its
//! response; that request's own final output, read whole, closes its lease
//! alone ([`crate::dispatch::LeaseWrite::AcknowledgeCancelled`]), whatever the
//! engine's counters read and whether or not it would otherwise settle at
//! stop.
//!
//! ADR 0036 §4: a cut embeddings request is read on after its cut; when the
//! engine's complete error answer arrives, its cancellation becomes answered
//! as of then ([`crate::dispatch::LeaseWrite::AnswerCancelled`]) and settles
//! on a later idle reading.

use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::dispatch::{check_session, CoordinatorSession, DispatchError};
use crate::StoreError;

/// ADR 0032 §4: the evidence a per-request acknowledgement names.
pub const ABORTED_REQUEST_RECEIPT: &str =
    "the engine finished this request after it was asked to abort it";

/// ADR 0036 §4 (owner decision 2026-10-27, question 7): the evidence of a cut
/// embeddings request, which is never aborted: the router kept reading it and
/// its answer passed every check.
pub const ANSWERED_REQUEST_RECEIPT: &str =
    "the engine finished this request after the client went away; it settled on its answer";

/// What proved a cancelling lease's request finished, for its receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelledEvidence {
    /// ADR 0032 §4: the request's own final output, read whole after the
    /// engine was asked to abort it.
    Aborted,
    /// ADR 0036 §4: a cut embeddings request read to its end, whose answer
    /// was completion.
    Answered,
}

impl CancelledEvidence {
    /// The receipt journaled with the acknowledgement.
    pub const fn receipt(self) -> &'static str {
        match self {
            Self::Aborted => ABORTED_REQUEST_RECEIPT,
            Self::Answered => ANSWERED_REQUEST_RECEIPT,
        }
    }
}

/// ADR 0032 §4: close the cancelling lease `lease_id` of this session on its
/// own final output, journaling `request_cancellation_acknowledged` with the
/// per-request receipt `evidence` names inside the caller's session-checked
/// transaction. ADR 0036 §4: a cut embeddings request settles the same way on
/// its answer. `Ok(false)` when it is not an open cancelling lease of this
/// session: never cancelled, already settled, or uncertain (a previous
/// session's). A lease held until its instance's stop closes too: the engine
/// proved it finished.
pub(crate) fn acknowledge_in(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    lease_id: &str,
    evidence: CancelledEvidence,
) -> Result<bool, DispatchError> {
    let lease: Option<(String, String)> = tx
        .prepare_cached(
            "SELECT l.deployment_id,b.id FROM request_leases l
               JOIN request_lease_cancellations c ON c.lease_id=l.id
               JOIN runtime_bindings b ON b.deployment_id=l.deployment_id
                 AND b.instance_index=l.instance_index AND b.state!='released'
              WHERE l.id=?1 AND l.session_id=?2 AND l.disposition='inflight'",
        )?
        .query_row(params![lease_id, session.id()], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let Some((deployment_id, binding_id)) = lease else {
        return Ok(false);
    };
    crate::events::append_event(
        tx,
        &crate::events::EventMetadata::RequestCancellationAcknowledged {
            deployment_id,
            binding_id,
            count: 1,
            receipt: format!("{}; request lease {lease_id}", evidence.receipt()),
        },
    )
    .map_err(|error| match error {
        crate::events::EventWriteError::Sql(error) => DispatchError::Sql(error),
        _ => DispatchError::Invalid,
    })?;
    // After the event: a refused event (non-storage) leaves the shared batch
    // transaction untouched, so the lease stays charged.
    tx.prepare_cached("DELETE FROM request_leases WHERE id=?1")?
        .execute([lease_id])?;
    Ok(true)
}

/// ADR 0036 §4: "A complete engine error answer is answered, as today, and
/// settles on the engine's idle reading." A cut embeddings request is
/// cancelled before the engine answers it; when the engine's complete error
/// answer is read afterwards, record its cancellation as answered and move
/// its time to now, so only an idle reading taken after that answer settles
/// it. `Ok(false)` when it is not an open cancelling lease of this session:
/// never cancelled, already settled, or uncertain (a previous session's).
pub(crate) fn answer_in(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    lease_id: &str,
) -> Result<bool, DispatchError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let changed = tx
        .prepare_cached(
            "UPDATE request_lease_cancellations
                SET engine_answered=1,cancelled_at_ms=MAX(cancelled_at_ms,?3)
              WHERE lease_id=?1 AND EXISTS(SELECT 1 FROM request_leases l
                WHERE l.id=?1 AND l.session_id=?2 AND l.disposition='inflight')",
        )?
        .execute(params![lease_id, session.id(), now])?;
    Ok(changed == 1)
}

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

/// ADR 0031 §6: whether the engine of instance lane (`deployment`, `index`),
/// SQL expressions, reports no idle signal: its retained revision (the
/// instance's own, else the deployment's), as resolved on its host when it
/// was, profiles `engine: custom` with no `custom.manifest.idle`. Every
/// built-in engine reports one; a configuration that does not decode is
/// treated as reporting one, so it keeps the conservative built-in rules.
pub(crate) fn lane_without_idle_signal(deployment: &str, index: &str) -> String {
    let chosen = "COALESCE(nh.effective_json,ne.effective_json)";
    format!(
        "EXISTS(SELECT 1 FROM deployment_instances ni JOIN deployments nd ON nd.id=ni.deployment_id
           LEFT JOIN effective_revisions ne ON ne.deployment_id=nd.id AND ne.revision=COALESCE(ni.revision,nd.revision)
           LEFT JOIN host_effective_revisions nh ON nh.deployment_id=nd.id AND nh.revision=COALESCE(ni.revision,nd.revision)
             AND nh.host_id=ni.host_id AND nh.outcome='resolved'
          WHERE ni.deployment_id={deployment} AND ni.instance_index={index}
            AND CASE WHEN json_valid({chosen}) THEN json_extract({chosen},'$.profile.engine')='custom'
                  AND json_type({chosen},'$.profile.custom.manifest.idle') IS NULL ELSE 0 END)"
    )
}

/// SPEC §10 (amended 2026-10-21), ADR 0031 §6: whether cancellation row
/// `cancellation` (an alias of `request_lease_cancellations`) of a lease on
/// instance lane (`deployment`, `index`) settles only at the instance's
/// verified exit: the engine did not answer the request before it was
/// cancelled, or the engine reports no idle signal. Otherwise the engine's
/// idle reading after the cancellation settles it.
pub(crate) fn settled_at_stop(cancellation: &str, deployment: &str, index: &str) -> String {
    format!(
        "({cancellation}.engine_answered=0 OR {})",
        lane_without_idle_signal(deployment, index)
    )
}

/// SPEC §10 (amended 2026-10-21), ADR 0031 §6: whether request lease `lease`
/// (an alias of `request_leases`) is cancelling and settles only at its
/// instance's verified exit ([`settled_at_stop`]). Drains do not wait for
/// such a lease; admission limits still count it.
pub(crate) fn cancelling_settled_at_stop(lease: &str) -> String {
    format!(
        "EXISTS(SELECT 1 FROM request_lease_cancellations xc WHERE xc.lease_id={lease}.id
          AND {})",
        settled_at_stop(
            "xc",
            &format!("{lease}.deployment_id"),
            &format!("{lease}.instance_index")
        )
    )
}

/// v51 (SPEC §10, amended 2026-10-21): record whether the engine answered a
/// cancelled request before its cancellation. A row already present reads 0,
/// not answered: the conservative reading, so no idle reading settles it.
/// Applied once; a store that has the column already is left unchanged.
pub(crate) fn migrate_v51(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    let present: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('request_lease_cancellations') WHERE name='engine_answered')",
        [],
        |r| r.get(0),
    )?;
    if !present {
        tx.execute_batch(
            "ALTER TABLE request_lease_cancellations
               ADD COLUMN engine_answered INTEGER NOT NULL DEFAULT 0 CHECK(engine_answered IN (0,1));",
        )?;
    }
    Ok(())
}

impl crate::Store {
    /// Bindings of this session with at least one cancelling lease that the
    /// engine's idle reading may settle. SPEC §10 (amended 2026-10-21), ADR
    /// 0031 §6: a cancellation settled at stop is not counted, and a binding
    /// with only those is never asked; its verified exit settles them.
    pub fn cancelling_bindings(
        &self,
        session: &CoordinatorSession,
    ) -> Result<Vec<CancellingBinding>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT b.id,b.deployment_id,COUNT(*),MAX(c.cancelled_at_ms) {CANCELLING}
               AND NOT {}
             GROUP BY b.id,b.deployment_id ORDER BY b.id",
            settled_at_stop("c", "l.deployment_id", "l.instance_index")
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
    /// SPEC §10 (amended 2026-10-21), ADR 0031 §6: never a cancellation settled
    /// at stop (unanswered by the engine, or on an engine that reports no idle
    /// signal), whatever the caller observed; only the verified exit settles it.
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
            &format!(
                "DELETE FROM request_leases WHERE id IN (SELECT l.id FROM request_lease_cancellations c
                   JOIN request_leases l ON l.id=c.lease_id
                   JOIN runtime_bindings b ON b.deployment_id=l.deployment_id
                     AND b.instance_index=l.instance_index
                   WHERE b.id=?2 AND l.session_id=?1 AND l.disposition='inflight'
                     AND c.cancelled_at_ms<=?3 AND NOT {})",
                settled_at_stop("c", "l.deployment_id", "l.instance_index")
            ),
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

    /// Cancel the fixture's lease; `answered` when the engine answered it first.
    fn cancel(f: &Fixture, answered: bool) {
        let outcome = f
            .store
            .apply_request_lease_batch(
                &f.session,
                &[LeaseWrite::Cancel {
                    ticket: f.ticket.clone(),
                    answered,
                }],
            )
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
        cancel(&f, true);
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

    // T17 T38, SPEC §10 (amended 2026-10-21): a request cancelled before the
    // engine answered it is never asked about and no idle reading closes it,
    // however late; drains do not wait for it, and it stays charged.
    #[test]
    fn an_unanswered_cancellation_is_never_settled_by_an_idle_reading() {
        let f = ready_instance_with_lease();
        cancel(&f, false);
        assert!(f.store.cancelling_bindings(&f.session).unwrap().is_empty());
        assert_eq!(
            f.store
                .settle_cancelled_leases(&f.session, &f.binding, i64::MAX, "r")
                .unwrap(),
            0
        );
        assert_eq!(f.store.switch_outstanding_leases(&f.dep, 0, 1).unwrap(), 0);
        assert_eq!(
            f.store.binding_outstanding_leases(&f.binding).unwrap(),
            Some(0)
        );
        assert_eq!(f.store.pending_dispatches(&f.dep).unwrap().len(), 1);
        let events = f.store.events_after(None, 100).unwrap().events;
        assert!(!events
            .iter()
            .any(|e| e.kind == "request_cancellation_acknowledged"));
    }

    // T17: only cancellations at or before the quiescence question close.
    #[test]
    fn settling_closes_only_leases_cancelled_before_the_question() {
        let f = ready_instance_with_lease();
        cancel(&f, true);
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
        cancel(&f, true);
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

    /// ADR 0032 §4: acknowledge `lease_id` on its own final output under
    /// `session`; the batch's one outcome.
    fn acknowledge(
        store: &Store,
        session: &CoordinatorSession,
        lease_id: &str,
    ) -> Result<bool, crate::dispatch::DispatchError> {
        acknowledge_by(store, session, lease_id, super::CancelledEvidence::Aborted)
    }

    /// As [`acknowledge`], naming `evidence`.
    fn acknowledge_by(
        store: &Store,
        session: &CoordinatorSession,
        lease_id: &str,
        evidence: super::CancelledEvidence,
    ) -> Result<bool, crate::dispatch::DispatchError> {
        let mut outcome = store.apply_request_lease_batch(
            session,
            &[LeaseWrite::AcknowledgeCancelled {
                lease_id: lease_id.to_owned(),
                evidence,
            }],
        )?;
        match outcome.pop() {
            Some(Ok(LeaseWriteOutcome::Settled(closed))) => Ok(closed),
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    fn acknowledgements(store: &Store) -> Vec<serde_json::Value> {
        store
            .events_after(None, 100)
            .unwrap()
            .events
            .into_iter()
            .filter(|e| e.kind == "request_cancellation_acknowledged")
            .map(|e| serde_json::from_str(&e.payload_json).unwrap())
            .collect()
    }

    // T17 (ADR 0032 §4): a cancelling lease whose request's own final output
    // was read after the abort closes alone, by its ID, with one
    // acknowledgement naming that evidence and the lease, in the same
    // transaction; a neighbour cancelled beside it stays charged.
    #[test]
    fn an_acknowledgement_closes_exactly_that_cancelling_lease_and_journals_it() {
        let f = ready_instance_with_lease();
        cancel(&f, true);
        let neighbour = f
            .store
            .grant_dispatch(
                &f.session,
                DispatchRequest {
                    deployment_id: &f.dep,
                    revision: 1,
                    generation: 1,
                    max_per_deployment: 2,
                    max_total: 4,
                },
            )
            .unwrap();
        f.store
            .apply_request_lease_batch(
                &f.session,
                &[LeaseWrite::Cancel {
                    ticket: neighbour.clone(),
                    answered: true,
                }],
            )
            .unwrap();
        assert!(acknowledge(&f.store, &f.session, f.ticket.id()).unwrap());
        let pending = f.store.pending_dispatches(&f.dep).unwrap();
        assert_eq!(
            pending.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            [neighbour.id()]
        );
        let acknowledged = acknowledgements(&f.store);
        assert_eq!(acknowledged.len(), 1, "{acknowledged:?}");
        assert_eq!(acknowledged[0]["count"], 1);
        assert_eq!(acknowledged[0]["binding_id"], f.binding.as_str());
        assert_eq!(acknowledged[0]["deployment_id"], f.dep.as_str());
        assert_eq!(
            acknowledged[0]["receipt"],
            format!(
                "{}; request lease {}",
                super::ABORTED_REQUEST_RECEIPT,
                f.ticket.id()
            )
        );
        // Settled once: a second acknowledgement changes nothing.
        assert!(!acknowledge(&f.store, &f.session, f.ticket.id()).unwrap());
        assert_eq!(acknowledgements(&f.store).len(), 1);
    }

    // T17 (ADR 0032 §4): a request the engine never answered before its cut
    // would settle only at the instance's verified exit; its own final output
    // after the abort proves it finished, so it closes now.
    #[test]
    fn an_acknowledgement_closes_a_lease_held_until_the_next_stop() {
        let f = ready_instance_with_lease();
        cancel(&f, false);
        assert!(acknowledge(&f.store, &f.session, f.ticket.id()).unwrap());
        assert!(f.store.pending_dispatches(&f.dep).unwrap().is_empty());
        assert_eq!(acknowledgements(&f.store).len(), 1);
    }

    // T47 T17 (ADR 0036 §4, owner decision 2026-10-27, question 7): a cut
    // embeddings request read to its end settles on its answer, closing its
    // cancelling lease (even one held until the next stop) with the
    // embeddings receipt, never the abort's.
    #[test]
    fn a_cut_embedding_settles_on_its_answer_with_its_own_receipt() {
        for answered in [true, false] {
            let f = ready_instance_with_lease();
            cancel(&f, answered);
            assert!(acknowledge_by(
                &f.store,
                &f.session,
                f.ticket.id(),
                super::CancelledEvidence::Answered
            )
            .unwrap());
            assert!(f.store.pending_dispatches(&f.dep).unwrap().is_empty());
            let acknowledged = acknowledgements(&f.store);
            assert_eq!(acknowledged.len(), 1, "{acknowledged:?}");
            assert_eq!(
                acknowledged[0]["receipt"],
                format!(
                    "the engine finished this request after the client went away; it settled \
                     on its answer; request lease {}",
                    f.ticket.id()
                )
            );
        }
    }

    // T17 (ADR 0032 §4): a lease that was never cancelled, or already settled
    // on the engine's idle reading, is not closed by an acknowledgement, and
    // nothing is journaled for it.
    #[test]
    fn an_acknowledgement_leaves_an_uncancelled_or_settled_lease_alone() {
        let f = ready_instance_with_lease();
        assert!(!acknowledge(&f.store, &f.session, f.ticket.id()).unwrap());
        assert_eq!(f.store.pending_dispatches(&f.dep).unwrap().len(), 1);
        assert!(acknowledgements(&f.store).is_empty());
        cancel(&f, true);
        assert_eq!(
            f.store
                .settle_cancelled_leases(&f.session, &f.binding, i64::MAX, "r")
                .unwrap(),
            1
        );
        assert!(!acknowledge(&f.store, &f.session, f.ticket.id()).unwrap());
        assert_eq!(acknowledgements(&f.store).len(), 1, "only the reading's");
        assert!(!acknowledge(&f.store, &f.session, "01K00000000000000000000X99").unwrap());
    }

    // T17 T38 (ADR 0032 §4): after a role restart the cancelling lease is
    // uncertain and belongs to the previous session: the new session's
    // acknowledgement leaves it charged, and the previous session can no
    // longer write at all.
    #[test]
    fn an_acknowledgement_never_closes_an_uncertain_or_other_session_lease() {
        let f = ready_instance_with_lease();
        cancel(&f, true);
        let Fixture {
            _directory,
            path,
            store,
            session,
            dep,
            ticket,
            ..
        } = f;
        drop(store);
        let store = Store::open(&path).unwrap();
        let next = store.begin_coordinator_session().unwrap();
        assert!(!acknowledge(&store, &next, ticket.id()).unwrap());
        assert!(matches!(
            acknowledge(&store, &session, ticket.id()),
            Err(crate::dispatch::DispatchError::StaleSession)
        ));
        let pending = store.pending_dispatches(&dep).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].uncertain);
        assert!(acknowledgements(&store).is_empty());
    }

    /// ADR 0036 §4: record `lease_id`'s cancellation as answered by the
    /// engine's complete error answer under `session`; the batch's one outcome.
    fn answer(
        store: &Store,
        session: &CoordinatorSession,
        lease_id: &str,
    ) -> Result<bool, crate::dispatch::DispatchError> {
        let mut outcome = store.apply_request_lease_batch(
            session,
            &[LeaseWrite::AnswerCancelled {
                lease_id: lease_id.to_owned(),
            }],
        )?;
        match outcome.pop() {
            Some(Ok(LeaseWriteOutcome::Settled(changed))) => Ok(changed),
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    // T47 T17 (ADR 0036 §4): a cut embeddings request cancelled before the
    // engine answered, whose complete error answer is read afterwards, is
    // answered as of that answer: drains wait for it again, and only an idle
    // reading taken after the answer settles it, never an earlier one.
    #[test]
    fn a_complete_error_answer_after_a_cut_settles_on_a_later_idle_reading() {
        let f = ready_instance_with_lease();
        cancel(&f, false);
        f.store
            .conn
            .execute(
                "UPDATE request_lease_cancellations SET cancelled_at_ms=1 WHERE lease_id=?1",
                [f.ticket.id()],
            )
            .unwrap();
        assert!(f.store.cancelling_bindings(&f.session).unwrap().is_empty());
        assert!(answer(&f.store, &f.session, f.ticket.id()).unwrap());
        let cancelling = f.store.cancelling_bindings(&f.session).unwrap();
        assert_eq!(cancelling.len(), 1);
        assert_eq!(cancelling[0].leases, 1);
        let at = cancelling[0].newest_cancelled_at_ms;
        assert!(at > 1, "the cancellation counts from the answer");
        assert_eq!(f.store.switch_outstanding_leases(&f.dep, 0, 1).unwrap(), 1);
        assert_eq!(
            f.store
                .settle_cancelled_leases(&f.session, &f.binding, at - 1, "r")
                .unwrap(),
            0,
            "a reading before the answer settles nothing"
        );
        assert_eq!(
            f.store
                .settle_cancelled_leases(&f.session, &f.binding, at, "r")
                .unwrap(),
            1
        );
        assert!(f.store.pending_dispatches(&f.dep).unwrap().is_empty());
    }

    // T47 T17 T38 (ADR 0036 §4): only an open cancelling lease of this session
    // is answered: a lease never cancelled, already settled, unknown, or
    // uncertain after a restart is left alone, and the previous session can
    // no longer write at all.
    #[test]
    fn answering_leaves_any_other_lease_alone() {
        let f = ready_instance_with_lease();
        assert!(!answer(&f.store, &f.session, f.ticket.id()).unwrap());
        assert!(!answer(&f.store, &f.session, "01K00000000000000000000X99").unwrap());
        cancel(&f, true);
        assert_eq!(
            f.store
                .settle_cancelled_leases(&f.session, &f.binding, i64::MAX, "r")
                .unwrap(),
            1
        );
        assert!(!answer(&f.store, &f.session, f.ticket.id()).unwrap());

        let f = ready_instance_with_lease();
        cancel(&f, false);
        let Fixture {
            _directory,
            path,
            store,
            session,
            dep,
            ticket,
            ..
        } = f;
        drop(store);
        let store = Store::open(&path).unwrap();
        let next = store.begin_coordinator_session().unwrap();
        assert!(!answer(&store, &next, ticket.id()).unwrap());
        assert!(matches!(
            answer(&store, &session, ticket.id()),
            Err(crate::dispatch::DispatchError::StaleSession)
        ));
        let answered: bool = store
            .conn
            .query_row(
                "SELECT engine_answered FROM request_lease_cancellations WHERE lease_id=?1",
                [ticket.id()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!answered);
        assert_eq!(store.pending_dispatches(&dep).unwrap().len(), 1);
    }
}

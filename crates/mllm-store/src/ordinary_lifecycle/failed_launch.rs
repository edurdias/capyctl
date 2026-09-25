//! Releasing a launch that failed after its step armed.
//!
//! Spec §6: once a step arms, a reservation, a port lease, a binding and an engine
//! key exist. When the launch then fails, all of that must be given up in one
//! transaction, and only against proof that every process the launch recorded is
//! gone. The guards are those of ordinary cleanup, less the step-deadline bound.
use super::*;
use crate::lifecycle::completion::{identities, nonempty_receipt};
use mllm_domain::completion::{CleanupEvidence, ProcessIdentity};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceKind {
    FailedLaunchReleased,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Gone {
    version: u8,
    kind: EvidenceKind,
    step_id: String,
    binding_id: String,
    incarnation: String,
    identities: Vec<IdentityDto>,
    observed_at_ms: i64,
    receipt: String,
}

/// The one encoding of this evidence. Replay compares these bytes, so every field
/// the guard reads is inside them and nothing else is.
fn evidence_value(id: &str, e: &CleanupEvidence) -> Result<String, LifecycleError> {
    nonempty_receipt(&e.receipt)?;
    encode(&Gone {
        version: 1,
        kind: EvidenceKind::FailedLaunchReleased,
        step_id: id.into(),
        binding_id: e.binding_id.clone(),
        incarnation: e.incarnation.clone(),
        identities: identity_dtos(&canonical_members_or_empty(&e.identities)?),
        observed_at_ms: e.observed_at_ms,
        receipt: e.receipt.clone(),
    })
}

/// Spec §6: the identity sets a failing launch can have recorded are the ones its
/// launch path is able to write. Nothing at all, when the gate never opened. The
/// API process alone, because a durable launcher records that identity as soon as
/// the process exists. Or the full canonical set, once the launch was associated.
/// `canonical_members` covers only the last of the three, so the two earlier shapes
/// are accepted here under the same per-identity rules. Field sizes are bounded by
/// `encode` on the way in and by `decode` on the way out.
pub(super) fn canonical_members_or_empty(
    ids: &[ProcessIdentity],
) -> Result<Vec<ProcessIdentity>, LifecycleError> {
    match ids {
        [] => Ok(Vec::new()),
        [only] => {
            if only.role != "api"
                || only.pid == 0
                || only.start_ticks == 0
                || only.boot_id.trim().is_empty()
            {
                return Err(LifecycleError::Invalid);
            }
            Ok(vec![only.clone()])
        }
        _ => canonical_members(ids),
    }
}

/// What the binding records as having been started, in the same normal form the
/// evidence is put into, so the two are compared as sets and not as orderings.
pub(super) fn recorded_identities(
    tx: &Transaction<'_>,
    binding_id: &str,
) -> Result<Vec<ProcessIdentity>, LifecycleError> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT identities_json FROM runtime_bindings WHERE id=?1",
            [binding_id],
            |r| r.get(0),
        )
        .optional()?;
    let dtos: Vec<IdentityDto> = decode(&raw.ok_or(LifecycleError::Conflict)?)?;
    canonical_members_or_empty(&identities(&dtos)).map_err(|_| LifecycleError::CorruptStoredData)
}

/// Spec §6: the freshness window of ordinary cleanup, without its `now <= deadline`
/// bound. A launch that failed by running out its own deadline can only be observed
/// gone after that deadline, so the bound is unsatisfiable here by construction; the
/// ttl still holds the observation to a bounded age.
fn fresh_after_deadline(
    issued: i64,
    observed: i64,
    now: i64,
    ttl: i64,
) -> Result<(), LifecycleError> {
    if issued < 0
        || ttl <= 0
        || observed < issued
        || now < observed
        || observed.checked_add(ttl).is_none_or(|end| now > end)
    {
        return Err(LifecycleError::Rejected("evidence freshness".into()));
    }
    Ok(())
}

impl crate::Store {
    /// Spec §6: release a launch that failed after arm, against proof that nothing
    /// it started remains. The step is cancelled, the run and the operation fail,
    /// and the binding, port lease, resource grant, claim and engine key are given
    /// up in the same transaction that records the evidence at a new completion
    /// epoch. The deployment's `desired_state` is not touched: what the owner asked
    /// for did not change because one launch failed.
    ///
    /// The guards are those of `complete_cleanup` with two deliberate differences.
    /// The step-deadline bound is dropped, for the reason `fresh_after_deadline`
    /// gives. And `complete_cleanup` refuses while the deployment holds a request
    /// lease under any other fence, where this releases its own fence's leases
    /// without that check: an Initialize never enables dispatch, so a lease under
    /// another fence is not evidence about this launch, and refusing on it would
    /// leave a failed launch retained over a fence it cannot affect.
    pub fn release_failed_launch(
        &self,
        s: &CoordinatorSession,
        id: &str,
        evidence: &CleanupEvidence,
        now: i64,
        ttl: i64,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        release(&tx, s, id, evidence, now, ttl)?;
        tx.commit()?;
        Ok(())
    }

    /// Spec §6: the ttl the release guard checks is the host's observation ttl on
    /// the step's own effective revision. A caller reads it here so that it offers
    /// the value the guard will compare against, rather than one derived elsewhere
    /// that the guard would then refuse.
    pub fn observation_ttl_for_step(&self, id: &str) -> Result<i64, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let (_, e, _) = load(&tx, id)?;
        Ok(e.host.observation_ttl_ms)
    }

    /// The processes a binding has on record, which are exactly the ones a release
    /// must prove gone. A caller builds its evidence from this, so that the set it
    /// offers is the set the guard requires.
    pub fn runtime_binding_identities(
        &self,
        binding_id: &str,
    ) -> Result<Vec<ProcessIdentity>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        recorded_identities(&tx, binding_id)
    }
}

fn release(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
    evidence: &CleanupEvidence,
    now: i64,
    ttl: i64,
) -> Result<(), LifecycleError> {
    if !is_ordinary(tx, id)? {
        return Err(LifecycleError::Unsupported);
    }
    let supplied = evidence_value(id, evidence)?;
    // A replay is answered from the evidence alone, before the plan is loaded. The
    // release deletes the reservation and the claim the plan describes, so after it
    // the plan no longer reads back as live work; the recorded evidence is what
    // says the release already happened, and it says so exactly.
    if let Some(old) = recorded(tx, id)? {
        return if old == supplied {
            Ok(())
        } else {
            Err(LifecycleError::Conflict)
        };
    }
    let (p, e, state) = load(tx, id)?;
    // Admission may already have been closed by whatever noticed the failure, so
    // this is the current check without the admission requirement.
    current_admitted(tx, s, &p, false, false)?;
    if !matches!(state.as_str(), "armed" | "uncertain")
        || evidence.binding_id != p.binding_id
        || evidence.incarnation != p.incarnation
        || canonical_members_or_empty(&evidence.identities)?
            != recorded_identities(tx, &p.binding_id)?
    {
        return Err(LifecycleError::Conflict);
    }
    if ttl != e.host.observation_ttl_ms {
        return Err(LifecycleError::Invalid);
    }
    let execution = p.execution.as_ref().ok_or(LifecycleError::Conflict)?;
    fresh_after_deadline(execution.issued_at_ms, evidence.observed_at_ms, now, ttl)?;
    one(tx.execute(
        "UPDATE lifecycle_steps SET state='cancelled' WHERE id=?1 AND state IN ('armed','uncertain')",
        [id],
    )?)?;
    one(tx.execute(
        "UPDATE lifecycle_runs SET state='failed' WHERE operation_id=?1 AND state IN ('running','uncertain')",
        [&p.operation_id],
    )?)?;
    one(tx.execute(
        "UPDATE operations SET state='failed',error_code='launch_failed' WHERE id=?1 AND state='running'",
        [&p.operation_id],
    )?)?;
    one(tx.execute(
        "DELETE FROM resource_owners WHERE owner_id=?1",
        [&p.owner()],
    )?)?;
    one(tx.execute(
        "DELETE FROM endpoint_leases WHERE binding_id=?1",
        [&p.binding_id],
    )?)?;
    one(tx.execute(
        "UPDATE runtime_bindings SET state='released' WHERE id=?1 AND state='uncertain'",
        [&p.binding_id],
    )?)?;
    // Spec §3: the encrypted engine key does not outlive the binding it was issued
    // for. A launch may fail before a key was ever stored, so this is not `one`.
    tx.execute(
        "DELETE FROM engine_secrets WHERE binding_id=?1",
        [&p.binding_id],
    )?;
    one(tx.execute(
        "DELETE FROM lifecycle_claims WHERE operation_id=?1",
        [&p.operation_id],
    )?)?;
    // An initialize never enables dispatch, so a lease on this fence would be one
    // nothing can hold any more. Dropping it keeps the fence clean for the next
    // attempt; there is ordinarily nothing to drop.
    tx.execute("DELETE FROM request_leases WHERE deployment_id=?1 AND revision=?2 AND generation=?3 AND session_id=?4",params![p.deployment_id,p.revision,p.generation,p.session_id])?;
    // The epoch advances because a grant was released, which is what it counts.
    let epoch = resource_ledger::advance_completion_epoch(tx)?;
    tx.execute(
        "INSERT INTO lifecycle_evidence(step_id,evidence_json,committed_epoch) VALUES(?1,?2,?3)",
        params![id, supplied, epoch],
    )?;
    event(tx, s, &p, Transition::LaunchFailed, Some(epoch))
}

/// Evidence already recorded against this step, if the release ran before. What it
/// released is checked here too, so a replay confirms a finished release rather
/// than merely a matching record.
fn recorded(tx: &Transaction<'_>, id: &str) -> Result<Option<String>, LifecycleError> {
    let prior: Option<String> = tx
        .query_row(
            "SELECT evidence_json FROM lifecycle_evidence WHERE step_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(raw) = prior else {
        return Ok(None);
    };
    let Ok(gone) = decode::<Gone>(&raw) else {
        // Some other kind of evidence stands against this step. This release did
        // not write it and must not claim it as its own replay; returning it as it
        // stands refuses the call, because it cannot equal a release encoding.
        return Ok(Some(raw));
    };
    let released:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps WHERE id=?1 AND state='cancelled') AND EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?2 AND state='released') AND NOT EXISTS(SELECT 1 FROM endpoint_leases WHERE binding_id=?2) AND NOT EXISTS(SELECT 1 FROM engine_secrets WHERE binding_id=?2)",params![id,gone.binding_id],|r|r.get(0))?;
    if gone.version != 1 || gone.step_id != id || !released {
        return Err(LifecycleError::CorruptStoredData);
    }
    nonempty_receipt(&gone.receipt)?;
    Ok(Some(raw))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{armed_ordinary, identity};
    use super::*;
    use crate::secrets::{new_engine_key, SecretRole, SecretsKey};

    fn text(store: &crate::Store, sql: &str, id: &str) -> String {
        store.conn.query_row(sql, [id], |r| r.get(0)).unwrap()
    }
    fn count(store: &crate::Store, sql: &str, id: &str) -> i64 {
        store.conn.query_row(sql, [id], |r| r.get(0)).unwrap()
    }
    fn step_state(store: &crate::Store, id: &str) -> String {
        text(store, "SELECT state FROM lifecycle_steps WHERE id=?1", id)
    }
    fn run_state(store: &crate::Store, operation: &str) -> String {
        text(
            store,
            "SELECT state FROM lifecycle_runs WHERE operation_id=?1",
            operation,
        )
    }
    fn binding_state(store: &crate::Store, id: &str) -> String {
        text(store, "SELECT state FROM runtime_bindings WHERE id=?1", id)
    }
    fn desired_state(store: &crate::Store, id: &str) -> String {
        text(
            store,
            "SELECT desired_state FROM deployments WHERE id=?1",
            id,
        )
    }

    // T30/T34: an explicit retry is legal only after verified failed-launch cleanup.
    #[test]
    fn start_after_verified_failure_creates_a_fresh_operation() {
        let (store, session, fence, execution) = armed_ordinary();
        let now = execution.issued_at_ms + 10;
        let evidence = CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities: vec![],
            observed_at_ms: now,
            receipt: "no process was associated".into(),
        };
        let ttl = store
            .observation_ttl_for_step(&execution.token.step_id)
            .unwrap();
        store
            .release_failed_launch(&session, &execution.token.step_id, &evidence, now, ttl)
            .unwrap();
        store
            .set_admission_enabled(&fence.deployment_id, false)
            .unwrap();
        let retry = store
            .accept_start(&session, &fence, now + 1, now + 1000)
            .unwrap();
        assert!(!retry.joined);
        assert_ne!(retry.operation_id, execution.token.operation_id);
        assert_ne!(retry.binding_id, execution.binding_id);
    }

    /// Spec §6: a launch that failed after arm is released with gone evidence. The
    /// step is cancelled, the run and the operation fail, the binding, lease, grant
    /// and claim are released, the engine key row is deleted, and the deployment
    /// keeps the ready state its owner asked for.
    // T30
    // T34
    #[test]
    fn a_failed_launch_is_released_with_evidence() {
        let (mut store, session, fence, execution) = armed_ordinary();
        store.set_secrets_key(SecretsKey::generate_ephemeral());
        let api = identity("api", 7);
        store
            .record_api_identity(&session, &fence, &execution.binding_id, &api)
            .unwrap();
        store
            .store_engine_key(
                &execution.binding_id,
                &execution.incarnation,
                &new_engine_key(),
                SecretRole::Inference,
            )
            .unwrap();
        let step = execution.token.step_id.clone();
        let operation = execution.token.operation_id.clone();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        assert_eq!(
            store
                .runtime_binding_identities(&execution.binding_id)
                .unwrap(),
            vec![api.clone()],
            "the binding records the API process the launcher wrote"
        );
        let before = store.resource_snapshot().unwrap().epoch;
        let now = execution.issued_at_ms + 10;
        let evidence = CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities: vec![api],
            observed_at_ms: now,
            receipt: "every recorded process observed gone".into(),
        };
        store
            .release_failed_launch(&session, &step, &evidence, now, ttl)
            .unwrap();

        assert_eq!(step_state(&store, &step), "cancelled");
        assert_eq!(run_state(&store, &operation), "failed");
        assert_eq!(
            text(
                &store,
                "SELECT state FROM operations WHERE id=?1",
                &operation
            ),
            "failed"
        );
        assert_eq!(
            text(
                &store,
                "SELECT error_code FROM operations WHERE id=?1",
                &operation
            ),
            "launch_failed"
        );
        assert_eq!(binding_state(&store, &execution.binding_id), "released");
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM endpoint_leases WHERE binding_id=?1",
                &execution.binding_id
            ),
            0
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM resource_owners WHERE owner_id=?1",
                &fence.deployment_id
            ),
            0
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM engine_secrets WHERE binding_id=?1",
                &execution.binding_id
            ),
            0
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM lifecycle_claims WHERE operation_id=?1",
                &operation
            ),
            0
        );
        assert_eq!(
            desired_state(&store, &fence.deployment_id),
            "ready",
            "a failed launch does not change what the owner asked for"
        );
        assert!(
            store.resource_snapshot().unwrap().epoch > before,
            "releasing a grant advances the completion epoch"
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM lifecycle_evidence WHERE step_id=?1",
                &step
            ),
            1
        );

        store
            .release_failed_launch(&session, &step, &evidence, now + 1, ttl)
            .unwrap();
        let other = CleanupEvidence {
            receipt: "different".into(),
            ..evidence.clone()
        };
        assert!(matches!(
            store.release_failed_launch(&session, &step, &other, now + 1, ttl),
            Err(LifecycleError::Conflict)
        ));
    }

    /// Spec §6 step 1: when no identity was ever recorded, that is itself the
    /// evidence, and the identity set offered must be empty in the same way.
    // T30
    #[test]
    fn a_never_released_launch_is_released_with_an_empty_identity_set() {
        let (store, session, fence, execution) = armed_ordinary();
        let step = execution.token.step_id.clone();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        assert!(store
            .runtime_binding_identities(&execution.binding_id)
            .unwrap()
            .is_empty());
        let now = execution.issued_at_ms + 10;
        let evidence = CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities: vec![],
            observed_at_ms: now,
            receipt: "gate never opened; child disposed".into(),
        };
        store
            .release_failed_launch(&session, &step, &evidence, now, ttl)
            .unwrap();
        assert_eq!(step_state(&store, &step), "cancelled");
        assert_eq!(binding_state(&store, &execution.binding_id), "released");
        assert_eq!(desired_state(&store, &fence.deployment_id), "ready");
    }

    /// Spec §6: an empty recorded set is matched only by an empty evidence set. A
    /// release that names a process the binding never recorded proves nothing.
    // T32
    #[test]
    fn evidence_naming_an_unrecorded_process_is_refused() {
        let (store, session, _, execution) = armed_ordinary();
        let step = execution.token.step_id.clone();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = execution.issued_at_ms + 10;
        let evidence = CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities: vec![identity("api", 7)],
            observed_at_ms: now,
            receipt: "observed gone".into(),
        };
        assert!(matches!(
            store.release_failed_launch(&session, &step, &evidence, now, ttl),
            Err(LifecycleError::Conflict)
        ));
        assert_eq!(step_state(&store, &step), "armed");
    }

    /// Spec §6: the evidence's identity set must equal what was recorded.
    // T32
    #[test]
    fn mismatched_identities_are_refused() {
        let (store, session, fence, execution) = armed_ordinary();
        store
            .record_api_identity(&session, &fence, &execution.binding_id, &identity("api", 7))
            .unwrap();
        let step = execution.token.step_id.clone();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = execution.issued_at_ms + 10;
        let evidence = CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities: vec![identity("api", 8)],
            observed_at_ms: now,
            receipt: "observed gone".into(),
        };
        assert!(matches!(
            store.release_failed_launch(&session, &step, &evidence, now, ttl),
            Err(LifecycleError::Conflict)
        ));
        assert_eq!(step_state(&store, &step), "armed");
        assert_eq!(binding_state(&store, &execution.binding_id), "uncertain");
    }

    /// Spec §6: a deadline-triggered failure can only be observed gone after that
    /// deadline, so evidence past it is accepted while the ttl still holds the
    /// observation to a bounded age.
    // T32
    #[test]
    fn evidence_after_the_step_deadline_is_accepted_when_fresh() {
        let (store, session, _, execution) = armed_ordinary();
        let step = execution.token.step_id.clone();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = execution.deadline_ms + 5_000;
        let stale = CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities: vec![],
            observed_at_ms: now - ttl - 1,
            receipt: "observed gone, too long ago".into(),
        };
        assert!(
            matches!(
                store.release_failed_launch(&session, &step, &stale, now, ttl),
                Err(LifecycleError::Rejected(_))
            ),
            "the ttl still bounds the age of the observation"
        );
        let fresh = CleanupEvidence {
            observed_at_ms: now - 100,
            receipt: "observed gone after the deadline".into(),
            ..stale.clone()
        };
        store
            .release_failed_launch(&session, &step, &fresh, now, ttl)
            .unwrap();
        assert_eq!(step_state(&store, &step), "cancelled");
    }

    /// The ttl a caller offers must be the one the host's policy sets, so that a
    /// looser window cannot be smuggled in with the evidence.
    // T32
    #[test]
    fn a_ttl_that_is_not_the_hosts_is_refused() {
        let (store, session, _, execution) = armed_ordinary();
        let step = execution.token.step_id.clone();
        let ttl = store.observation_ttl_for_step(&step).unwrap();
        let now = execution.issued_at_ms + 10;
        let evidence = CleanupEvidence {
            binding_id: execution.binding_id.clone(),
            incarnation: execution.incarnation.clone(),
            identities: vec![],
            observed_at_ms: now,
            receipt: "observed gone".into(),
        };
        assert!(matches!(
            store.release_failed_launch(&session, &step, &evidence, now, ttl + 1),
            Err(LifecycleError::Invalid)
        ));
    }
}

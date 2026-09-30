//! Bounded discovery and conservative failure recording for the owned worker.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitializeStatus {
    Planned,
    Armed,
    Completed,
    Uncertain,
    Superseded,
    /// The step is still planned, but its deployment closed its own admission.
    /// It is not superseded work: it is a deployment that gave up and now waits
    /// for its deadline or for an operator Stop.
    // ADR 0011 decision 4: a failed deployment stops itself, not the host.
    Closed,
    Expired,
    ExpiredUnarmed,
}

/// One bounded discovery result. Expiry is committed before this is returned;
/// work still requires a fresh arm before any execution.
pub enum InitializePoll {
    Idle,
    ExpiredUnarmed,
    Work(Box<InitializeWork>),
}

/// The oldest planned step of this session, in durable acceptance order.
///
/// `admitted` selects what the caller is looking for. Work must come from a
/// deployment that is still admitting; the deadline release must be able to see
/// a step whose deployment has closed its own admission.
fn next_plan(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    admitted: bool,
    busy: &super::lanes::BusyLanes,
) -> Result<Option<(Plan, EffectiveDeployment)>, LifecycleError> {
    let id: Option<String> = tx
        .query_row(
            // ADR 0011 decision 4: a deployment whose admission is closed is
            // closed for work. It must not be returned here, because the arm
            // that would follow refuses it and one deployment's failure would
            // again stop every other deployment on the host.
            // SPEC §6.1 FAILED: admission closed.
            // ADR 0015: a step on a busy instance, or a held start, is skipped
            // so that it cannot hide every step queued behind it.
            &format!(
                "SELECT s.id FROM lifecycle_steps s
         JOIN operations o ON o.id=s.operation_id
         JOIN lifecycle_runs r ON r.operation_id=o.id
         JOIN instance_runtime d ON d.id=s.deployment_id AND d.instance_index=r.instance_index
         WHERE o.kind='initialize' AND s.state='planned'
           AND s.session_id=?1 AND r.session_id=?1
           AND d.revision=r.revision AND d.current_generation=r.generation
           AND d.desired_state='ready' AND d.suspended=0
           AND (?2=0 OR d.admission_enabled=1)
           AND {lane_free}
           AND s.binding_id NOT IN (SELECT value FROM json_each(?4))
         ORDER BY o.accepted_at,o.id LIMIT 1",
                lane_free = super::lanes::lane_free(3)
            ),
            params![
                session.id(),
                admitted,
                busy.instances_json()?,
                busy.held_json()?
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = id else {
        return Ok(None);
    };
    if id.len() != 26 {
        return Err(LifecycleError::CorruptStoredData);
    }
    let (plan, effective, state) = load(tx, &id)?;
    current_admitted(tx, session, &plan, false, admitted)?;
    if state != "planned" {
        return Err(LifecycleError::Conflict);
    }
    Ok(Some((plan, effective)))
}

/// The planned step of this session with the earliest deadline that has already
/// passed, whether or not its deployment is still admitting.
///
/// Expiry is a release, not work, so the admission predicate is left out: a
/// deployment that closed its own admission must still reach its deadline. The
/// order is by deadline and not by acceptance, so that a step whose deadline has
/// passed never waits behind an older step whose deadline has not. The run's
/// `deadline_ms` is the plan's own deadline; `validate_initialize_run` proves the
/// two agree, and the check below refuses the pair if they ever disagree.
// ADR 0011 decision 4: a failed deployment stops itself, not the host.
fn next_expired_plan(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    now_ms: i64,
    busy: &super::lanes::BusyLanes,
) -> Result<Option<(Plan, EffectiveDeployment)>, LifecycleError> {
    let id: Option<String> = tx
        .query_row(
            // ADR 0015: a step whose instance has an effect in flight is that
            // effect's to settle, deadline included; it is not expired here.
            &format!(
                "SELECT s.id FROM lifecycle_steps s
         JOIN operations o ON o.id=s.operation_id
         JOIN lifecycle_runs r ON r.operation_id=o.id
         JOIN instance_runtime d ON d.id=s.deployment_id AND d.instance_index=r.instance_index
         WHERE o.kind='initialize' AND s.state='planned'
           AND s.session_id=?1 AND r.session_id=?1
           AND d.revision=r.revision AND d.current_generation=r.generation
           AND d.desired_state='ready' AND d.suspended=0
           AND r.deadline_ms<=?2
           AND {lane_free}
         ORDER BY r.deadline_ms,o.accepted_at,o.id LIMIT 1",
                lane_free = super::lanes::lane_free(3)
            ),
            params![session.id(), now_ms, busy.instances_json()?],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = id else {
        return Ok(None);
    };
    if id.len() != 26 {
        return Err(LifecycleError::CorruptStoredData);
    }
    let (plan, effective, state) = load(tx, &id)?;
    current_admitted(tx, session, &plan, false, false)?;
    if state != "planned" {
        return Err(LifecycleError::Conflict);
    }
    if now_ms < plan.deadline_ms {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(Some((plan, effective)))
}

/// The most retired planned steps one discovery pass transfers.
const MAX_RETIRED_PLANNED: i64 = 64;

/// SPEC §13.2: reconcile before dispatch. A planned Initialize has had no
/// effect: nothing is armed, granted or sent, so no process can exist for it.
/// Found live (Phase B): a step left planned by a retired coordinator session
/// was neither armed nor expired by any later session, and its claim made every
/// later Start and Stop of that deployment fail as `reconciliation_required`
/// for ever. Such a step is moved to this session unchanged, keeping its
/// original deadline, so it is armed within that deadline or expired at it like
/// any other. A step that does not validate as effect-free stays where it is.
fn adopt_retired_planned(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
) -> Result<(), LifecycleError> {
    let ids = tx
        .prepare(
            "SELECT s.id FROM lifecycle_steps s
             JOIN operations o ON o.id=s.operation_id
             JOIN lifecycle_runs r ON r.operation_id=o.id
             WHERE o.kind='initialize' AND s.state='planned' AND r.state='queued'
               AND o.state='pending' AND s.grant_id IS NULL
               AND s.session_id!=?1 AND r.session_id=s.session_id
             ORDER BY o.accepted_at,o.id LIMIT ?2",
        )?
        .query_map(params![session.id(), MAX_RETIRED_PLANNED], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        let (mut plan, _, state) = load(tx, &id)?;
        let granted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM resource_grants WHERE operation_id=?1)
             OR EXISTS(SELECT 1 FROM lifecycle_evidence WHERE step_id=?2)
             OR EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?3 AND state!='reserved')",
            params![plan.operation_id, plan.step_id, plan.binding_id],
            |r| r.get(0),
        )?;
        if state != "planned"
            || plan.execution.is_some()
            || plan.session_id == session.id()
            || granted
            || association(tx, &plan)?.is_some()
        {
            continue;
        }
        let retired = std::mem::replace(&mut plan.session_id, session.id().into());
        one(tx.execute(
            "UPDATE lifecycle_steps SET session_id=?2,step_json=?3 WHERE id=?1 AND session_id=?4 AND state='planned'",
            params![id, session.id(), encode(&plan)?, retired],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET session_id=?2 WHERE operation_id=?1 AND session_id=?3 AND state='queued'",
            params![plan.operation_id, session.id(), retired],
        )?)?;
        tx.execute(
            "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,NULL,?2,'planned_initialize_adopted',?3)",
            params![
                ulid::Ulid::new().to_string(),
                plan.operation_id,
                format!(
                    "deployment {}: a restarted controller adopted a start that never armed; \
                     it keeps its original deadline",
                    plan.deployment_id
                ),
            ],
        )?;
    }
    Ok(())
}

/// Whether this plan's instance is still admitting work (ADR 0013 §6: one
/// instance that gave up closes its own admission, not its siblings').
fn admitting(tx: &Transaction<'_>, plan: &Plan) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2 AND admission_enabled=1)",
        params![plan.deployment_id, plan.instance_index],
        |row| row.get(0),
    )?)
}

pub(super) fn prepare_work(
    tx: &Transaction<'_>,
    plan: Plan,
    effective: EffectiveDeployment,
) -> Result<InitializeWork, LifecycleError> {
    let policy = policy(tx, &effective)?;
    let binding = decode(&plan.binding_json)?;
    let fence = plan.fence();
    Ok(InitializeWork {
        plan,
        effective,
        policy,
        binding,
        fence,
    })
}

/// Frozen, validated input for driver construction; never permission to execute.
/// Only the result of a fresh `arm_step` grants that permission.
pub struct InitializeWork {
    plan: Plan,
    effective: EffectiveDeployment,
    policy: ResourcePolicySnapshot,
    binding: BindingDto,
    fence: DeploymentFence,
}

impl InitializeWork {
    pub(super) fn plan(&self) -> &Plan {
        &self.plan
    }
    pub fn operation_id(&self) -> &str {
        &self.plan.operation_id
    }
    pub fn step_id(&self) -> &str {
        &self.plan.step_id
    }
    pub fn binding_id(&self) -> &str {
        &self.plan.binding_id
    }
    pub fn incarnation(&self) -> &str {
        &self.plan.incarnation
    }
    pub fn fence(&self) -> &DeploymentFence {
        &self.fence
    }
    /// ADR 0013 §5: the instance this incarnation realizes.
    pub fn instance_index(&self) -> u32 {
        self.plan.instance_index
    }
    pub fn deadline_ms(&self) -> i64 {
        self.plan.deadline_ms
    }
    pub fn effective(&self) -> &EffectiveDeployment {
        &self.effective
    }
    pub fn policy(&self) -> &ResourcePolicySnapshot {
        &self.policy
    }
    pub fn endpoint(&self) -> &str {
        &self.binding.endpoint
    }
    pub fn credential_ref(&self) -> &str {
        &self.binding.credential_ref
    }
}

impl crate::Store {
    /// The exact frozen context from this arm's full source validation. A replay
    /// never returns a context. No provenance proof survives into another command.
    pub fn arm_initialize_with_context(
        &self,
        session: &CoordinatorSession,
        step_id: &str,
        context: AdmissionContext<'_>,
    ) -> Result<(ArmResult, Option<StepExecutionContext>), LifecycleError> {
        self.arm_initialize_with_residents(session, step_id, context, &[])
    }

    /// As [`Self::arm_initialize_with_context`], crediting Ready engines on
    /// the host with the memory `residents` (sampled beside the observations)
    /// attributes to their own processes (ADR 0007).
    pub fn arm_initialize_with_residents(
        &self,
        session: &CoordinatorSession,
        step_id: &str,
        context: AdmissionContext<'_>,
        residents: &[capyctl_domain::resources::ProcessResident],
    ) -> Result<(ArmResult, Option<StepExecutionContext>), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let result = arm_with_context(&tx, session, step_id, context, residents)?;
        tx.commit()?;
        Ok(result)
    }

    /// A fenced durable observer read. It conveys no send or release authority.
    pub fn initialize_status(
        &self,
        session: &CoordinatorSession,
        step_id: &str,
        now_ms: i64,
    ) -> Result<InitializeStatus, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        if ulid::Ulid::from_string(step_id).is_err() || now_ms < 0 {
            return Err(LifecycleError::Invalid);
        }
        // This is a status observation, not catalog or execution authority.
        // Validate the bounded local relationships without recursively proving
        // the source suite on every observer poll. Arm and completion still do.
        let (raw, state, operation, binding, run_state, operation_state): (String, String, String, String, String, String) = tx.query_row(
            "SELECT s.step_json,s.state,s.operation_id,s.binding_id,r.state,o.state FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=s.operation_id JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND o.kind='initialize' AND s.session_id=?2 AND r.session_id=?2 AND r.deployment_id=s.deployment_id",
            params![step_id,session.id()],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)),
        ).optional()?.ok_or(LifecycleError::Stale)?;
        let plan: Plan = decode(&raw)?;
        if plan.version != 1
            || plan.step_id != step_id
            || plan.operation_id != operation
            || plan.binding_id != binding
            || plan.accepted_at_ms < 0
            || plan.deadline_ms <= plan.accepted_at_ms
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        if plan.session_id != session.id() {
            return Err(LifecycleError::Stale);
        }
        if state == "cancelled" {
            // An observer of old work must not inspect a successor's owners or
            // reservation as though they belonged to the cancelled operation.
            let same_terminal_fence: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM instance_runtime WHERE id=?1 AND revision=?2 AND current_generation=?3 AND desired_state='stopped') AND NOT EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1 AND instance_index=?4)",
                params![plan.deployment_id,plan.revision,plan.generation,plan.instance_index], |r| r.get(0),
            )?;
            if !same_terminal_fence {
                // Spec §6: a launch released after failing leaves its own step
                // cancelled with the gone evidence recorded, its binding released,
                // and its deployment's admission closed by the coordinator that
                // gave up on it. Its fence is unchanged, so this is the
                // deployment's own closure and not somebody else's work
                // superseding it, and an operator waiting on the start must be
                // told which of the two it is.
                let released_and_closed: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM instance_runtime WHERE id=?1 AND revision=?2 AND current_generation=?3 AND kind='model' AND desired_state='ready' AND suspended=0 AND admission_enabled=0) AND EXISTS(SELECT 1 FROM lifecycle_evidence WHERE step_id=?4) AND EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?5 AND state='released')",
                    params![plan.deployment_id,plan.revision,plan.generation,step_id,plan.binding_id],
                    |r| r.get(0),
                )?;
                return Ok(if released_and_closed {
                    InitializeStatus::Closed
                } else {
                    InitializeStatus::Superseded
                });
            }
            let (plan, effective, _) = load(&tx, step_id)?;
            expiry::terminal(&tx, session, &plan, &effective, now_ms)?;
            return Ok(InitializeStatus::ExpiredUnarmed);
        }
        match current(&tx, session, &plan, state == "completed") {
            // ADR 0011 decision 4: a deployment that gave up closes its own
            // admission. Its planned step is not superseded work, and telling a
            // waiting operator that it was superseded is wrong. Re-read without
            // the admission predicate and report the closure as itself.
            Err(LifecycleError::Stale) => {
                let closed = state == "planned"
                    && !admitting(&tx, &plan)?
                    && current_admitted(&tx, session, &plan, false, false).is_ok();
                return Ok(match (closed, now_ms >= plan.deadline_ms) {
                    // Past its deadline it is expiring work like any other, and
                    // the release path must still see it that way.
                    (true, true) => InitializeStatus::Expired,
                    (true, false) => InitializeStatus::Closed,
                    _ => InitializeStatus::Superseded,
                });
            }
            other => other?,
        }
        let consistent = match state.as_str() {
            "planned" => {
                run_state == "queued" && operation_state == "pending" && plan.execution.is_none()
            }
            "armed" => {
                run_state == "running" && operation_state == "running" && plan.execution.is_some()
            }
            "uncertain" => {
                run_state == "uncertain" && operation_state == "running" && plan.execution.is_some()
            }
            "completed" => {
                run_state == "succeeded"
                    && operation_state == "succeeded"
                    && plan.execution.is_some()
            }
            _ => false,
        };
        if !consistent {
            return Err(LifecycleError::CorruptStoredData);
        }
        for id in [
            &plan.operation_id,
            &plan.deployment_id,
            &plan.binding_id,
            &plan.incarnation,
            &plan.session_id,
        ] {
            if ulid::Ulid::from_string(id).is_err() {
                return Err(LifecycleError::CorruptStoredData);
            }
        }
        let effective = decode_effective_snapshot(&plan.effective_json)
            .map_err(|_| LifecycleError::CorruptStoredData)?;
        validate_local(&tx, &plan, &effective, &state)?;
        let associated = association(&tx, &plan)?;
        if state == "planned" && associated.is_some() {
            return Err(LifecycleError::CorruptStoredData);
        }
        if state != "completed"
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM lifecycle_evidence WHERE step_id=?1)",
                [step_id],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        let identities: String = tx.query_row(
            "SELECT identities_json FROM runtime_bindings WHERE id=?1",
            [&plan.binding_id],
            |row| row.get(0),
        )?;
        // Spec §3: a durable launcher records the API identity as soon as the
        // process exists, before the launch is associated. An armed step may
        // therefore carry exactly that one identity with no association yet; a
        // planned step has launched nothing and must carry none. Any other
        // unassociated shape is not one the launch path can write.
        if associated.is_none() {
            let recorded = decode::<Vec<IdentityDto>>(&identities)?;
            let launching = matches!(state.as_str(), "armed" | "uncertain");
            if !recorded.is_empty()
                && (!launching
                    || super::failed_launch::canonical_members_or_empty(
                        &crate::lifecycle::completion::identities(&recorded),
                    )
                    .ok()
                    .is_none_or(|members| members.len() != 1))
            {
                return Err(LifecycleError::CorruptStoredData);
            }
        }
        if state == "completed" {
            let associated = associated.ok_or(LifecycleError::CorruptStoredData)?;
            let (raw, epoch): (String, u64) = tx.query_row(
                "SELECT evidence_json,committed_epoch FROM lifecycle_evidence WHERE step_id=?1",
                [step_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let evidence: crate::lifecycle::completion::CompletionEvidenceV1 = decode(&raw)?;
            let value: serde_json::Value = decode(&raw)?;
            let observed_at_ms = value["observed_at_ms"]
                .as_i64()
                .ok_or(LifecycleError::CorruptStoredData)?;
            let receipt = value["control_receipt"]
                .as_str()
                .ok_or(LifecycleError::CorruptStoredData)?
                .to_owned();
            let context = plan.context(&effective)?;
            use capyctl_domain::completion::Milestone;
            let expected = completion_value(&CompletionEvidence {
                token: context.token,
                identities: members(&associated.identities)?,
                observed_at_ms,
                control_receipt: Some(receipt),
                milestones: vec![
                    Milestone::AllocationsRestored,
                    Milestone::WeightsUsable,
                    Milestone::CacheValid,
                    Milestone::ModelUsable,
                ],
            })?;
            let execution = plan
                .execution
                .as_ref()
                .ok_or(LifecycleError::CorruptStoredData)?;
            if evidence != expected
                || associated.observed_at_ms < execution.issued_at_ms
                || associated.observed_at_ms > plan.deadline_ms
                || observed_at_ms < execution.issued_at_ms
                || observed_at_ms > plan.deadline_ms
                || epoch <= execution.expected_epoch.saturating_add(1)
                || epoch > resource_ledger::read_snapshot(&tx).map_err(resource)?.epoch
            {
                return Err(LifecycleError::CorruptStoredData);
            }
        }
        Ok(match state.as_str() {
            "planned" if now_ms >= plan.deadline_ms => InitializeStatus::Expired,
            "planned" => InitializeStatus::Planned,
            "armed" => InitializeStatus::Armed,
            "completed" => InitializeStatus::Completed,
            "uncertain" => InitializeStatus::Uncertain,
            _ => return Err(LifecycleError::CorruptStoredData),
        })
    }

    /// Final validation of a previously fresh arm, never replay permission.
    /// The caller must also read its clock after this potentially expensive read.
    pub fn revalidate_initialize_send(
        &self,
        session: &CoordinatorSession,
        step_id: &str,
        expected: &StepExecutionContext,
        now_ms: i64,
    ) -> Result<i64, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let (raw, state): (String, String) = tx.query_row(
            "SELECT step_json,state FROM lifecycle_steps WHERE id=?1",
            [step_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let plan: Plan = decode(&raw)?;
        let effective = decode_effective_snapshot(&plan.effective_json)
            .map_err(|_| LifecycleError::CorruptStoredData)?;
        if plan.step_id != step_id || plan.version != 1 || plan.context(&effective)? != *expected {
            return Err(LifecycleError::CorruptStoredData);
        }
        validate_local(&tx, &plan, &effective, &state)?;
        current(&tx, session, &plan, false)?;
        let no_prior_effect: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND identities_json='[]') AND NOT EXISTS(SELECT 1 FROM owned_launch_associations WHERE step_id=?2) AND EXISTS(SELECT 1 FROM deployment_instances WHERE deployment_id=?3 AND instance_index=?4 AND dispatch_enabled=0)",
            params![plan.binding_id,plan.step_id,plan.deployment_id,plan.instance_index], |row| row.get(0),
        )?;
        if !no_prior_effect {
            return Err(LifecycleError::Conflict);
        }
        let execution = plan.execution.as_ref().ok_or(LifecycleError::Conflict)?;
        let policy = policy(&tx, &effective)?;
        if state != "armed"
            || now_ms < execution.issued_at_ms
            || now_ms >= plan.deadline_ms
            || policy.revision != execution.policy_revision
        {
            return Err(LifecycleError::Conflict);
        }
        Ok(policy.controls.observation_ttl_ms)
    }

    /// Read at most one current-session planned operation in durable acceptance
    /// order. Expired work is returned explicitly, not silently skipped. A worker
    /// must not arm it, and this read does not free its reserved endpoint.
    /// Superseded generations and prior sessions require explicit reconciliation.
    pub fn next_initialize(
        &self,
        session: &CoordinatorSession,
    ) -> Result<Option<InitializeWork>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let work = next_plan(&tx, session, true, &Default::default())?
            .map(|(plan, effective)| prepare_work(&tx, plan, effective))
            .transpose()?;
        tx.commit()?;
        Ok(work)
    }

    /// Durably expire every planned step whose deadline has passed, oldest
    /// deadline first, and only then look for work. The release uses the same
    /// transaction-local exact no-effect validation as an explicit expired-step
    /// retry, so nothing is released without evidence that the step never armed.
    ///
    /// Expiry is read without the admission predicate, so that a deployment which
    /// closed its own admission still reaches its deadline, and it is ordered by
    /// deadline rather than by acceptance, so that an expired step never waits
    /// behind an older step whose deadline is later. Work is then read among the
    /// deployments that are still admitting.
    // ADR 0011 decision 4: a failed deployment stops itself, not the host.
    pub fn next_initialize_or_expire(
        &self,
        session: &CoordinatorSession,
        now_ms: i64,
    ) -> Result<InitializePoll, LifecycleError> {
        self.next_initialize_or_expire_among(session, now_ms, &Default::default())
    }

    /// As `next_initialize_or_expire`, skipping every step on an instance in
    /// `busy.instances` (for work and for expiry: the effect in flight settles
    /// its own step) and every start whose binding is in `busy.held` (for work
    /// only: a held start still reaches its deadline).
    // ADR 0015: one deployment's in-flight load never hides another's start.
    pub fn next_initialize_or_expire_among(
        &self,
        session: &CoordinatorSession,
        now_ms: i64,
        busy: &super::lanes::BusyLanes,
    ) -> Result<InitializePoll, LifecycleError> {
        if now_ms < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        adopt_retired_planned(&tx, session)?;
        let mut expired = false;
        while let Some((plan, effective)) = next_expired_plan(&tx, session, now_ms, busy)? {
            expiry::expire_in_transaction(&tx, session, &plan, &effective, "planned", now_ms)?;
            expired = true;
        }
        let result = if expired {
            InitializePoll::ExpiredUnarmed
        } else {
            // SPEC §6.1 FAILED: admission closed. A closed deployment's step is
            // neither work nor expired, and it must not starve any other one.
            match next_plan(&tx, session, true, busy)? {
                None => InitializePoll::Idle,
                Some((plan, effective)) => {
                    InitializePoll::Work(Box::new(prepare_work(&tx, plan, effective)?))
                }
            }
        };
        tx.commit()?;
        Ok(result)
    }

    /// Record an uncertain effect without releasing any grant, identity, claim or
    /// endpoint. May run after the deadline. Exact retries append no second event.
    /// A stale session or superseded fence is rejected without touching newer work;
    /// the original durable arm remains sufficient for conservative recovery.
    pub fn mark_initialize_uncertain(
        &self,
        session: &CoordinatorSession,
        step_id: &str,
        now_ms: i64,
    ) -> Result<bool, LifecycleError> {
        if step_id.len() != 26 || now_ms < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let (plan, _, state) = load(&tx, step_id)?;
        current(&tx, session, &plan, false)?;
        let execution = plan.execution.as_ref().ok_or(LifecycleError::Conflict)?;
        if now_ms < execution.issued_at_ms {
            return Err(LifecycleError::Invalid);
        }
        if state == "uncertain" {
            return Ok(false);
        }
        if state != "armed" {
            return Err(LifecycleError::Conflict);
        }
        one(tx.execute("UPDATE lifecycle_steps SET state='uncertain' WHERE id=?1 AND session_id=?2 AND state='armed'", params![step_id,session.id()])?)?;
        one(tx.execute("UPDATE lifecycle_runs SET state='uncertain' WHERE operation_id=?1 AND session_id=?2 AND state='running'", params![plan.operation_id,session.id()])?)?;
        one(tx.execute("UPDATE deployment_instances SET dispatch_enabled=0 WHERE deployment_id=?1 AND revision=?2 AND generation=?3", params![plan.deployment_id,plan.revision,plan.generation])?)?;
        event(&tx, session, &plan, Transition::Uncertain, None)?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod retired_planned_tests {
    use super::*;
    use crate::Store;
    use capyctl_config::effective::resolve_effective;
    use capyctl_domain::resources::MemoryObservation;
    use serde_json::{json, Value};

    /// A deployment whose Start was accepted by `old` and never armed.
    fn planned_by_retired_session() -> (Store, CoordinatorSession, DeploymentFence, String) {
        let value: Value = serde_json::from_str(include_str!(
            "../../../capyctl-config/tests/fixtures/f2-deployment.json"
        ))
        .unwrap();
        let (config, host) = (value["deployment"].clone(), value["host"].clone());
        let effective = resolve_effective(&config, &host).unwrap();
        let store = Store::open_in_memory().unwrap();
        let old = store.begin_coordinator_session().unwrap();
        let observations = vec![MemoryObservation {
            domain: "unified".into(),
            capacity_bytes: 64 << 30,
            available_bytes: 60 << 30,
            sampled_at_ms: 1,
        }];
        store
            .import_resource_policy(&old, &effective.host, &observations, 1)
            .unwrap();
        let body = json!({"config": config}).to_string();
        let receipt = store
            .create_stopped_managed_configuration(&old, "principal", "key", &body, &host, 10)
            .unwrap();
        let fence = DeploymentFence {
            deployment_id: receipt.deployment_id.clone(),
            revision: receipt.revision,
            generation: receipt.generation,
        };
        let accepted = store.accept_start(&old, &fence, 100, 100_100).unwrap();
        (store, old, fence, accepted.step_id)
    }

    fn step_session(store: &Store, step: &str) -> String {
        store
            .conn
            .query_row(
                "SELECT session_id FROM lifecycle_steps WHERE id=?1",
                [step],
                |r| r.get(0),
            )
            .unwrap()
    }

    // T33 T34 T09: found live (Phase B). A Start the retired controller accepted
    // but never armed is adopted by the restarted one and driven within its
    // original deadline, instead of blocking the deployment for ever.
    #[test]
    fn a_restarted_controller_adopts_a_start_that_never_armed() {
        let (store, old, fence, step) = planned_by_retired_session();
        let session = store.begin_coordinator_session().unwrap();
        // Before discovery the retired session's claim refuses a new Start.
        assert!(matches!(
            store.accept_start(&session, &fence, 200, 100_100),
            Err(LifecycleError::Stale)
        ));
        match store.next_initialize_or_expire(&session, 200).unwrap() {
            InitializePoll::Work(work) => assert_eq!(work.step_id(), step),
            _ => panic!("the adopted start must be discovered as work"),
        }
        assert_eq!(step_session(&store, &step), session.id());
        assert_ne!(old.id(), session.id());
        // Adoption granted nothing: the ledger holds no owner for it.
        assert!(!store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id));
        // The same Start is joined now, not refused.
        assert_eq!(
            store
                .accept_start(&session, &fence, 300, 100_100)
                .unwrap()
                .step_id,
            step
        );
    }

    // T30 T33: a retired start past its deadline (a deployment that gave up
    // before the restart) is expired by the restarted controller, not orphaned.
    #[test]
    fn a_retired_start_past_its_deadline_is_expired_after_adoption() {
        let (store, _old, fence, step) = planned_by_retired_session();
        store
            .conn
            .execute(
                "UPDATE deployments SET admission_enabled=0 WHERE id=?1",
                [&fence.deployment_id],
            )
            .unwrap();
        let session = store.begin_coordinator_session().unwrap();
        assert!(matches!(
            store.next_initialize_or_expire(&session, 100_200).unwrap(),
            InitializePoll::ExpiredUnarmed
        ));
        let state: String = store
            .conn
            .query_row(
                "SELECT state FROM lifecycle_steps WHERE id=?1",
                [&step],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "cancelled");
        let claims: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM lifecycle_claims WHERE deployment_id=?1",
                [&fence.deployment_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claims, 0);
    }
}

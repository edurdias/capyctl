//! Bounded discovery and conservative failure recording for the owned worker.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualifiedInitializeStatus {
    Planned,
    Armed,
    Completed,
    Uncertain,
    Superseded,
    Expired,
    ExpiredUnarmed,
}

/// One bounded discovery result. Expiry is committed before this is returned;
/// work still requires a fresh arm before any execution.
pub enum QualifiedInitializePoll {
    Idle,
    ExpiredUnarmed,
    Work(Box<QualifiedInitializeWork>),
}

fn next_plan(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
) -> Result<Option<(Plan, EffectiveDeployment)>, LifecycleError> {
    let id: Option<String> = tx
        .query_row(
            "SELECT s.id FROM lifecycle_steps s
         JOIN operations o ON o.id=s.operation_id
         JOIN lifecycle_runs r ON r.operation_id=o.id
         JOIN deployments d ON d.id=s.deployment_id
         WHERE o.kind='qualified_initialize' AND s.state='planned'
           AND s.session_id=?1 AND r.session_id=?1
           AND d.revision=r.revision AND d.current_generation=r.generation
           AND d.desired_state='ready' AND d.suspended=0
         ORDER BY o.accepted_at,o.id LIMIT 1",
            [session.id()],
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
    current(tx, session, &plan, false)?;
    if state != "planned" {
        return Err(LifecycleError::Conflict);
    }
    Ok(Some((plan, effective)))
}

fn prepare_work(
    tx: &Transaction<'_>,
    plan: Plan,
    effective: EffectiveDeployment,
) -> Result<QualifiedInitializeWork, LifecycleError> {
    let policy = policy(tx, &effective)?;
    let binding = decode(&plan.binding_json)?;
    let fence = plan.fence();
    Ok(QualifiedInitializeWork {
        plan,
        effective,
        policy,
        binding,
        fence,
    })
}

/// Frozen, validated input for driver construction; never permission to execute.
/// Only the result of a fresh `arm_step` grants that permission.
pub struct QualifiedInitializeWork {
    plan: Plan,
    effective: EffectiveDeployment,
    policy: ResourcePolicySnapshot,
    binding: BindingDto,
    fence: DeploymentFence,
}

impl QualifiedInitializeWork {
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
    pub fn arm_qualified_initialize_with_context(
        &self,
        session: &CoordinatorSession,
        step_id: &str,
        context: AdmissionContext<'_>,
    ) -> Result<(ArmResult, Option<StepExecutionContext>), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let result = arm_with_context(&tx, session, step_id, context)?;
        tx.commit()?;
        Ok(result)
    }

    /// A fenced durable observer read. It conveys no send or release authority.
    pub fn qualified_initialize_status(
        &self,
        session: &CoordinatorSession,
        step_id: &str,
        now_ms: i64,
    ) -> Result<QualifiedInitializeStatus, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        if ulid::Ulid::from_string(step_id).is_err() || now_ms < 0 {
            return Err(LifecycleError::Invalid);
        }
        // This is a status observation, not catalog or execution authority.
        // Validate the bounded local relationships without recursively proving
        // the source suite on every observer poll. Arm and completion still do.
        let (raw, state, operation, binding, run_state, operation_state): (String, String, String, String, String, String) = tx.query_row(
            "SELECT s.step_json,s.state,s.operation_id,s.binding_id,r.state,o.state FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=s.operation_id JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND o.kind='qualified_initialize' AND s.session_id=?2 AND r.session_id=?2 AND r.deployment_id=s.deployment_id",
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
                "SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND revision=?2 AND current_generation=?3 AND desired_state='stopped') AND NOT EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1)",
                params![plan.deployment_id,plan.revision,plan.generation], |r| r.get(0),
            )?;
            if !same_terminal_fence {
                return Ok(QualifiedInitializeStatus::Superseded);
            }
            let (plan, effective, _) = load(&tx, step_id)?;
            expiry::terminal(&tx, session, &plan, &effective, now_ms)?;
            return Ok(QualifiedInitializeStatus::ExpiredUnarmed);
        }
        match current(&tx, session, &plan, state == "completed") {
            Err(LifecycleError::Stale) => return Ok(QualifiedInitializeStatus::Superseded),
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
        if associated.is_none() && !decode::<Vec<IdentityDto>>(&identities)?.is_empty() {
            return Err(LifecycleError::CorruptStoredData);
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
            use mllm_domain::completion::Milestone;
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
            "planned" if now_ms >= plan.deadline_ms => QualifiedInitializeStatus::Expired,
            "planned" => QualifiedInitializeStatus::Planned,
            "armed" => QualifiedInitializeStatus::Armed,
            "completed" => QualifiedInitializeStatus::Completed,
            "uncertain" => QualifiedInitializeStatus::Uncertain,
            _ => return Err(LifecycleError::CorruptStoredData),
        })
    }

    /// Final validation of a previously fresh arm, never replay permission.
    /// The caller must also read its clock after this potentially expensive read.
    pub fn revalidate_qualified_initialize_send(
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
            "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND identities_json='[]') AND NOT EXISTS(SELECT 1 FROM owned_launch_associations WHERE step_id=?2) AND EXISTS(SELECT 1 FROM deployments WHERE id=?3 AND dispatch_enabled=0)",
            params![plan.binding_id,plan.step_id,plan.deployment_id], |row| row.get(0),
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
    pub fn next_qualified_initialize(
        &self,
        session: &CoordinatorSession,
    ) -> Result<Option<QualifiedInitializeWork>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let work = next_plan(&tx, session)?
            .map(|(plan, effective)| prepare_work(&tx, plan, effective))
            .transpose()?;
        tx.commit()?;
        Ok(work)
    }

    /// Inspect one oldest current planned operation and durably expire it before
    /// checking current launch policy. The release uses the same transaction-local
    /// exact no-effect validation as an explicit expired-step retry.
    pub fn next_qualified_initialize_or_expire(
        &self,
        session: &CoordinatorSession,
        now_ms: i64,
    ) -> Result<QualifiedInitializePoll, LifecycleError> {
        if now_ms < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let result = match next_plan(&tx, session)? {
            None => QualifiedInitializePoll::Idle,
            Some((plan, effective)) if now_ms >= plan.deadline_ms => {
                expiry::expire_in_transaction(&tx, session, &plan, &effective, "planned", now_ms)?;
                QualifiedInitializePoll::ExpiredUnarmed
            }
            Some((plan, effective)) => {
                QualifiedInitializePoll::Work(Box::new(prepare_work(&tx, plan, effective)?))
            }
        };
        tx.commit()?;
        Ok(result)
    }

    /// Record an uncertain effect without releasing any grant, identity, claim or
    /// endpoint. May run after the deadline. Exact retries append no second event.
    /// A stale session or superseded fence is rejected without touching newer work;
    /// the original durable arm remains sufficient for conservative recovery.
    pub fn mark_qualified_initialize_uncertain(
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
        one(tx.execute("UPDATE deployments SET dispatch_enabled=0 WHERE id=?1 AND revision=?2 AND current_generation=?3", params![plan.deployment_id,plan.revision,plan.generation])?)?;
        event(&tx, session, &plan, Transition::Uncertain, None)?;
        tx.commit()?;
        Ok(true)
    }
}

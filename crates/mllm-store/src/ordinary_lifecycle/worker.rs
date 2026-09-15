//! Bounded discovery and conservative failure recording for the owned worker.
use super::*;

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
        let (plan, effective, state) = load(&tx, &id)?;
        current(&tx, session, &plan, false)?;
        if state != "planned" {
            return Err(LifecycleError::Conflict);
        }
        let policy = policy(&tx, &effective)?;
        let binding = decode(&plan.binding_json)?;
        let fence = plan.fence();
        tx.commit()?;
        Ok(Some(QualifiedInitializeWork {
            plan,
            effective,
            policy,
            binding,
            fence,
        }))
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

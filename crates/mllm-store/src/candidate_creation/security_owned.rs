//! Discovery and exact pre-send fences for the application-owned Security worker.
use super::*;

impl crate::Store {
    /// One bounded current-session source, only after durable corpus results.
    /// Discovery does not arm a child or authorize replay of existing work.
    pub fn next_candidate_security(
        &self,
        session: &CoordinatorSession,
        now: i64,
    ) -> Result<Option<CandidateInferenceWork>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let raw = tx.query_row(
            "SELECT r.plan_json FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id
             WHERE o.kind='candidate_action_v3' AND r.session_id=?1 AND r.state='succeeded'
             AND CASE
               WHEN typeof(r.plan_json)!='text' OR length(CAST(r.plan_json AS BLOB))>?2 OR NOT json_valid(r.plan_json) THEN 1
               WHEN json_extract(r.plan_json,'$.version') IS NOT 3 OR json_extract(r.plan_json,'$.action') IS NULL OR json_extract(r.plan_json,'$.action') NOT IN ('initialize','security','park','restore') THEN 1
               ELSE json_extract(r.plan_json,'$.action')='initialize'
                 AND (SELECT count(*) FROM qualification_request_attempts a JOIN qualification_request_results z ON z.request_operation_id=a.request_operation_id
                      WHERE a.run_id=json_extract(r.plan_json,'$.scope.run_id') AND a.subcheck_id='' AND a.case_id<>json_extract(r.plan_json,'$.ready_probe_case'))>=4
                 AND NOT EXISTS(SELECT 1 FROM qualification_case_actions a JOIN lifecycle_steps s ON s.id=a.step_id
                     WHERE a.run_id=json_extract(r.plan_json,'$.scope.run_id') AND a.case_id<>json_extract(r.plan_json,'$.case_id') AND s.state='completed'
                     AND CASE WHEN typeof(s.step_json)='text' AND length(CAST(s.step_json AS BLOB))<=?2 AND json_valid(s.step_json)
                         THEN json_extract(s.step_json,'$.plan.action')='security' ELSE 0 END)
             END ORDER BY o.rowid LIMIT 1",
            params![session.id(), super::super::super::super::MAX_BYTES as i64],
            |r| Ok(worker::bounded_text(r, 0, super::super::super::super::MAX_BYTES)),
        ).optional()?.transpose()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let p: CandidateActionPlanV3 = decode(&raw)?;
        validate_plan(&tx, &p)?;
        markers::baseline(&tx, &p)?;
        let existing = tx.query_row(
            "SELECT a.step_id,s.step_json,r.plan_json FROM qualification_case_actions a
             LEFT JOIN lifecycle_steps s ON s.id=a.step_id LEFT JOIN lifecycle_runs r ON r.operation_id=a.operation_id
             WHERE a.run_id=?1 AND a.case_id=?2",
            params![p.scope.run_id,security_case(&tx,&p)?],
            |r| Ok((worker::bounded_text(r,0,26),worker::bounded_text(r,1,super::super::super::super::MAX_BYTES),worker::bounded_text(r,2,super::super::super::super::MAX_BYTES))),
        ).optional()?;
        if let Some((step, anchor, plan)) = existing {
            let anchor: ArmedActionV3 = decode(&anchor?)?;
            let security: CandidateActionPlanV3 = decode(&plan?)?;
            if step? != security.scope.parent_step_id
                || anchor.plan != security
                || security.action != Action::Security
                || security.scope.run_id != p.scope.run_id
            {
                return Err(LifecycleError::CorruptStoredData);
            }
            validate_plan(&tx, &security)?;
        }
        let eligible: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM deployments d JOIN runtime_bindings b ON b.deployment_id=d.id JOIN qualification_runs q ON q.id=?1
             WHERE d.id=?2 AND d.revision=?3 AND d.current_generation=?4 AND d.desired_state='stopped' AND d.observed_state='ready' AND d.admission_enabled=0 AND d.dispatch_enabled=0
             AND b.id=?5 AND b.incarnation=?6 AND b.revision=?3 AND b.ownership='managed' AND b.state='live'
             AND q.state='running' AND q.principal_id=?7 AND q.host_id=?8)",
            params![p.scope.run_id,p.scope.deployment_id,p.scope.revision,p.scope.generation,p.scope.binding_id,p.scope.incarnation,p.scope.principal,p.scope.host],
            |r|r.get(0),
        )?;
        let qualification = read_candidate_policy(&tx, &p.scope.host)
            .map_err(super::super::super::super::map_qualification)
            .map_err(creation_error)?
            .ok_or(LifecycleError::Conflict)?;
        if !eligible
            || qualification.revision != p.scope.qualification_policy_revision
            || p.scope.session_id != session.id()
        {
            return Err(LifecycleError::Stale);
        }
        let principal = p.scope.principal.clone();
        let run = p.scope.run_id.clone();
        let revision = p.scope.revision;
        drop(tx);
        self.candidate_inference_work(session, &principal, &run, revision, now)
            .map(Some)
    }

    /// This only validates a caller-held New result; it never recovers send authority.
    pub fn revalidate_candidate_security_send(
        &self,
        session: &CoordinatorSession,
        work: &CandidateInferenceWork,
        dispatch: &CandidateSecurityDispatch,
        admission: AdmissionContext<'_>,
    ) -> Result<(), LifecycleError> {
        let (context, request) = match dispatch {
            CandidateSecurityDispatch::NewControl(d) => (d.context(), None),
            CandidateSecurityDispatch::NewRequest(d) => (d.context(), Some(d.as_ref())),
            CandidateSecurityDispatch::AlreadyRecorded { .. } => return Err(LifecycleError::Stale),
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let p = plan_for_step(&tx, &context.token.step_id)?;
        validate_plan(&tx, &p)?;
        if p.action != Action::Security
            || work.principal != p.scope.principal
            || work.run_id != p.scope.run_id
            || work.binding_id != p.scope.binding_id
            || work.incarnation != p.scope.incarnation
            || work.host_id != p.scope.host
            || work.revision != p.scope.revision
            || work.policy.revision != p.scope.resource_policy_revision
            || work.deadline_ms != p.deadline_ms
        {
            return Err(LifecycleError::Stale);
        }
        let index = p
            .effects
            .iter()
            .position(|e| e.step_id == context.token.step_id)
            .ok_or(LifecycleError::Stale)?;
        let (state, raw) = tx.query_row(
            "SELECT state,step_json FROM lifecycle_steps WHERE id=?1",
            [&context.token.step_id],
            |r| {
                Ok((
                    worker::bounded_text(r, 0, 32),
                    worker::bounded_text(r, 1, super::super::super::super::MAX_BYTES),
                ))
            },
        )?;
        let armed: ArmedEffectV3 = decode(&raw?)?;
        if state? != "armed"
            || *context != execution(&tx, &p, index, armed.issued_at_ms)?
            || admission.now_ms < context.issued_at_ms
            || admission.now_ms >= context.deadline_ms
        {
            return Err(LifecycleError::Stale);
        }
        let lease = if let Some(d) = request {
            let a = attempt_for(&tx, &p, index)?.ok_or(LifecycleError::Stale)?;
            if d.request_operation_id != a.request_operation_id
                || d.request != probe_request(&p)?
                || d.security_endpoint != Some(endpoint(index))
                || d.ticket
                    != DispatchTicket::candidate(
                        a.lease_id.clone(),
                        p.scope.deployment_id.clone(),
                        p.scope.revision,
                        p.scope.generation,
                        p.scope.session_id.clone(),
                    )
            {
                return Err(LifecycleError::Stale);
            }
            Some(a.lease_id)
        } else {
            if index != 0 {
                return Err(LifecycleError::Stale);
            }
            None
        };
        current_security_with_lease(&tx, session, &p, admission, lease.as_deref())
    }
}

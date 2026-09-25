//! SPEC §10, ADR 0013 §8 (W10): the coordinator's commands for request-driven
//! switching. Each is one store transaction under the owned session; the
//! sequence that drives them lives in `crate::switching`.
use super::*;
use mllm_store::ordinary_lifecycle::switching::{
    StartSwitchPlan, SwitchPlan, SwitchRecord, SwitchRelease,
};
use std::collections::BTreeSet;

impl CoordinatorCommands {
    fn owned<T>(
        &self,
        run: impl FnOnce(&crate::ownership::OwnedCoordinatorState) -> Result<T, LifecycleError>,
    ) -> Result<T, CoordinatorCommandError> {
        let owner = self.shared.owner.lock().map_err(|error| {
            drop(error);
            self.shared.fail("ownership mutex poisoned")
        })?;
        run(&owner).map_err(|error| {
            if matches!(
                error,
                LifecycleError::Sql(_) | LifecycleError::CorruptStoredData
            ) {
                self.shared.fail_locked(&owner, error.to_string());
            }
            CoordinatorCommandError::Lifecycle(error)
        })
    }

    /// ADR 0013 §8 rules 1–4: plan the release that lets one instance of
    /// `target` activate, against the hosts eligible now and the router's
    /// recent activity. `protected` deployments serve a waiting group.
    pub fn plan_switch(
        &self,
        target: &str,
        only: Option<u32>,
        explicit: bool,
        protected: &BTreeSet<String>,
    ) -> Result<SwitchPlan, CoordinatorCommandError> {
        // Read before the owner lock: the source keeps its own state.
        let eligible = self.shared.observations.eligible_hosts();
        let activity = self
            .shared
            .activity
            .lock()
            .map_err(|_| self.shared.fail("activity registry poisoned"))?
            .clone();
        let last = |deployment: &str, generation: i64| {
            [generation, -1]
                .iter()
                .filter_map(|g| activity.get(&(deployment.to_owned(), *g)).copied())
                .max()
        };
        self.owned(|owner| {
            owner
                .store()
                .plan_switch(owner.session(), target, only, explicit, eligible.as_ref(), protected, &last)
        })
    }

    /// Owner decision 2026-09-25 (`start deployment --evict`): plan the
    /// releases every instance of `target` the start activates needs, against
    /// the hosts eligible now and the router's recent activity.
    pub fn plan_start_switch(
        &self,
        target: &str,
        protected: &BTreeSet<String>,
    ) -> Result<StartSwitchPlan, CoordinatorCommandError> {
        let eligible = self.shared.observations.eligible_hosts();
        let activity = self
            .shared
            .activity
            .lock()
            .map_err(|_| self.shared.fail("activity registry poisoned"))?
            .clone();
        let last = |deployment: &str, generation: i64| {
            [generation, -1]
                .iter()
                .filter_map(|g| activity.get(&(deployment.to_owned(), *g)).copied())
                .max()
        };
        self.owned(|owner| {
            owner.store().plan_start_switch(
                owner.session(),
                target,
                eligible.as_ref(),
                protected,
                &last,
            )
        })
    }

    /// Owner decision 2026-09-25: the hosts eligible for placement now
    /// (`None`: every resolving host is a candidate) and why each other host
    /// with a session is not. Read without the owner lock held.
    pub fn eligibility(
        &self,
    ) -> (
        Option<BTreeSet<String>>,
        std::collections::BTreeMap<String, String>,
    ) {
        (
            self.shared.observations.eligible_hosts(),
            self.shared.observations.ineligible_hosts(),
        )
    }

    /// When the router last sent this deployment's instance a request, on
    /// the coordinator's clock (SPEC §6.5, §10 fairness).
    pub fn last_activity(&self, deployment: &str, generation: i64) -> Option<i64> {
        let activity = self.shared.activity.lock().ok()?;
        [generation, -1]
            .iter()
            .filter_map(|g| activity.get(&(deployment.to_owned(), *g)).copied())
            .max()
    }

    /// SPEC §10 step 3: close one victim's dispatch gate.
    pub fn close_for_switch(
        &self,
        deployment: &str,
        instance: u32,
        generation: i64,
    ) -> Result<bool, CoordinatorCommandError> {
        self.owned(|owner| {
            owner
                .store()
                .close_for_switch(owner.session(), deployment, instance, generation)
        })
    }

    /// SPEC §10: a failed switch reopens a victim it closed but never released.
    pub fn reopen_after_switch(
        &self,
        deployment: &str,
        instance: u32,
        generation: i64,
    ) -> Result<bool, CoordinatorCommandError> {
        self.owned(|owner| {
            owner
                .store()
                .reopen_after_switch(owner.session(), deployment, instance, generation)
        })
    }

    /// SPEC §10 step 4: leases still charged to one victim incarnation.
    pub fn switch_outstanding_leases(
        &self,
        deployment: &str,
        instance: u32,
        generation: i64,
    ) -> Result<usize, CoordinatorCommandError> {
        self.owned(|owner| {
            owner
                .store()
                .switch_outstanding_leases(deployment, instance, generation)
        })
    }

    /// SPEC §10 step 5: park or stop one drained victim; `may_park` is the
    /// plan's decision (false for a solo first start's victims, which stop).
    /// Refused while the worker admits no cleanup (shutdown).
    pub fn accept_switch_release(
        &self,
        principal: &str,
        deployment: &str,
        instance: u32,
        generation: i64,
        key: &str,
        may_park: bool,
    ) -> Result<SwitchRelease, CoordinatorCommandError> {
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(CoordinatorError::Stopped("worker is not admitting cleanup".into()).into());
        }
        let now = (self.shared.clock)()?;
        let release = self.owned(|owner| {
            owner.store().accept_switch_release(
                owner.session(),
                principal,
                deployment,
                instance,
                generation,
                key,
                now,
                may_park,
            )
        })?;
        self.shared.wake.notify_one();
        Ok(release)
    }

    /// SPEC §6.1: what a request for `deployment` finds right now.
    pub fn request_view(
        &self,
        deployment: &str,
    ) -> Result<mllm_store::ordinary_lifecycle::switching::RequestView, CoordinatorCommandError>
    {
        self.owned(|owner| owner.store().switch_request_view(deployment))
    }

    /// W10 gap (c): forget a switch that ended without a terminal record.
    pub fn end_switch(&self, switch_id: &str) -> Result<(), CoordinatorCommandError> {
        self.owned(|owner| owner.store().end_switch(owner.session(), switch_id))
    }

    /// Journal one switch transition as a management event.
    pub fn record_switch(&self, record: &SwitchRecord<'_>) -> Result<(), CoordinatorCommandError> {
        self.owned(|owner| owner.store().record_switch(owner.session(), record))
    }
}

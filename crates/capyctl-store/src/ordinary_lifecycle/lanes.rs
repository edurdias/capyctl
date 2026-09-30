//! ADR 0015: discovery that skips the instances the coordinator is already
//! driving.
//!
//! The coordinator runs lifecycle effects for different instances at the same
//! time, but never two for one instance. Discovery is therefore told which
//! instances have an effect in flight, and which bindings' Initialize is held
//! back (a retry cooldown or a deferral), so that it returns the next step that
//! can actually be driven instead of the same busy one again. Nothing here arms,
//! releases or reserves anything: every transaction that does keeps its own
//! checks, and a busy set that is stale or wrong can only delay work, never
//! authorize it.
use super::*;
use std::collections::BTreeSet;

/// An instance as `(deployment_id, instance_index)`: the lane the coordinator
/// serializes one instance's lifecycle effects on.
pub type InstanceLane = (String, u32);

/// Work the coordinator has in flight or holds back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusyLanes {
    /// Instances, as `(deployment_id, instance_index)`, with a lifecycle effect
    /// in flight. No step of theirs is discovered or expired by discovery; the
    /// task that drives the effect settles its own step.
    pub instances: BTreeSet<InstanceLane>,
    /// Bindings whose planned Initialize is held back until a later poll. Only
    /// Initialize discovery honours this; expiry, stops and cleanups do not, so
    /// a held start can still be stopped or reach its deadline.
    pub held: BTreeSet<String>,
}

impl BusyLanes {
    pub(crate) fn instances_json(&self) -> Result<String, LifecycleError> {
        let lanes: Vec<String> = self
            .instances
            .iter()
            .map(|(deployment, instance)| format!("{deployment}#{instance}"))
            .collect();
        serde_json::to_string(&lanes).map_err(|_| LifecycleError::Invalid)
    }

    pub(crate) fn held_json(&self) -> Result<String, LifecycleError> {
        serde_json::to_string(&self.held).map_err(|_| LifecycleError::Invalid)
    }
}

/// SQL true when the binding of step alias `s` lies on no busy instance; `?N`
/// binds `BusyLanes::instances_json`. A step whose binding row is missing is
/// not excluded here, so the step's own validation still reports it.
pub(crate) fn lane_free(param: usize) -> String {
    format!(
        "NOT EXISTS(SELECT 1 FROM runtime_bindings lb WHERE lb.id=s.binding_id \
         AND (lb.deployment_id||'#'||lb.instance_index) IN (SELECT value FROM json_each(?{param})))"
    )
}

impl crate::Store {
    /// ADR 0015: the instance a binding realizes, which is the lane the
    /// coordinator serializes its lifecycle effects on. `None` when the binding
    /// is unknown.
    pub fn binding_lane(&self, binding_id: &str) -> Result<Option<InstanceLane>, LifecycleError> {
        let lane: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT deployment_id,instance_index FROM runtime_bindings WHERE id=?1",
                [binding_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        lane.map(|(deployment, instance)| {
            u32::try_from(instance)
                .map(|instance| (deployment, instance))
                .map_err(|_| LifecycleError::CorruptStoredData)
        })
        .transpose()
    }
}

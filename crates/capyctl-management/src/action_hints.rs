//! ADR 0040 §8: the snapshot's supported-action hints carry, for each refusal
//! the state alone decides, the code the action's handler answers it with.
use capyctl_store::snapshot::{ActionRefusal, HintedAction, Snapshot};

/// Fill in every hint's `reason` from its typed refusal.
pub(crate) fn map_reasons(snapshot: &mut Snapshot) {
    for deployment in &mut snapshot.deployments {
        let instances = deployment.instances.iter_mut();
        deployment
            .actions
            .iter_mut()
            .chain(instances.flat_map(|instance| instance.actions.iter_mut()))
            .for_each(|hint| hint.reason = hint.refusal.map(|r| code(hint.action, r)));
    }
}

/// The handlers' mapping of the same store refusals: `actions::command_failure`
/// (`Conflict` to `LifecycleConflict`, `RuntimeRetained` to `RuntimeRetained`,
/// `Stale` to `ReconciliationRequired`), except that a delete answers
/// `RuntimeRetained` as `DeleteRequiresCleanup`; each code as
/// `ConfigurationFailure::response` writes it.
fn code(action: HintedAction, refusal: ActionRefusal) -> &'static str {
    match (action, refusal) {
        (HintedAction::Delete, ActionRefusal::RuntimeRetained) => "delete_requires_cleanup",
        (_, ActionRefusal::RuntimeRetained) => "runtime_retained",
        (_, ActionRefusal::Conflict) => "lifecycle_conflict",
        (_, ActionRefusal::Stale) => "reconciliation_required",
    }
}

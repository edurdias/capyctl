use mllm_domain::resources::*;

use crate::residency::{admit_phase, validate_admission_context, AdmissionContext, ResourceError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForecastStep {
    pub owner: String,
    pub footprint: PhaseFootprint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForecastAction {
    Phase(ForecastStep),
    RemoveAfterVerifiedCleanup { owner: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceFailure {
    pub step: usize,
    pub reason: ResourceError,
}

/// Forecasts steps by copying inputs and updating synthetic accounting only.
///
/// Forecast state is not measured evidence and does not authorize engine actions.
/// The coordinator must reobserve and revalidate resources before each real step.
pub fn forecast_sequence(
    initial: &LedgerSnapshot,
    steps: &[ForecastStep],
    context: AdmissionContext<'_>,
) -> Result<LedgerSnapshot, SequenceFailure> {
    forecast_actions(
        initial,
        &steps
            .iter()
            .cloned()
            .map(ForecastAction::Phase)
            .collect::<Vec<_>>(),
        context,
    )
}

/// Replays phase changes and conditional cleanup using synthetic resident evidence.
/// Cleanup is only a forecast: real release still requires verified owned cleanup.
pub fn forecast_actions(
    initial: &LedgerSnapshot,
    actions: &[ForecastAction],
    context: AdmissionContext<'_>,
) -> Result<LedgerSnapshot, SequenceFailure> {
    let mut state = initial.clone();
    let mut forecast = context.observations.to_vec();
    let mut floors = context.resident_floors.to_vec();
    for (index, action) in actions.iter().enumerate() {
        let fail = |reason| SequenceFailure {
            step: index,
            reason,
        };
        let current_context = AdmissionContext {
            observations: &forecast,
            resident_floors: &floors,
            ..context
        };
        let step = match action {
            ForecastAction::Phase(step) => step,
            ForecastAction::RemoveAfterVerifiedCleanup { owner } => {
                validate_admission_context(&state, current_context).map_err(fail)?;
                if !state.owners.contains_key(owner) {
                    return Err(fail(ResourceError::Invalid));
                }
                for observation in &mut forecast {
                    let credit = floors
                        .iter()
                        .find(|f| f.owner == *owner && f.domain == observation.domain)
                        .map(|f| f.bytes)
                        .unwrap_or(0);
                    observation.available_bytes = observation
                        .available_bytes
                        .checked_add(credit)
                        .ok_or_else(|| fail(ResourceError::Invalid))?;
                }
                floors.retain(|f| f.owner != *owner);
                state.owners.remove(owner);
                continue;
            }
        };
        admit_phase(&state, &step.owner, &step.footprint, current_context).map_err(fail)?;
        for observation in &mut forecast {
            let get = |f: &PhaseFootprint| {
                f.allocations
                    .iter()
                    .find(|a| a.domain == observation.domain)
                    .map(|a| a.bytes)
                    .unwrap_or(0)
            };
            let before = floors
                .iter()
                .find(|f| f.owner == step.owner && f.domain == observation.domain)
                .map(|f| f.bytes)
                .unwrap_or(0);
            let after = get(&step.footprint);
            observation.available_bytes = observation
                .available_bytes
                .checked_add(before)
                .and_then(|x| x.checked_sub(after))
                .ok_or_else(|| fail(ResourceError::Invalid))?
                .min(observation.capacity_bytes);
            floors.retain(|f| !(f.owner == step.owner && f.domain == observation.domain));
            floors.push(ResidentFloor {
                owner: step.owner.clone(),
                domain: observation.domain.clone(),
                bytes: after,
                sampled_at_ms: observation.sampled_at_ms,
            });
        }
        state
            .owners
            .insert(step.owner.clone(), step.footprint.clone());
    }
    Ok(state)
}

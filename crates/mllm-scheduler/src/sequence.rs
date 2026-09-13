use mllm_domain::resources::*;

use crate::residency::{admit_phase, AdmissionContext, ResourceError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForecastStep {
    pub owner: String,
    pub footprint: PhaseFootprint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceFailure {
    pub step: usize,
    pub reason: ResourceError,
}

pub fn forecast_sequence(
    initial: &LedgerSnapshot,
    steps: &[ForecastStep],
    context: AdmissionContext<'_>,
) -> Result<LedgerSnapshot, SequenceFailure> {
    let mut state = initial.clone();
    let mut forecast = context.observations.to_vec();
    let mut floors = context.resident_floors.to_vec();
    for (index, step) in steps.iter().enumerate() {
        let fail = |reason| SequenceFailure {
            step: index,
            reason,
        };
        admit_phase(
            &state,
            &step.owner,
            &step.footprint,
            AdmissionContext {
                observations: &forecast,
                resident_floors: &floors,
                ..context
            },
        )
        .map_err(fail)?;
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

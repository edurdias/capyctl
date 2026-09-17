//! Turning a frozen binding into the adapter spec its engine family needs.
//!
//! The coordinator deliberately knows nothing about credentials or launch plans, so
//! this supplies them. It reads the binding's own frozen profile rather than any
//! ambient configuration: the profile is what the deployment was admitted against,
//! and reading anything else would let a runtime drift from what was qualified.
//!
//! Only families that can be built honestly are built. A family whose production
//! prerequisites are missing is refused by name, because constructing it with
//! plausible placeholders would produce a runtime that looks configured and is not.

use std::sync::Arc;

use mllm_adapters::resolve::AdapterSpec;
use mllm_config::engine_policy::Engine;
use mllm_store::ordinary_lifecycle::worker::InitializeWork;

use crate::coordinator::{CoordinatorError, EngineBindings, ServiceClock};

/// Builds adapter specs from frozen bindings.
pub struct ProfileBindings {
    clock: ServiceClock,
}

impl ProfileBindings {
    /// The clock is the service's own. The Fake family stamps the milestones it
    /// observes with it, so evidence is dated by the authority that reads it back
    /// rather than by whatever the adapter could reach for itself.
    pub fn new(clock: ServiceClock) -> Self {
        Self { clock }
    }

    fn missing(family: &str, what: &str) -> CoordinatorError {
        CoordinatorError::Service(format!(
            "cannot build a {family} runtime yet: {what}. Refusing rather than \
             constructing one that looks configured and is not"
        ))
    }
}

impl EngineBindings for ProfileBindings {
    fn spec(&self, work: &InitializeWork) -> Result<AdapterSpec, CoordinatorError> {
        let effective = work.effective();
        let profile = &effective.profile;
        match profile.engine {
            Engine::Vllm => {
                let endpoint = work.endpoint().parse().map_err(|_| {
                    CoordinatorError::Service(format!(
                        "frozen binding endpoint is not a URL: {}",
                        work.endpoint()
                    ))
                })?;
                Ok(AdapterSpec::Vllm {
                    endpoint,
                    // The engine's own listener is unauthenticated on this path, which
                    // is what production does today. A credential reference exists on
                    // the profile but nothing resolves it yet, and inventing a secret
                    // here would authenticate against a listener expecting none.
                    api_key: None,
                    fingerprint: profile.build_fingerprint.clone(),
                    // Deep-park paths stay denied unless the host policy opts in
                    // (SPEC §9.1, T21). The profile carries that decision; it is not
                    // re-derived from anything ambient.
                    policy: if profile.security.experimental_controls {
                        mllm_adapters::fake::ParkPolicy::ExperimentalAllowed
                    } else {
                        mllm_adapters::fake::ParkPolicy::Denied
                    },
                    // Readiness is the served id appearing in the engine's model
                    // list, so the adapter must use the route the deployment serves
                    // rather than the checkpoint path.
                    model_id: effective
                        .routes
                        .first()
                        .cloned()
                        .unwrap_or_else(|| effective.name.clone()),
                })
            }
            Engine::Fake => {
                let clock = self.clock.clone();
                Ok(AdapterSpec::Fake {
                    clock: Arc::new(move || {
                        clock().map_err(|_| {
                            mllm_adapters::traits::RuntimeError::Uncertain(
                                "service observation clock failed".into(),
                            )
                        })
                    }),
                })
            }
            Engine::Sglang => Err(Self::missing(
                "SGLang",
                "its controls need a resolved admin credential and a trusted \
                 observation socket, and neither is wired in production",
            )),
        }
    }
}

#[cfg(test)]
mod tests;

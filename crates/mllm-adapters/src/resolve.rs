//! Engine family to adapter resolution.
//!
//! Adapters were previously constructed at hardcoded call sites, which is why the
//! SGLang adapter was unreachable outside tests and why a third engine family had
//! nowhere to attach. Resolution is one exhaustive match, so adding a family is a
//! compile error until it is handled here rather than a silently missing case.
//!
//! Each family states exactly what it needs. The shapes differ because the engines
//! differ: SGLang controls run under durable coordinator steps and require a frozen
//! launch plus a trusted observer, while vLLM's contract is an endpoint and a park
//! policy. Flattening those into one bag of optional fields would let a caller build
//! an adapter that is missing something its engine requires.

use std::sync::Arc;

use mllm_config::engine_policy::Engine;
use mllm_domain::launch::NativeCandidateLaunch;

use crate::{
    fake::{FakeEngine, ParkPolicy},
    sglang::{SglangAdapter, SglangRuntimeObserver},
    traits::{EngineAdapter, RuntimeError},
    vllm::VllmAdapter,
};

/// Construction inputs for one immutable runtime binding.
pub enum AdapterSpec {
    Vllm {
        endpoint: reqwest::Url,
        api_key: Option<String>,
        fingerprint: String,
        policy: ParkPolicy,
        model_id: String,
    },
    /// SGLang refuses the un-fenced control path, so it takes the frozen launch it
    /// was qualified against and the observer that supplies fresh evidence.
    Sglang {
        frozen: Box<NativeCandidateLaunch>,
        inference: String,
        admin: String,
        observer: Arc<dyn SglangRuntimeObserver>,
    },
    Fake,
}

impl AdapterSpec {
    /// The family this spec builds. Used to check a resolved adapter against the
    /// engine its runtime profile declares.
    pub fn engine(&self) -> Engine {
        match self {
            Self::Vllm { .. } => Engine::Vllm,
            Self::Sglang { .. } => Engine::Sglang,
            Self::Fake => Engine::Fake,
        }
    }
}

/// Build the adapter for a runtime profile's declared engine family.
///
/// `declared` is the family recorded on the profile. A spec for a different family
/// is rejected rather than quietly resolved, because the profile's identity — its
/// build fingerprint, reserved-flag policy and qualification evidence — is only
/// meaningful for the engine it names.
pub fn resolve(
    declared: Engine,
    spec: AdapterSpec,
) -> Result<Box<dyn EngineAdapter>, RuntimeError> {
    if spec.engine() != declared {
        return Err(RuntimeError::Unsupported);
    }
    Ok(match spec {
        AdapterSpec::Vllm {
            endpoint,
            api_key,
            fingerprint,
            policy,
            model_id,
        } => Box::new(VllmAdapter::new(
            endpoint,
            api_key,
            fingerprint,
            policy,
            model_id,
        )),
        AdapterSpec::Sglang {
            frozen,
            inference,
            admin,
            observer,
        } => Box::new(SglangAdapter::from_frozen(
            &frozen, inference, admin, observer,
        )?),
        AdapterSpec::Fake => Box::new(FakeEngine::new()),
    })
}

#[cfg(test)]
mod tests;

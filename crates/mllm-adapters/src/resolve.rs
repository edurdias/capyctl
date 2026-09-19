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
use mllm_domain::launch::NativeLaunch;

use crate::{
    policy::ParkPolicy,
    sglang::{SglangAdapter, SglangRuntimeObserver},
    traits::{EngineAdapter, RuntimeError},
    vllm::VllmAdapter,
};

/// Construction inputs for one immutable runtime binding.
///
/// The vLLM variant carries a whole launch plan and is therefore much larger than
/// the others. It is built once per launch and consumed immediately, so boxing the
/// plan would only move one short-lived allocation while making every caller spell
/// the indirection out.
#[allow(clippy::large_enum_variant)]
pub enum AdapterSpec {
    Vllm {
        endpoint: reqwest::Url,
        api_key: Option<String>,
        fingerprint: String,
        policy: ParkPolicy,
        model_id: String,
        /// The launch plan for an owned launch (Spec §4). A binding that only
        /// talks to an engine somebody else started carries none, and the
        /// resolved adapter then refuses Initialize.
        launch: Option<crate::vllm::PlanInputVllm>,
        /// The per-launch engine credential (Spec §3). It reaches the engine
        /// through the child's environment; nothing renders it on argv.
        engine_key: Option<String>,
    },
    /// SGLang refuses the un-fenced control path, so it takes the frozen launch
    /// it was verified against, the two per-launch credentials, and — when the
    /// binding is a durable control binding — the observer that supplies fresh
    /// evidence. An adapter built for launch has no observer and answers its
    /// control actions with the honest refusal (design §4.4).
    Sglang {
        frozen: Box<NativeLaunch>,
        inference: String,
        admin: String,
        observer: Option<Arc<dyn SglangRuntimeObserver>>,
    },
}

impl AdapterSpec {
    /// The family this spec builds. Used to check a resolved adapter against the
    /// engine its runtime profile declares.
    pub fn engine(&self) -> Engine {
        match self {
            Self::Vllm { .. } => Engine::Vllm,
            Self::Sglang { .. } => Engine::Sglang,
        }
    }
}

/// Build the adapter for a runtime profile's declared engine family.
///
/// `declared` is the family recorded on the profile. A spec for a different family
/// is rejected rather than quietly resolved, because the profile's identity — its
/// build fingerprint, reserved-flag policy and verification evidence — is only
/// meaningful for the engine it names.
///
/// `tools` are the director's process tools (Spec §3). A family that owns the
/// processes it launches needs them to perform Initialize; one that talks to an
/// engine somebody else started is resolved without them and refuses the step.
pub fn resolve(
    declared: Engine,
    spec: AdapterSpec,
    tools: Option<Arc<dyn crate::traits::OwnedProcessLaunch>>,
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
            launch,
            engine_key,
        } => {
            let mut adapter = VllmAdapter::new(endpoint, api_key, fingerprint, policy, model_id);
            if let Some(launch) = launch {
                adapter = adapter.with_launch(launch);
            }
            if let Some(tools) = tools {
                adapter = adapter.with_tools(tools);
            }
            if let Some(engine_key) = engine_key {
                adapter = adapter.with_engine_key(engine_key);
            }
            Box::new(adapter)
        }
        AdapterSpec::Sglang {
            frozen,
            inference,
            admin,
            observer,
        } => {
            let mut adapter =
                SglangAdapter::from_frozen(&frozen, observer)?.with_credentials(inference, admin);
            if let Some(tools) = tools {
                adapter = adapter.with_tools(tools);
            }
            Box::new(adapter)
        }
    })
}

#[cfg(test)]
mod tests;

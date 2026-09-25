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
        /// through the child's environment; nothing renders it on argv. It is
        /// the inference key: `/v1` takes it, and so does ingress.
        engine_key: Option<String>,
        /// SPEC §9.1 / T21, ADR 0012: the per-launch admin credential. It
        /// reaches the child as `MLLM_VLLM_ADMIN_KEY`, the guard keys the
        /// development and control routes with it, and the adapter presents it
        /// on those routes only. `None` is the single-key guard an engine
        /// launched before the admin role keeps until it restarts.
        admin_key: Option<String>,
    },
    /// SGLang refuses the un-fenced control path, so it takes the frozen launch
    /// it was verified against, the two per-launch credentials, and — when the
    /// binding is a durable control binding — the observer that supplies fresh
    /// evidence. An adapter built for launch has no observer and answers its
    /// control actions with the honest refusal (design §4.4).
    ///
    /// The launch context is the rest of what an owned SGLang launch needs, and
    /// each field comes from the only party that honestly holds it: the wrapper
    /// and log paths are properties of this installation (the bindings), the
    /// session ULID is the coordinator session (the spawn factory). A missing
    /// session makes the launch refuse rather than render a descriptor without
    /// a launch scope.
    Sglang {
        frozen: Box<NativeLaunch>,
        inference: String,
        admin: String,
        observer: Option<Arc<dyn SglangRuntimeObserver>>,
        /// The protected entrypoint wrapper path under the installation's
        /// runtime directory; rendering refuses to build a command without it.
        wrapper: Option<std::path::PathBuf>,
        /// Where the engine's own log is expected; quoted (redacted) when a
        /// launch dies before readiness.
        log: Option<String>,
        /// The coordinator session ULID the private descriptor's launch scope
        /// names. Threaded by the resolved-spawn factory, never invented here.
        session: Option<String>,
        /// ADR 0014 §8, SPEC §8.2: the host's approvals for sensitive extra
        /// arguments; `None` approves nothing.
        extra_approvals: Option<String>,
        /// SPEC §8.2 / T21: the per-launch rendezvous directory inside the
        /// host's private root, removed on gone evidence. `None`: the entry
        /// makes its own temporary one.
        rendezvous: Option<std::path::PathBuf>,
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
            admin_key,
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
            if let Some(admin_key) = admin_key {
                adapter = adapter.with_admin_key(admin_key);
            }
            Box::new(adapter)
        }
        AdapterSpec::Sglang {
            frozen,
            inference,
            admin,
            observer,
            wrapper,
            log,
            session,
            extra_approvals,
            rendezvous,
        } => {
            let mut adapter = SglangAdapter::from_frozen(&frozen, observer)
                .map_err(|error| match error {
                    // A shape the frozen contract refuses is not a family
                    // mismatch: the reason names the launch, never a path or
                    // credential, and the caller's journal must be able to
                    // tell the two apart.
                    RuntimeError::Unsupported => RuntimeError::Uncertain(
                        "the frozen SGLang launch shape was refused by its contract".into(),
                    ),
                    other => other,
                })?
                .with_launch(*frozen)
                .with_credentials(inference, admin);
            if let Some(wrapper) = wrapper {
                adapter = adapter.with_wrapper(wrapper);
            }
            if let Some(log) = log {
                adapter = adapter.with_log(log);
            }
            if let Some(session) = session {
                adapter = adapter.with_session(session);
            }
            if let Some(approvals) = extra_approvals {
                adapter = adapter.with_extra_approvals(approvals);
            }
            if let Some(dir) = rendezvous {
                adapter = adapter.with_rendezvous_dir(dir);
            }
            if let Some(tools) = tools {
                adapter = adapter.with_tools(tools);
            }
            Box::new(adapter)
        }
    })
}

#[cfg(test)]
mod tests;

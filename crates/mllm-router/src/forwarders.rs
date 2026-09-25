//! Where the router gets the forwarder for a request.
//!
//! The router used to hold one forwarder per engine family, built once at boot and
//! looked up by the deployment's kind. That table was correct only while an engine's
//! address was a fixed thing: one port, chosen in configuration, with one credential
//! for the life of the process.
//!
//! SPEC §3 makes both per-launch. A port is leased when the deployment starts and
//! returned when it stops, and a key is minted fresh for every launch so a stale one
//! cannot reach a new engine. A boot-time table therefore addresses a port that may
//! now belong to something else and presents a credential that was already retired —
//! and it does so silently, because a wrong port that happens to answer looks exactly
//! like a right one.
//!
//! So the forwarder is resolved per request from the lifecycle authority, which is
//! the only component that knows what is actually running. The cache below is keyed
//! by incarnation rather than by deployment for the same reason: an entry is valid
//! for the launch it was built from and for no other.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mllm_adapters::traits::ChatForward;
use mllm_controller::{LifecycleFault, LifecyclePort};

/// Why a request could not be given a forwarder.
///
/// Carries no credential: the key never appears in a message, and `RuntimeEndpoint`
/// has no Debug so it cannot be formatted into one by accident.
#[derive(Debug, thiserror::Error)]
pub enum ForwarderError {
    /// Nothing is running for this deployment. Distinct from a failure to look:
    /// the authority answered, and its answer was that there is no runtime.
    #[error("no runtime is recorded for deployment {0}")]
    NoRuntime(String),

    /// The authority could not answer. Nothing is known about the runtime either
    /// way, so this is never evidence that the deployment is down.
    #[error("the lifecycle authority could not report a runtime: {0}")]
    Authority(#[from] LifecycleFault),

    /// A retained binding recorded an endpoint that will not parse. The store holds
    /// something no launch could have produced, which is corruption rather than an
    /// absent runtime.
    #[error("the recorded endpoint for deployment {0} is not a URL")]
    Endpoint(String),
}

/// What the router asks for a deployment's forwarder.
///
/// A trait rather than a concrete type so a test can stand a forwarder up directly
/// without a lifecycle authority behind it, and so the live source's caching is not
/// something every caller has to know about.
pub trait ForwarderSource: Send + Sync {
    /// The forwarder for this deployment's currently running engine.
    fn forwarder(&self, deployment: &str) -> Result<Arc<dyn ChatForward>, ForwarderError>;

    /// ADR 0013 §10 (I3): the forwarder for exactly the instance incarnation
    /// `generation` names, the one a request lease was just granted for. Never
    /// another instance's: a lease and its forward name the same incarnation.
    fn forwarder_for(
        &self,
        deployment: &str,
        _generation: i64,
    ) -> Result<Arc<dyn ChatForward>, ForwarderError> {
        self.forwarder(deployment)
    }
}

/// Forwarders built from what the lifecycle authority reports is running.
pub struct LiveForwarders {
    port: Arc<dyn LifecyclePort>,
    /// Keyed by deployment, instance generation and incarnation together.
    /// Keying by deployment alone would hand a restarted deployment the
    /// forwarder built for the launch it replaced, which is the exact failure
    /// this module exists to prevent; and a deployment with several instances
    /// (ADR 0013 §5) has one live incarnation per generation. The legacy
    /// whole-deployment read has no generation (`None`).
    cache: Mutex<HashMap<CacheKey, Arc<dyn ChatForward>>>,
}

type CacheKey = (String, Option<i64>, String);

/// SPEC §17: the cache is bounded. Past this many entries it is emptied, which
/// only costs rebuilding a forwarder; every entry is re-validated per request.
const MAX_CACHED_FORWARDERS: usize = 1024;

impl LiveForwarders {
    pub fn new(port: Arc<dyn LifecyclePort>) -> Self {
        Self {
            port,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The cache holds no invariant beyond the map itself, so a panic elsewhere
    /// while it was held has left nothing half-written. Recovering the map is
    /// honest here; propagating a poisoning would turn an unrelated panic into a
    /// permanently unservable router.
    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<CacheKey, Arc<dyn ChatForward>>> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl LiveForwarders {
    /// Build or reuse the forwarder for one launch. A newer incarnation of the
    /// same (deployment, generation) supersedes every earlier one: the entries
    /// it replaces hold a retired key and a port returned to the lease pool, so
    /// keeping them would only make a stale answer reachable.
    fn build(
        &self,
        deployment: &str,
        generation: Option<i64>,
        runtime: mllm_controller::RuntimeEndpoint,
    ) -> Result<Arc<dyn ChatForward>, ForwarderError> {
        let key = (
            deployment.to_string(),
            generation,
            runtime.incarnation.clone(),
        );
        let mut cache = self.cache();
        if let Some(existing) = cache.get(&key) {
            return Ok(existing.clone());
        }
        let base: reqwest::Url = runtime
            .endpoint
            .parse()
            .map_err(|_| ForwarderError::Endpoint(deployment.to_string()))?;
        let forwarder = mllm_adapters::forward::engine_forwarder(
            base,
            runtime.served_model.clone(),
            runtime.engine_key.clone(),
        );
        cache.retain(|(cached, cached_generation, _), _| {
            cached != deployment || *cached_generation != generation
        });
        if cache.len() >= MAX_CACHED_FORWARDERS {
            cache.clear();
        }
        cache.insert(key, forwarder.clone());
        Ok(forwarder)
    }
}

impl ForwarderSource for LiveForwarders {
    fn forwarder(&self, deployment: &str) -> Result<Arc<dyn ChatForward>, ForwarderError> {
        // Read the authority before taking the lock: the read can block on the
        // coordinator's own store, and holding the cache across it would serialise
        // every deployment's dispatch behind one of them.
        let runtime = self
            .port
            .runtime_endpoint(deployment)?
            .ok_or_else(|| ForwarderError::NoRuntime(deployment.to_string()))?;
        self.build(deployment, None, runtime)
    }

    fn forwarder_for(
        &self,
        deployment: &str,
        generation: i64,
    ) -> Result<Arc<dyn ChatForward>, ForwarderError> {
        let runtime = self
            .port
            .instance_endpoint(deployment, generation)?
            .ok_or_else(|| ForwarderError::NoRuntime(deployment.to_string()))?;
        self.build(deployment, Some(generation), runtime)
    }
}

/// A fixed table of forwarders, for tests that supply an engine double directly
/// rather than standing up a lifecycle authority to launch one.
///
/// Not for production wiring: a fixed table is precisely what leased ports and
/// per-launch keys made wrong.
#[doc(hidden)]
pub struct StaticForwarders(pub HashMap<String, Arc<dyn ChatForward>>);

impl ForwarderSource for StaticForwarders {
    fn forwarder(&self, deployment: &str) -> Result<Arc<dyn ChatForward>, ForwarderError> {
        if let Some(found) = self.0.get(deployment) {
            return Ok(found.clone());
        }
        // A single-entry table is one engine double standing in for whatever the
        // test deployed, whose id the test could not know when it built the table.
        // Two or more entries are an addressed table, so an unmatched name is a
        // miss rather than a default.
        match self.0.values().next() {
            Some(only) if self.0.len() == 1 => Ok(only.clone()),
            _ => Err(ForwarderError::NoRuntime(deployment.to_string())),
        }
    }
}

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mllm_adapters::traits::{ChatForward, EngineAdapter, RuntimeAction};
use mllm_domain::completion::ProcessIdentity;
use mllm_domain::resources::RecipeFootprints;
use mllm_launchers::{AssociationError, DurableSpawn, DurableSpawnOutcome, LaunchAssociation};
use mllm_store::dispatch::CoordinatorSession;
use mllm_store::lifecycle::DeploymentFence;

pub use mllm_adapters::traits::RuntimeError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeOwnership {
    Managed,
    Attached,
}

/// Immutable, process-local composition of a durable runtime record and its adapters.
/// Secret bytes are deliberately absent; only `credential_ref` is retained here.
pub struct RuntimeBinding {
    pub id: String,
    pub deployment_id: String,
    pub revision: i64,
    pub incarnation: String,
    pub qualification_id: String,
    pub recipe: RecipeFootprints,
    pub ownership: RuntimeOwnership,
    pub endpoint: String,
    pub credential_ref: String,
    pub driver: Arc<dyn EngineAdapter>,
    pub forward: Arc<dyn ChatForward>,
}

#[derive(Default)]
pub struct RuntimeBindings {
    retained: Mutex<HashMap<String, Arc<RuntimeBinding>>>,
}

impl RuntimeBindings {
    pub fn retain(&self, binding: Arc<RuntimeBinding>) -> Result<(), RuntimeError> {
        let mut retained = self.retained.lock().map_err(|_| {
            RuntimeError::Uncertain("runtime binding registry lock poisoned".into())
        })?;
        if retained.contains_key(&binding.deployment_id) {
            return Err(RuntimeError::Uncertain(
                "retained runtime binding cannot be replaced in place".into(),
            ));
        }
        retained.insert(binding.deployment_id.clone(), binding);
        Ok(())
    }

    pub fn binding(
        &self,
        deployment_id: &str,
        revision: i64,
    ) -> Result<Arc<RuntimeBinding>, RuntimeError> {
        let retained = self.retained.lock().map_err(|_| {
            RuntimeError::Uncertain("runtime binding registry lock poisoned".into())
        })?;
        let binding = retained.get(deployment_id).ok_or(RuntimeError::Missing)?;
        if binding.revision != revision {
            return Err(RuntimeError::StaleRevision);
        }
        Ok(binding.clone())
    }

    /// Records the control decision without dropping the immutable binding or its leases.
    pub fn park(&self, deployment_id: &str, revision: i64) -> Result<(), RuntimeError> {
        self.control(deployment_id, revision, RuntimeAction::Park)
    }

    pub fn control(
        &self,
        deployment_id: &str,
        revision: i64,
        _action: RuntimeAction,
    ) -> Result<(), RuntimeError> {
        let binding = self.binding(deployment_id, revision)?;
        if binding.ownership == RuntimeOwnership::Attached {
            return Err(RuntimeError::Unsupported);
        }
        Ok(())
    }
}

/// Controller-owned composition which proves the binding is retained before constructing a child.
pub struct DurableRuntimeSupervisor<'a> {
    store: &'a mllm_store::Store,
    session: &'a CoordinatorSession,
    launcher: DurableSpawn,
}

impl<'a> DurableRuntimeSupervisor<'a> {
    pub fn new(store: &'a mllm_store::Store, session: &'a CoordinatorSession) -> Self {
        Self {
            store,
            session,
            launcher: DurableSpawn::new(),
        }
    }

    pub fn spawn(
        &self,
        fence: &DeploymentFence,
        binding_id: &str,
        command: &mllm_adapters::traits::RenderedCommand,
    ) -> Result<DurableSpawnOutcome, RuntimeError> {
        let binding = self
            .store
            .runtime_binding(&fence.deployment_id)
            .map_err(|error| RuntimeError::Uncertain(error.to_string()))?
            .ok_or(RuntimeError::Missing)?;
        if binding.revision != fence.revision {
            return Err(RuntimeError::StaleRevision);
        }
        if binding.id != binding_id {
            return Err(RuntimeError::Missing);
        }
        if binding.ownership == "attached" {
            return Err(RuntimeError::Unsupported);
        }
        self.store
            .arm_runtime_spawn(self.session, fence, binding_id, &binding.incarnation)
            .map_err(|error| RuntimeError::Uncertain(error.to_string()))?;
        let association = StoreAssociation {
            store: self.store,
            session: self.session,
            fence,
            binding_id,
        };
        self.launcher
            .spawn_persisted(&binding.incarnation, command, &association)
            .map_err(|error| RuntimeError::Uncertain(error.to_string()))
    }
}

struct StoreAssociation<'a> {
    store: &'a mllm_store::Store,
    session: &'a CoordinatorSession,
    fence: &'a DeploymentFence,
    binding_id: &'a str,
}

impl LaunchAssociation for StoreAssociation<'_> {
    fn persist_api_identity(&self, identity: &ProcessIdentity) -> Result<(), AssociationError> {
        self.store
            .record_api_identity(self.session, self.fence, self.binding_id, identity)
            .map_err(|error| AssociationError::Uncertain(error.to_string()))
    }
}

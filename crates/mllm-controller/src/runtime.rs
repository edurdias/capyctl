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

/// One controller-local launch capability, created only by a fresh persisted arm.
/// It cannot be cloned, serialized, or inserted into ordinary runtime bindings.
/// Dropping it consumes the attempt without permitting a later replay to spawn.
pub struct NativeCandidateHandoff<'a> {
    store: &'a mllm_store::Store,
    session: &'a CoordinatorSession,
    step_id: String,
    fence: DeploymentFence,
    frozen: mllm_domain::launch::NativeLaunch,
    command: mllm_adapters::traits::RenderedCommand,
    descriptors: mllm_launchers::ProtectedLaunchDescriptors,
    now_ms: &'a dyn Fn() -> Result<i64, RuntimeError>,
}

/// Trusted service dependencies; never populated from candidate or request data.
/// The clock must read current service time on every invocation, not cache arm time.
pub struct NativeCandidateService<'a> {
    pub wrapper: &'a std::path::Path,
    pub now_ms: &'a dyn Fn() -> Result<i64, RuntimeError>,
}

impl<'a> NativeCandidateHandoff<'a> {
    /// Called by the trusted coordinator, never a management/router request.
    /// `preflight` must verify the pinned checkpoint and engine contract without
    /// starting an engine. `resolve` is the service-owned credential provider.
    /// Both callbacks execute only after the arm transaction has committed.
    pub fn arm(
        store: &'a mllm_store::Store,
        session: &'a CoordinatorSession,
        step_id: &str,
        context: mllm_scheduler::residency::AdmissionContext<'_>,
        resolve: &dyn Fn(&str) -> Result<Vec<u8>, RuntimeError>,
        preflight: &dyn Fn(&mllm_domain::launch::NativeLaunch) -> Result<(), RuntimeError>,
        service: NativeCandidateService<'a>,
    ) -> Result<Option<Self>, RuntimeError> {
        use mllm_adapters::sglang::{ProtectedDescriptorFds, SglangLaunch};
        use mllm_store::lifecycle::ArmResult;
        let ArmResult::New { step_id } = store
            .arm_step(session, step_id, context)
            .map_err(|_| native_error("candidate arm rejected"))?
        else {
            return Ok(None);
        };
        let frozen = store
            .candidate_native_launch(
                session,
                &step_id,
                (service.now_ms)().map_err(|_| native_error("candidate clock unavailable"))?,
            )
            .map_err(|_| native_error("candidate descriptor unavailable"))?;
        let execution = store
            .candidate_initialize_execution(session, &step_id)
            .map_err(|_| native_error("candidate execution unavailable"))?;
        let launch = SglangLaunch::from_frozen(&frozen)?;
        let root = std::path::Path::new(frozen.checkpoint_root());
        if !root.is_dir() || root.canonicalize().ok().as_deref() != Some(root) {
            return Err(native_error("candidate checkpoint root unavailable"));
        }
        preflight(&frozen).map_err(|_| native_error("candidate preflight failed"))?;
        let inference = resolve(frozen.inference_credential_ref())
            .map_err(|_| native_error("candidate credential resolution failed"))?;
        let admin = resolve(frozen.admin_credential_ref())
            .map_err(|_| native_error("candidate credential resolution failed"))?;
        let private = serde_json::to_vec(&serde_json::json!({
            "schema_version": 2,
            "kind": "sglang_candidate_private_launch",
            "checkpoint_root": frozen.checkpoint_root(),
            "public_settings": launch.public_metadata(),
            "launch_scope": {
                "session_id": session.id(),
                "deployment_id": execution.token.deployment_id,
                "operation_id": execution.token.operation_id,
                "step_id": execution.token.step_id,
                "revision": execution.token.revision,
                "generation": execution.token.generation,
                "binding_id": execution.binding_id,
                "incarnation": execution.incarnation,
                "issued_at_ms": execution.issued_at_ms,
                "deadline_ms": execution.deadline_ms,
            },
        }))
        .map_err(|_| native_error("candidate descriptor encoding failed"))?;
        let descriptors =
            mllm_launchers::ProtectedLaunchDescriptors::new(&private, &inference, &admin)
                .map_err(|_| native_error("candidate descriptor creation failed"))?;
        let [launch_fd, inference_fd, admin_fd] = descriptors.numbers();
        let command = launch.render_for_launcher(
            ProtectedDescriptorFds::for_launcher(
                launch_fd.into(),
                inference_fd.into(),
                admin_fd.into(),
            )?,
            service.wrapper,
        )?;
        let handoff = Self {
            store,
            session,
            step_id,
            fence: DeploymentFence {
                deployment_id: execution.token.deployment_id,
                revision: execution.token.revision,
                generation: execution.token.generation,
            },
            frozen,
            command,
            descriptors,
            now_ms: service.now_ms,
        };
        // A provider may take time or lose the coordinator session. Revalidate
        // after all external work, as well as immediately before process creation.
        handoff.validate_current()?;
        Ok(Some(handoff))
    }

    /// Bounded public launch specification. Neither paths nor credentials from
    /// the private descriptors are present in this inspectable value.
    pub fn command(&self) -> &mllm_adapters::traits::RenderedCommand {
        &self.command
    }

    pub fn spawn(self, launcher: &DurableSpawn) -> Result<DurableSpawnOutcome, RuntimeError> {
        self.validate_current()?;
        launcher
            .spawn_protected(
                &self.frozen.metadata().incarnation,
                &self.command,
                &self.descriptors,
                &self,
            )
            .map_err(|_| native_error("candidate launch uncertain"))
    }

    fn validate_current(&self) -> Result<(), RuntimeError> {
        mllm_adapters::sglang::SglangLaunch::validate_wrapper_path(std::path::Path::new(
            &self.command.argv[2],
        ))?;
        let current = self
            .store
            .candidate_native_launch(
                self.session,
                &self.step_id,
                (self.now_ms)().map_err(|_| native_error("candidate clock unavailable"))?,
            )
            .map_err(|_| native_error("candidate handoff is stale"))?;
        if current.metadata() != self.frozen.metadata()
            || current.settings() != self.frozen.settings()
            || current.checkpoint_root() != self.frozen.checkpoint_root()
            || current.executable() != self.frozen.executable()
            || current.inference_credential_ref() != self.frozen.inference_credential_ref()
            || current.admin_credential_ref() != self.frozen.admin_credential_ref()
        {
            return Err(native_error("candidate handoff changed"));
        }
        Ok(())
    }
}

impl LaunchAssociation for NativeCandidateHandoff<'_> {
    fn persist_api_identity(&self, identity: &ProcessIdentity) -> Result<(), AssociationError> {
        self.validate_current()
            .map_err(|_| AssociationError::Uncertain("candidate handoff is stale".into()))?;
        self.store
            .record_api_identity(
                self.session,
                &self.fence,
                &self.frozen.metadata().binding_id,
                identity,
            )
            .map_err(|_| AssociationError::Uncertain("candidate API association uncertain".into()))
    }
}

fn native_error(reason: &str) -> RuntimeError {
    RuntimeError::Uncertain(reason.into())
}

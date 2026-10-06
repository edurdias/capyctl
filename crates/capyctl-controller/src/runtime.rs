use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use capyctl_adapters::traits::{ChatForward, EngineAdapter, RuntimeAction};
use capyctl_domain::completion::ProcessIdentity;
use capyctl_domain::launch::NativeLaunch;
use capyctl_domain::resources::RecipeFootprints;
use capyctl_launchers::{AssociationError, DurableSpawn, DurableSpawnOutcome, LaunchAssociation};
use capyctl_store::dispatch::CoordinatorSession;
use capyctl_store::lifecycle::DeploymentFence;

pub use capyctl_adapters::traits::RuntimeError;

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
    pub identity_id: String,
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
    store: &'a capyctl_store::Store,
    session: &'a CoordinatorSession,
    launcher: DurableSpawn,
}

impl<'a> DurableRuntimeSupervisor<'a> {
    pub fn new(store: &'a capyctl_store::Store, session: &'a CoordinatorSession) -> Self {
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
        command: &capyctl_adapters::traits::RenderedCommand,
    ) -> Result<DurableSpawnOutcome, RuntimeError> {
        let binding = self
            .store
            .retained_binding(binding_id)
            .map_err(|error| RuntimeError::Uncertain(error.to_string()))?
            .filter(|binding| binding.deployment_id == fence.deployment_id)
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
    store: &'a capyctl_store::Store,
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

/// Where an armed ordinary initialize's frozen native launch comes from. The store
/// holds no descriptor for it (the candidate rows that did are gone); the
/// application supplies one and must return the same value on every call for the
/// same step, or the handoff refuses to send.
pub trait NativeLaunchSource: Send + Sync {
    fn frozen(
        &self,
        session: &CoordinatorSession,
        step_id: &str,
        now_ms: i64,
    ) -> Result<NativeLaunch, RuntimeError>;
}

/// Renders and spawns a protected native launch for an armed ordinary initialize.
///
/// Native entrypoint denials stay closed (AGENTS.md): this type opens nothing.
/// `ProfileBindings` still refuses SGLang, and nothing implements
/// `NativeLaunchSource` in production until the ordinary native launch is designed.
///
/// One controller-local launch capability, created only by a fresh persisted arm.
/// It cannot be cloned, serialized, or inserted into ordinary runtime bindings.
/// Dropping it consumes the attempt without permitting a later replay to spawn.
pub struct NativeLaunchHandoff<'a> {
    store: &'a capyctl_store::Store,
    session: &'a CoordinatorSession,
    step_id: String,
    fence: DeploymentFence,
    frozen: NativeLaunch,
    command: capyctl_adapters::traits::RenderedCommand,
    descriptors: capyctl_launchers::ProtectedLaunchDescriptors,
    now_ms: &'a dyn Fn() -> Result<i64, RuntimeError>,
    source: &'a dyn NativeLaunchSource,
    token: capyctl_domain::completion::TransitionToken,
    binding_id: String,
    incarnation: String,
    issued_at_ms: i64,
    deadline_ms: i64,
}

/// Trusted service dependencies; never populated from request data.
/// The clock must read current service time on every invocation, not cache arm time.
pub struct NativeLaunchService<'a> {
    pub wrapper: &'a std::path::Path,
    pub now_ms: &'a dyn Fn() -> Result<i64, RuntimeError>,
    pub source: &'a dyn NativeLaunchSource,
}

impl<'a> NativeLaunchHandoff<'a> {
    /// Called by the trusted coordinator, never a management/router request.
    /// `preflight` must verify the pinned checkpoint and engine contract without
    /// starting an engine. `resolve` is the service-owned credential provider.
    /// Both callbacks execute only after the arm transaction has committed.
    pub fn arm(
        store: &'a capyctl_store::Store,
        session: &'a CoordinatorSession,
        step_id: &str,
        context: capyctl_scheduler::residency::AdmissionContext<'_>,
        resolve: &dyn Fn(&str) -> Result<Vec<u8>, RuntimeError>,
        preflight: &dyn Fn(&NativeLaunch) -> Result<(), RuntimeError>,
        service: NativeLaunchService<'a>,
    ) -> Result<Option<Self>, RuntimeError> {
        use capyctl_adapters::sglang::{ProtectedDescriptorFds, SglangLaunch};
        use capyctl_store::lifecycle::ArmResult;
        let (armed, execution) = store
            .arm_initialize_with_context(session, step_id, context)
            .map_err(|_| native_error("arm rejected"))?;
        let ArmResult::New { step_id } = armed else {
            return Ok(None);
        };
        // Only this arm's own transaction returns a context; a replay never does.
        let Some(execution) = execution else {
            return Err(native_error("arm returned no execution context"));
        };
        let now = (service.now_ms)().map_err(|_| native_error("clock unavailable"))?;
        let frozen = service
            .source
            .frozen(session, &step_id, now)
            .map_err(|_| native_error("descriptor unavailable"))?;
        let launch = SglangLaunch::from_frozen(&frozen)?;
        let root = std::path::Path::new(frozen.checkpoint_root());
        if !root.is_dir() || root.canonicalize().ok().as_deref() != Some(root) {
            return Err(native_error("checkpoint root unavailable"));
        }
        preflight(&frozen).map_err(|_| native_error("preflight failed"))?;
        let inference = resolve(frozen.inference_credential_ref())
            .map_err(|_| native_error("credential resolution failed"))?;
        let admin = resolve(frozen.admin_credential_ref())
            .map_err(|_| native_error("credential resolution failed"))?;
        let private = crate::native_launch::private_descriptor(
            session.id(),
            &execution,
            frozen.checkpoint_root(),
            launch.public_metadata(),
            frozen.metadata().placement_digest.as_deref(),
        )?;
        let descriptors =
            capyctl_launchers::ProtectedLaunchDescriptors::new(&private, &inference, &admin)
                .map_err(|_| native_error("descriptor creation failed"))?;
        let [launch_fd, inference_fd, admin_fd] = descriptors.numbers()[..] else {
            return Err(native_error("descriptor creation failed"));
        };
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
                deployment_id: execution.token.deployment_id.clone(),
                revision: execution.token.revision,
                generation: execution.token.generation,
            },
            frozen,
            command,
            descriptors,
            now_ms: service.now_ms,
            source: service.source,
            token: execution.token,
            binding_id: execution.binding_id,
            incarnation: execution.incarnation,
            issued_at_ms: execution.issued_at_ms,
            deadline_ms: execution.deadline_ms,
        };
        // A provider may take time or lose the coordinator session. Revalidate
        // after all external work, as well as immediately before process creation.
        handoff.validate_current()?;
        Ok(Some(handoff))
    }

    /// Bounded public launch specification. Neither paths nor credentials from
    /// the private descriptors are present in this inspectable value.
    pub fn command(&self) -> &capyctl_adapters::traits::RenderedCommand {
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
            .map_err(|_| native_error("launch uncertain"))
    }

    fn validate_current(&self) -> Result<(), RuntimeError> {
        capyctl_adapters::sglang::SglangLaunch::validate_wrapper_path(std::path::Path::new(
            &self.command.argv[2],
        ))?;
        let now = (self.now_ms)().map_err(|_| native_error("clock unavailable"))?;
        let current = self
            .source
            .frozen(self.session, &self.step_id, now)
            .map_err(|_| native_error("handoff is stale"))?;
        if current.metadata() != self.frozen.metadata()
            || current.settings() != self.frozen.settings()
            || current.checkpoint_root() != self.frozen.checkpoint_root()
            || current.executable() != self.frozen.executable()
            || current.inference_credential_ref() != self.frozen.inference_credential_ref()
            || current.admin_credential_ref() != self.frozen.admin_credential_ref()
        {
            return Err(native_error("handoff changed"));
        }
        // The arm's own execution context is the authority for what may still be
        // sent. Re-reading it proves the step, its binding and its fence are the
        // ones this handoff was built from, and that the deadline has not passed.
        let execution = self
            .store
            .initialize_execution(self.session, &self.step_id)
            .map_err(|_| native_error("handoff is stale"))?;
        if execution.token != self.token
            || execution.binding_id != self.binding_id
            || execution.incarnation != self.incarnation
            || execution.deadline_ms != self.deadline_ms
            || now < self.issued_at_ms
            || now >= self.deadline_ms
        {
            return Err(native_error("handoff is stale"));
        }
        Ok(())
    }
}

impl LaunchAssociation for NativeLaunchHandoff<'_> {
    fn persist_api_identity(&self, identity: &ProcessIdentity) -> Result<(), AssociationError> {
        self.validate_current()
            .map_err(|_| AssociationError::Uncertain("handoff is stale".into()))?;
        self.store
            .record_api_identity(
                self.session,
                &self.fence,
                &self.frozen.metadata().binding_id,
                identity,
            )
            .map_err(|_| AssociationError::Uncertain("API association uncertain".into()))
    }
}

fn native_error(reason: &str) -> RuntimeError {
    RuntimeError::Uncertain(reason.into())
}

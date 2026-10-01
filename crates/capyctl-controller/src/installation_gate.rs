//! ADR 0008 (owner decision 2026-09-23), embedded path: the standalone role's
//! engine installation fingerprint and drift, as a remote host agent keeps them.
//!
//! At boot the embedded host registers its one installation: the engine
//! package's version and a digest over its files
//! (`capyctl_agent::installation`). Before every Initialize the installation is
//! measured again. A different digest is drift: it is shown in standalone
//! status, journaled once per newly observed digest, and refused with the
//! closed reason `installation_drift` only when the launch's host policy says
//! `installation_drift: refuse` (default `warn`). An installation that could not
//! be measured at registration is `unmeasured` and never drifts. Adoption,
//! Park and Restore are never refused on drift: a running engine loaded its
//! code at launch.
//!
//! Passing fingerprints are not qualification evidence (AGENTS.md).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use capyctl_adapters::traits::*;
use capyctl_agent::installation::{InstallationDrift, InstallationRegistry};
use capyctl_config::engine_policy::Engine;
use capyctl_domain::completion::EffectObservation;
use capyctl_protocol::pb;
use capyctl_store::ordinary_lifecycle::worker::InitializeWork;

use crate::coordinator::{CoordinatorError, EngineBindings};
use crate::ownership::SharedCoordinatorState;

/// The closed refusal a drifted installation's launch gets under `refuse`.
pub const DRIFT_REFUSAL: &str = "installation_drift";

/// The embedded host's registered installation and what its launches found.
pub struct EmbeddedInstallation {
    profile: String,
    engine: Engine,
    executable: PathBuf,
    /// The fields registration published (version, digest, state).
    registration: pb::RuntimeProfileStatus,
    registry: InstallationRegistry,
}

/// One launch-time measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftCheck {
    /// `(registered, observed)` when this measurement found a drift not
    /// already flagged (a new drift, or a drift to a different digest).
    pub newly_drifted: Option<(String, String)>,
    /// `Some(DRIFT_REFUSAL)` when the policy refuses the drifted launch.
    pub refused: Option<&'static str>,
}

impl EmbeddedInstallation {
    /// Register (measure) the installation `profile` names. A measurement that
    /// fails is `unmeasured`, never a refusal.
    pub fn register(profile: &str, engine: Engine, executable: &Path) -> Self {
        let measurer = capyctl_agent::installation::InstallationMeasurer::new();
        let (version, digest, state) =
            capyctl_agent::installation::registration(&measurer, Some(engine), executable);
        let registration = pb::RuntimeProfileStatus {
            name: profile.to_owned(),
            installation_version: version,
            installation_digest: digest,
            installation_state: state,
            ..Default::default()
        };
        let registry = InstallationRegistry::from_inventory(&pb::ReportInventory {
            profiles: vec![registration.clone()],
            ..Default::default()
        });
        Self {
            profile: profile.to_owned(),
            engine,
            executable: executable.to_owned(),
            registration,
            registry,
        }
    }

    /// The status view: registered version and digest, `measured`,
    /// `unmeasured` or `drifted`, and the digest a launch observed instead.
    pub fn view(&self) -> serde_json::Value {
        let mut profiles = [self.registration.clone()];
        self.registry.overlay(&mut profiles);
        let [profile] = profiles;
        let mut view = serde_json::json!({
            "profile": self.profile,
            // Final review M10: which installation a deployment runs on is
            // matched by its executable (the effective configuration names
            // it); the management API is loopback and admin-only.
            "executable": self.executable.to_string_lossy(),
            "version": profile.installation_version,
            "digest": profile.installation_digest,
            "state": profile.installation_state,
            // ADR 0008: what the launch-time probe found missing (deep_park).
            "capabilities_missing": profile.capabilities_missing,
        });
        if !profile.installation_observed_digest.is_empty() {
            view["observed_digest"] = profile.installation_observed_digest.into();
        }
        view
    }

    /// Measure the installation again for a launch under `policy`.
    pub fn check(&self, policy: InstallationDrift) -> DriftCheck {
        let before = self.registry.drifted(&self.profile);
        let verdict = self
            .registry
            .verify(&self.profile, self.engine, &self.executable, policy);
        let newly_drifted = self
            .registry
            .drifted(&self.profile)
            .filter(|observed| before.as_ref() != Some(observed))
            .and_then(|observed| {
                self.registry
                    .registered(&self.profile)
                    .map(|registered| (registered.to_owned(), observed))
            });
        DriftCheck {
            newly_drifted,
            refused: verdict.err(),
        }
    }
}

/// ADR 0018 §5: the embedded host's installations, keyed by executable (two
/// profiles on one executable are one installation), in registration order.
pub struct EmbeddedInstallations {
    registered: std::sync::RwLock<Vec<Arc<EmbeddedInstallation>>>,
}

impl EmbeddedInstallations {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            registered: Default::default(),
        })
    }

    /// Register (measure) the installation at `executable` unless one is
    /// already registered there. Blocking: it reads the installation's files.
    pub fn register(&self, profile: &str, engine: Engine, executable: &Path) {
        if self.for_executable(executable).is_some() {
            return;
        }
        let registered = Arc::new(EmbeddedInstallation::register(profile, engine, executable));
        if let Ok(mut all) = self.registered.write() {
            if !all.iter().any(|known| known.executable == executable) {
                all.push(registered);
            }
        }
    }

    /// Forget every installation whose executable is not in `executables`
    /// (a removed profile's installation is no longer the host's).
    pub fn retain(&self, executables: &[&Path]) {
        if let Ok(mut all) = self.registered.write() {
            all.retain(|known| executables.contains(&known.executable.as_path()));
        }
    }

    pub fn for_executable(&self, executable: &Path) -> Option<Arc<EmbeddedInstallation>> {
        self.registered
            .read()
            .ok()?
            .iter()
            .find(|known| known.executable == executable)
            .cloned()
    }

    /// Every registered installation's status view, in registration order.
    pub fn views(&self) -> Vec<serde_json::Value> {
        self.registered
            .read()
            .map(|all| all.iter().map(|known| known.view()).collect())
            .unwrap_or_default()
    }
}

/// Wraps an embedded engine adapter: before Initialize the installation is
/// measured, a new drift is journaled, and `refuse` refuses before any effect.
pub struct InstallationGate {
    inner: Arc<dyn EngineAdapter>,
    owner: SharedCoordinatorState,
    installation: Arc<EmbeddedInstallation>,
    policy: InstallationDrift,
    host: String,
}

impl InstallationGate {
    pub fn new(
        inner: Arc<dyn EngineAdapter>,
        owner: SharedCoordinatorState,
        installation: Arc<EmbeddedInstallation>,
        work: &InitializeWork,
    ) -> Arc<Self> {
        let effective = work.effective();
        Arc::new(Self {
            inner,
            owner,
            installation,
            policy: effective.profile.security.installation_drift,
            host: effective.host.name.clone(),
        })
    }

    async fn admitted(&self) -> Result<(), RuntimeError> {
        let installation = self.installation.clone();
        let policy = self.policy;
        let check = tokio::task::spawn_blocking(move || installation.check(policy))
            .await
            .map_err(|_| RuntimeError::Uncertain("the installation was not measured".into()))?;
        if let Some((registered, observed)) = &check.newly_drifted {
            // Evidence only; a journal failure never decides the launch.
            if let Ok(owner) = self.owner.lock() {
                let _ = owner.store().record_installation_drift(
                    &self.host,
                    &self.installation.profile,
                    registered,
                    observed,
                );
            }
        }
        match check.refused {
            Some(reason) => Err(RuntimeError::Refused(reason.into())),
            None => Ok(()),
        }
    }
}

#[async_trait::async_trait]
impl EngineAdapter for InstallationGate {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        if matches!(command.action, RuntimeAction::Initialize) {
            self.admitted().await?;
        }
        self.inner.execute_persisted(command).await
    }
    async fn inspect(&self, member: &MemberRef) -> Result<EngineState, AdapterError> {
        self.inner.inspect(member).await
    }
    async fn render_plan(&self, plan: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        self.inner.render_plan(plan).await
    }
    async fn check_readiness(&self, member: &MemberRef) -> Result<Readiness, AdapterError> {
        self.inner.check_readiness(member).await
    }
    async fn prepare_park(&self, member: &MemberRef) -> Result<Quiescence, AdapterError> {
        self.inner.prepare_park(member).await
    }
    async fn park(
        &self,
        member: &MemberRef,
        level: ParkLevel,
    ) -> Result<ParkOutcome, AdapterError> {
        self.inner.park(member, level).await
    }
    async fn restore(&self, member: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        self.inner.restore(member).await
    }
    async fn reload_weights(&self, member: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        self.inner.reload_weights(member).await
    }
    async fn observe_work(&self, member: &MemberRef) -> Result<WorkObservation, AdapterError> {
        self.inner.observe_work(member).await
    }
    async fn cancel_work(
        &self,
        member: &MemberRef,
        request: &RequestRef,
        require_ack: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        self.inner.cancel_work(member, request, require_ack).await
    }
    async fn idle_before_signal(&self, member: &MemberRef) -> Option<bool> {
        self.inner.idle_before_signal(member).await
    }
}

/// Any embedded bindings, carrying the installations the embedded host
/// registered so the coordinator gates each Initialize on the one its
/// profile's executable names.
pub struct InstalledBindings {
    inner: Arc<dyn EngineBindings>,
    installations: Arc<EmbeddedInstallations>,
}

impl InstalledBindings {
    pub fn new(inner: Arc<dyn EngineBindings>, installations: Arc<EmbeddedInstallations>) -> Self {
        Self {
            inner,
            installations,
        }
    }
}

impl EngineBindings for InstalledBindings {
    fn spec(
        &self,
        work: &InitializeWork,
    ) -> Result<capyctl_adapters::resolve::AdapterSpec, CoordinatorError> {
        self.inner.spec(work)
    }

    fn checkpoint_verifier(&self) -> Option<Arc<capyctl_agent::checkpoint::CheckpointVerifier>> {
        self.inner.checkpoint_verifier()
    }

    fn launch_gone(&self, incarnation: &str) {
        self.inner.launch_gone(incarnation)
    }

    /// ADR 0018 §5: picked by the frozen profile's executable. One that is
    /// not registered is admitted without a drift check: unmeasured is never
    /// a refusal (ADR 0008).
    fn installation(&self, work: &InitializeWork) -> Option<Arc<EmbeddedInstallation>> {
        self.installations
            .for_executable(Path::new(&work.effective().profile.executable))
    }

    fn adapter(
        &self,
        declared: Engine,
        spec: capyctl_adapters::resolve::AdapterSpec,
        tools: Arc<dyn OwnedProcessLaunch>,
    ) -> Result<Arc<dyn EngineAdapter>, CoordinatorError> {
        self.inner.adapter(declared, spec, tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic virtual environment with an SGLang package tree.
    fn venv() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::write(dir.path().join("bin/python3"), "").unwrap();
        let site = dir.path().join("lib/python3.12/site-packages");
        std::fs::create_dir_all(site.join("sglang")).unwrap();
        std::fs::write(site.join("sglang/__init__.py"), "# build\n").unwrap();
        std::fs::create_dir_all(site.join("sglang-0.5.20.dist-info")).unwrap();
        std::fs::write(
            site.join("sglang-0.5.20.dist-info/METADATA"),
            "Name: sglang\nVersion: 0.5.20\n",
        )
        .unwrap();
        dir
    }

    // T21 T22: standalone registers its installation, flags drift in its
    // status once per newly observed digest, and refuses only under `refuse`.
    #[test]
    fn standalone_registers_flags_drift_and_refuses_only_under_refuse() {
        let dir = venv();
        let executable = dir.path().join("bin/python3");
        let installation = EmbeddedInstallation::register("local", Engine::Sglang, &executable);
        let view = installation.view();
        assert_eq!(view["profile"], "local");
        assert_eq!(view["version"], "0.5.20");
        assert_eq!(view["state"], "measured");
        let registered = view["digest"].as_str().unwrap().to_owned();
        assert!(registered.starts_with("sha256:"));
        assert!(view.get("observed_digest").is_none());
        assert_eq!(
            installation.check(InstallationDrift::Refuse),
            DriftCheck {
                newly_drifted: None,
                refused: None
            }
        );

        let file = dir
            .path()
            .join("lib/python3.12/site-packages/sglang/__init__.py");
        std::fs::write(&file, "# patched after boot\n").unwrap();
        let warned = installation.check(InstallationDrift::Warn);
        assert_eq!(warned.refused, None);
        let (from, observed) = warned.newly_drifted.clone().unwrap();
        assert_eq!(from, registered);
        assert_ne!(observed, registered);
        let view = installation.view();
        assert_eq!(view["state"], "drifted");
        assert_eq!(view["digest"], registered.as_str());
        assert_eq!(view["observed_digest"], observed.as_str());
        // The same drift is not journaled twice; `refuse` refuses it.
        assert_eq!(
            installation.check(InstallationDrift::Refuse),
            DriftCheck {
                newly_drifted: None,
                refused: Some(DRIFT_REFUSAL)
            }
        );
        // Measuring as registered again clears the drift.
        std::fs::write(&file, "# build\n").unwrap();
        assert_eq!(installation.check(InstallationDrift::Refuse).refused, None);
        assert_eq!(installation.view()["state"], "measured");
    }

    // T21 T22: an installation that cannot be measured is `unmeasured` and
    // never drifts or refuses.
    #[test]
    fn an_unmeasured_installation_never_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let installation =
            EmbeddedInstallation::register("local", Engine::Vllm, &dir.path().join("vllm"));
        assert_eq!(installation.view()["state"], "unmeasured");
        assert_eq!(installation.check(InstallationDrift::Refuse).refused, None);
    }

    // T21 T22 (ADR 0018 §5): the embedded host keeps one installation per
    // executable; a launch finds its own by executable, an unregistered one
    // finds none (admitted unmeasured), and a removed one is forgotten.
    #[test]
    fn embedded_installations_are_keyed_by_executable() {
        let sglang = venv();
        let other = tempfile::tempdir().unwrap();
        let python = sglang.path().join("bin/python3");
        let vllm = other.path().join("vllm");
        let all = EmbeddedInstallations::new();
        all.register("local-sglang", Engine::Sglang, &python);
        all.register("sglang-again", Engine::Sglang, &python);
        all.register("local-vllm", Engine::Vllm, &vllm);
        let views = all.views();
        assert_eq!(views.len(), 2, "two profiles on one executable are one");
        assert_eq!(views[0]["profile"], "local-sglang");
        assert_eq!(views[0]["state"], "measured");
        assert_eq!(views[1]["state"], "unmeasured");
        assert!(all.for_executable(&python).is_some());
        assert!(all.for_executable(Path::new("/nowhere/python3")).is_none());
        all.retain(&[python.as_path()]);
        assert!(all.for_executable(&vllm).is_none());
        assert_eq!(all.views().len(), 1);
    }
}

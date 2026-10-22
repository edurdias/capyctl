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
//! be measured at registration is `unmeasured` and never drifts. ADR 0031 §8: a
//! custom profile's pinned installation digest is its baseline; registered
//! differently (or unmeasured) it stands drifted from registration on, and its
//! first launch journals that drift. Adoption, Park and Restore are never
//! refused on drift: a running engine loaded its code at launch.
//!
//! Passing fingerprints are not qualification evidence (AGENTS.md).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use capyctl_adapters::traits::*;
use capyctl_agent::installation::{InstallationDrift, InstallationRegistry};
use capyctl_config::custom::CustomProfile;
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
    /// ADR 0031 §3: a custom engine's block; its manifest's `environment`
    /// names the root measured instead of the default one, and (§8) its
    /// pinned installation digest is the registered baseline.
    custom: Option<CustomProfile>,
    /// The fields registration published (version, digest, state).
    registration: pb::RuntimeProfileStatus,
    registry: InstallationRegistry,
    /// The observed digest of the drift last reported for the journal,
    /// `None` while not drifted: each drift is journaled once, a drift found
    /// at registration (ADR 0031 §8) by the first launch.
    reported: std::sync::Mutex<Option<String>>,
    /// ADR 0033 §1: the observed LMCache digest of the drift last journaled.
    lmcache_reported: std::sync::Mutex<Option<String>>,
}

/// One launch-time measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftCheck {
    /// `(registered, observed)` when this measurement found a drift not
    /// already reported (a new drift, a drift to a different digest, or one
    /// registration found and no launch reported yet).
    pub newly_drifted: Option<(String, String)>,
    /// `Some(DRIFT_REFUSAL)` when the policy refuses the drifted launch.
    pub refused: Option<&'static str>,
}

impl EmbeddedInstallation {
    /// Register (measure) the installation `profile` names. A measurement that
    /// fails is `unmeasured`, never a refusal. ADR 0031 §3: a custom engine
    /// is measured with its manifest (`custom`); §8: its pinned installation
    /// digest, when set, is the baseline and a different measurement is
    /// drift, never adopted.
    pub fn register(
        profile: &str,
        engine: Engine,
        executable: &Path,
        custom: Option<&CustomProfile>,
    ) -> Self {
        let measurer = capyctl_agent::installation::InstallationMeasurer::new();
        let mut registration = pb::RuntimeProfileStatus {
            name: profile.to_owned(),
            ..Default::default()
        };
        capyctl_agent::installation::registration(&measurer, Some(engine), executable, custom)
            .write(&mut registration);
        let mut registry = InstallationRegistry::from_inventory(&pb::ReportInventory {
            profiles: vec![registration.clone()],
            ..Default::default()
        });
        if let Some(custom) = custom {
            registry = registry.with_custom(profile, custom.manifest.clone());
        }
        Self {
            profile: profile.to_owned(),
            engine,
            executable: executable.to_owned(),
            custom: custom.cloned(),
            registration,
            registry,
            reported: std::sync::Mutex::new(None),
            lmcache_reported: std::sync::Mutex::new(None),
        }
    }

    /// Whether this is `engine`'s installation at `executable`, measured
    /// with `custom`'s root against its pinned digest. ADR 0018 §5: two
    /// profiles of one engine on one executable are one installation; two
    /// engines on one executable (a built-in and a custom engine included)
    /// are two. ADR 0031 §3, §8: two custom profiles on one executable with
    /// different roots or pinned digests are two.
    fn is(&self, engine: Engine, executable: &Path, custom: Option<&CustomProfile>) -> bool {
        fn scope(custom: Option<&CustomProfile>) -> Option<(Option<&str>, Option<&str>)> {
            custom.map(|c| {
                (
                    c.manifest.environment.as_deref(),
                    c.installation_digest.as_deref(),
                )
            })
        }
        self.engine == engine
            && self.executable == executable
            && scope(self.custom.as_ref()) == scope(custom)
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
        let verdict = self
            .registry
            .verify(&self.profile, self.engine, &self.executable, policy);
        let drifted = self.registry.drifted(&self.profile);
        let newly_drifted = self
            .reported
            .lock()
            .ok()
            .and_then(|mut reported| {
                let new = drifted
                    .as_ref()
                    .filter(|observed| reported.as_ref() != Some(*observed))
                    .cloned();
                *reported = drifted;
                new
            })
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

/// ADR 0018 §5: the embedded host's installations, keyed by executable and
/// engine (two profiles of one engine on one executable are one
/// installation; a built-in and a custom engine on one executable are two,
/// measured and drift-checked separately) and, ADR 0031 §3, §8, a custom
/// engine's manifest `environment` and pinned installation digest, in
/// registration order.
pub struct EmbeddedInstallations {
    registered: std::sync::RwLock<Vec<Arc<EmbeddedInstallation>>>,
}

impl EmbeddedInstallations {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            registered: Default::default(),
        })
    }

    /// Register (measure) `engine`'s installation at `executable` (with
    /// `custom`'s root and pin for a custom engine) unless one is already
    /// registered there. Blocking: it reads the installation's files.
    pub fn register(
        &self,
        profile: &str,
        engine: Engine,
        executable: &Path,
        custom: Option<&CustomProfile>,
    ) {
        if self.find(engine, executable, custom).is_some() {
            return;
        }
        let registered = Arc::new(EmbeddedInstallation::register(
            profile, engine, executable, custom,
        ));
        if let Ok(mut all) = self.registered.write() {
            if !all.iter().any(|known| known.is(engine, executable, custom)) {
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

    /// `engine`'s installation at `executable`, measured with `custom`'s
    /// root and pin for a custom engine (`None` for every other engine).
    pub fn find(
        &self,
        engine: Engine,
        executable: &Path,
        custom: Option<&CustomProfile>,
    ) -> Option<Arc<EmbeddedInstallation>> {
        self.registered
            .read()
            .ok()?
            .iter()
            .find(|known| known.is(engine, executable, custom))
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
/// ADR 0033 §1: a launch planned with an LMCache server has LMCache checked
/// in the same environment, with the same drift policy.
pub struct InstallationGate {
    inner: Arc<dyn EngineAdapter>,
    owner: SharedCoordinatorState,
    installation: Arc<EmbeddedInstallation>,
    policy: InstallationDrift,
    host: String,
    /// ADR 0033 (lifecycle design §2.3): the launch's LMCache state, shared
    /// with its adapter, and what its check needs.
    lmcache: Option<LmcacheCheck>,
}

/// The launch-time LMCache check of one launch (ADR 0033 §1).
struct LmcacheCheck {
    state: Arc<capyctl_adapters::lmcache::LmcacheState>,
    required: bool,
    registered: Option<capyctl_config::lmcache::LmcacheRegistration>,
}

impl InstallationGate {
    pub fn new(
        inner: Arc<dyn EngineAdapter>,
        owner: SharedCoordinatorState,
        installation: Arc<EmbeddedInstallation>,
        work: &InitializeWork,
        lmcache: Option<Arc<capyctl_adapters::lmcache::LmcacheState>>,
    ) -> Arc<Self> {
        let effective = work.effective();
        Arc::new(Self {
            inner,
            owner,
            installation,
            policy: effective.profile.security.installation_drift,
            host: effective.host.name.clone(),
            lmcache: lmcache.map(|state| LmcacheCheck {
                state,
                required: work.lmcache().is_some_and(|plan| plan.required()),
                registered: effective.profile.lmcache.clone(),
            }),
        })
    }

    async fn admitted(&self) -> Result<(), RuntimeError> {
        let installation = self.installation.clone();
        let policy = self.policy;
        let check = tokio::task::spawn_blocking(move || installation.check(policy))
            .await
            .map_err(|_| RuntimeError::Uncertain("the installation was not measured".into()))?;
        if let Some((registered, observed)) = &check.newly_drifted {
            self.journal_drift(&self.installation.profile, registered, observed);
        }
        match check.refused {
            Some(reason) => Err(RuntimeError::Refused(reason.into())),
            None => self.lmcache_admitted().await,
        }
    }

    /// Evidence only; a journal failure never decides the launch.
    fn journal_drift(&self, profile: &str, registered: &str, observed: &str) {
        if let Ok(owner) = self.owner.lock() {
            let _ = owner
                .store()
                .record_installation_drift(&self.host, profile, registered, observed);
        }
    }

    /// ADR 0033 §1 (lifecycle design §2.3): a launch that is to start a
    /// server has LMCache checked in its environment first. Drift is the
    /// engine's: journaled once per digest, refused `installation_drift`
    /// under `refuse`. A failing check refuses `lmcache_unavailable:<reason>`
    /// under `required`; otherwise LMCache is off for the launch with the
    /// check's reason, and the engine starts without it.
    async fn lmcache_admitted(&self) -> Result<(), RuntimeError> {
        let Some(lmcache) = &self.lmcache else {
            return Ok(());
        };
        if !matches!(
            lmcache.state.launch(),
            Some(capyctl_adapters::lmcache::LmcacheLaunch::Start(_))
        ) {
            return Ok(());
        }
        let (engine, executable, registered) = (
            self.installation.engine,
            self.installation.executable.clone(),
            lmcache.registered.clone(),
        );
        let checked = tokio::task::spawn_blocking(move || {
            capyctl_agent::lmcache_install::check_lmcache(
                &capyctl_agent::installation::InstallationMeasurer::new(),
                engine,
                &executable,
                registered.as_ref(),
            )
        })
        .await
        .map_err(|_| RuntimeError::Uncertain("LMCache was not checked".into()))?;
        match checked {
            Ok(install) => {
                if !install.drifted {
                    return Ok(());
                }
                if let Some(registered) = &lmcache.registered {
                    let new = self
                        .installation
                        .lmcache_reported
                        .lock()
                        .map(|mut reported| {
                            let new = reported.as_deref() != Some(install.digest.as_str());
                            *reported = Some(install.digest.clone());
                            new
                        })
                        .unwrap_or(false);
                    if new {
                        self.journal_drift(
                            &format!("{} (LMCache)", self.installation.profile),
                            &registered.digest,
                            &install.digest,
                        );
                    }
                }
                match self.policy {
                    InstallationDrift::Refuse => Err(RuntimeError::Refused(DRIFT_REFUSAL.into())),
                    InstallationDrift::Warn => Ok(()),
                }
            }
            Err(failure) if lmcache.required => {
                Err(RuntimeError::Refused(failure.reason.required_refusal()))
            }
            Err(failure) => {
                lmcache.state.turn_off(failure.reason, failure.detail);
                Ok(())
            }
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
    async fn idle_before_signal(
        &self,
        member: &MemberRef,
    ) -> Option<capyctl_adapters::traits::EngineWork> {
        self.inner.idle_before_signal(member).await
    }
    async fn engine_quiescent(&self, member: &MemberRef, after_ms: i64) -> bool {
        self.inner.engine_quiescent(member, after_ms).await
    }
    async fn wake_canary(
        &self,
        context: &capyctl_domain::completion::StepExecutionContext,
        max_tokens: u32,
        bound: std::time::Duration,
    ) -> Result<capyctl_adapters::completion_probe::CanaryAnswer, RuntimeError> {
        self.inner.wake_canary(context, max_tokens, bound).await
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

    /// ADR 0018 §5: picked by the frozen profile's engine and executable
    /// (and, ADR 0031 §3, §8, a custom engine's manifest root and pinned
    /// digest). One that is not registered is admitted without a drift
    /// check: unmeasured is never a refusal (ADR 0008).
    fn installation(&self, work: &InitializeWork) -> Option<Arc<EmbeddedInstallation>> {
        let profile = &work.effective().profile;
        self.installations.find(
            profile.engine,
            Path::new(&profile.executable),
            profile.custom.as_ref(),
        )
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
    use capyctl_config::custom::CustomManifest;

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

    /// [`venv`] as a virtual environment (`pyvenv.cfg`) with a dependency
    /// beside the SGLang package, which a built-in SGLang measurement does
    /// not cover. Returns the dependency's file.
    fn venv_with_dependency() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = venv();
        std::fs::write(dir.path().join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        let dep = dir.path().join("lib/python3.12/site-packages/dep");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(dep.join("__init__.py"), "x = 1\n").unwrap();
        (dir, dep.join("__init__.py"))
    }

    /// A custom engine's block (version `1.0`), with `environment` when set.
    fn custom(environment: Option<&Path>) -> CustomProfile {
        let mut text = String::from(
            "kind: custom_engine\nschema_version: 1\napi: chat\nversion: \"1.0\"\nargs: [\"{host}\", \"{port}\", \"{model_dir}\"]\n",
        );
        if let Some(root) = environment {
            text.push_str(&format!("environment: {}\n", root.display()));
        }
        CustomProfile::new(CustomManifest::parse(&text).unwrap())
    }

    // T21 T22: standalone registers its installation, flags drift in its
    // status once per newly observed digest, and refuses only under `refuse`.
    #[test]
    fn standalone_registers_flags_drift_and_refuses_only_under_refuse() {
        let dir = venv();
        let executable = dir.path().join("bin/python3");
        let installation =
            EmbeddedInstallation::register("local", Engine::Sglang, &executable, None);
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
            EmbeddedInstallation::register("local", Engine::Vllm, &dir.path().join("vllm"), None);
        assert_eq!(installation.view()["state"], "unmeasured");
        assert_eq!(installation.check(InstallationDrift::Refuse).refused, None);
    }

    // T21 T22 (ADR 0018 §5): the embedded host keeps one installation per
    // executable and engine; a launch finds its own by engine and
    // executable, an unregistered one finds none (admitted unmeasured), and
    // a removed one is forgotten.
    #[test]
    fn embedded_installations_are_keyed_by_executable_and_engine() {
        let sglang = venv();
        let other = tempfile::tempdir().unwrap();
        let python = sglang.path().join("bin/python3");
        let vllm = other.path().join("vllm");
        let all = EmbeddedInstallations::new();
        all.register("local-sglang", Engine::Sglang, &python, None);
        all.register("sglang-again", Engine::Sglang, &python, None);
        all.register("local-vllm", Engine::Vllm, &vllm, None);
        let views = all.views();
        assert_eq!(views.len(), 2, "two profiles on one executable are one");
        assert_eq!(views[0]["profile"], "local-sglang");
        assert_eq!(views[0]["state"], "measured");
        assert_eq!(views[1]["state"], "unmeasured");
        assert!(all.find(Engine::Sglang, &python, None).is_some());
        assert!(all
            .find(Engine::Sglang, Path::new("/nowhere/python3"), None)
            .is_none());
        // Another engine on the same executable is another installation.
        assert!(all.find(Engine::Vllm, &python, None).is_none());
        all.register("vllm-here", Engine::Vllm, &python, None);
        assert_eq!(all.views().len(), 3);
        assert_eq!(
            all.find(Engine::Vllm, &python, None).unwrap().view()["profile"],
            "vllm-here"
        );
        all.retain(&[python.as_path()]);
        assert!(all.find(Engine::Vllm, &vllm, None).is_none());
        assert_eq!(all.views().len(), 2);
    }

    // T44 T21 (ADR 0031 §3): standalone measures a custom engine with its
    // manifest: the whole virtual environment by default, so a changed
    // dependency drifts; it warns, and refuses only under `refuse`. A second
    // profile on the same executable with another root is another
    // installation.
    #[test]
    fn standalone_measures_a_custom_engine_with_its_manifest() {
        use std::os::unix::fs::PermissionsExt;
        let venv = tempfile::tempdir().unwrap();
        let bin = venv.path().join("bin");
        let site = venv.path().join("lib/python3.12/site-packages/dep");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(venv.path().join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        std::fs::write(site.join("__init__.py"), "x = 1\n").unwrap();
        let executable = bin.join("serve");
        std::fs::write(&executable, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let default = custom(None);
        let all = EmbeddedInstallations::new();
        all.register("serve", Engine::Custom, &executable, Some(&default));
        let narrow = custom(Some(&site));
        all.register("serve-narrow", Engine::Custom, &executable, Some(&narrow));
        assert_eq!(all.views().len(), 2, "another root is another installation");
        let installation = all
            .find(Engine::Custom, &executable, Some(&default))
            .unwrap();
        let view = installation.view();
        assert_eq!(view["state"], "measured");
        assert_eq!(view["version"], "1.0");
        assert_eq!(installation.check(InstallationDrift::Refuse).refused, None);
        std::fs::write(site.join("__init__.py"), "x = 2\n").unwrap();
        let warned = installation.check(InstallationDrift::Warn);
        assert_eq!(warned.refused, None);
        assert!(warned.newly_drifted.is_some());
        assert_eq!(installation.view()["state"], "drifted");
        assert_eq!(
            installation.check(InstallationDrift::Refuse).refused,
            Some(DRIFT_REFUSAL)
        );
    }

    // T44 T21 T22 (ADR 0018 §5, ADR 0031 §3): a built-in profile and a
    // custom profile on one executable are two installations whichever
    // registers first, each with its own measurement: the custom one covers
    // the whole virtual environment with the manifest's version. A changed
    // dependency the built-in measurement does not cover is drift for the
    // custom installation only, refused under `refuse`.
    #[test]
    fn a_builtin_and_a_custom_profile_on_one_executable_are_two_installations() {
        for custom_first in [false, true] {
            let (dir, dependency) = venv_with_dependency();
            let python = dir.path().join("bin/python3");
            let serve = custom(None);
            let all = EmbeddedInstallations::new();
            let register_builtin = || all.register("local-sglang", Engine::Sglang, &python, None);
            let register_custom = || all.register("serve", Engine::Custom, &python, Some(&serve));
            if custom_first {
                register_custom();
                register_builtin();
            } else {
                register_builtin();
                register_custom();
            }
            assert_eq!(all.views().len(), 2, "custom first: {custom_first}");
            let sglang = all.find(Engine::Sglang, &python, None).unwrap();
            let engine = all.find(Engine::Custom, &python, Some(&serve)).unwrap();
            assert!(!Arc::ptr_eq(&sglang, &engine));
            let (built_in, custom_view) = (sglang.view(), engine.view());
            assert_eq!(built_in["profile"], "local-sglang");
            assert_eq!(built_in["version"], "0.5.20");
            assert_eq!(built_in["state"], "measured");
            assert_eq!(custom_view["profile"], "serve");
            assert_eq!(custom_view["version"], "1.0");
            assert_eq!(custom_view["state"], "measured");
            assert_ne!(
                built_in["digest"], custom_view["digest"],
                "the custom installation measures the whole environment"
            );

            std::fs::write(&dependency, "x = 2\n").unwrap();
            assert_eq!(
                sglang.check(InstallationDrift::Refuse),
                DriftCheck {
                    newly_drifted: None,
                    refused: None
                },
                "outside the SGLang package"
            );
            let refused = engine.check(InstallationDrift::Refuse);
            assert_eq!(refused.refused, Some(DRIFT_REFUSAL));
            assert!(refused.newly_drifted.is_some());
            assert_eq!(engine.view()["state"], "drifted");
            assert_eq!(sglang.view()["state"], "measured");
        }
    }

    // T44 T21 T22 (ADR 0031 §3, §8): a custom profile's pinned installation
    // digest is the embedded baseline, never the fresh measurement. Equal,
    // it is `measured`. Changed before registration, it is `drifted` from
    // registration on with the measured digest observed; the first launch
    // reports the drift for the journal once, warns under `warn` and refuses
    // under `refuse`. A profile without the pin keeps today's rule and is
    // another installation.
    #[test]
    fn a_pinned_installation_digest_is_the_embedded_baseline() {
        let (dir, dependency) = venv_with_dependency();
        let python = dir.path().join("bin/python3");
        let unpinned = custom(None);
        let approved = capyctl_agent::installation::InstallationMeasurer::new()
            .measure_profile(Engine::Custom, &python, Some(&unpinned.manifest))
            .unwrap()
            .digest;
        let mut pinned = unpinned.clone();
        pinned.installation_digest = Some(approved.clone());
        let register =
            || EmbeddedInstallation::register("serve", Engine::Custom, &python, Some(&pinned));

        let installation = register();
        let view = installation.view();
        assert_eq!(view["state"], "measured");
        assert_eq!(view["digest"], approved.as_str());
        assert_eq!(
            installation.check(InstallationDrift::Refuse),
            DriftCheck {
                newly_drifted: None,
                refused: None
            }
        );

        // Changed between approval and registration: never adopted.
        std::fs::write(&dependency, "x = 2\n").unwrap();
        let installation = register();
        let view = installation.view();
        assert_eq!(view["state"], "drifted");
        assert_eq!(view["digest"], approved.as_str(), "the pinned digest");
        let observed = view["observed_digest"].as_str().unwrap().to_owned();
        assert!(observed.starts_with("sha256:"));
        assert_ne!(observed, approved);
        assert_eq!(
            installation.check(InstallationDrift::Warn),
            DriftCheck {
                newly_drifted: Some((approved.clone(), observed.clone())),
                refused: None
            },
            "the first launch journals the drift found at registration"
        );
        assert_eq!(
            installation.check(InstallationDrift::Refuse),
            DriftCheck {
                newly_drifted: None,
                refused: Some(DRIFT_REFUSAL)
            },
            "journaled once"
        );
        let refusing = register();
        assert_eq!(
            refusing.check(InstallationDrift::Refuse),
            DriftCheck {
                newly_drifted: Some((approved.clone(), observed.clone())),
                refused: Some(DRIFT_REFUSAL)
            }
        );

        // Without the pin the measurement is the baseline, as for every
        // built-in engine; the two are separate installations.
        let all = EmbeddedInstallations::new();
        all.register("serve", Engine::Custom, &python, Some(&pinned));
        all.register("serve-unpinned", Engine::Custom, &python, Some(&unpinned));
        assert_eq!(all.views().len(), 2);
        let plain = all
            .find(Engine::Custom, &python, Some(&unpinned))
            .unwrap()
            .view();
        assert_eq!(plain["state"], "measured");
        assert_eq!(plain["digest"], observed.as_str());

        // Measured as pinned again, the drift clears.
        std::fs::write(&dependency, "x = 1\n").unwrap();
        assert_eq!(installation.check(InstallationDrift::Refuse).refused, None);
        assert_eq!(installation.view()["state"], "measured");
    }

    // T17: the gate does not hide the engine's quiescence.
    #[tokio::test]
    async fn the_gate_forwards_engine_quiescence() {
        use std::os::unix::fs::PermissionsExt;
        struct Quiet;
        #[async_trait::async_trait]
        impl EngineAdapter for Quiet {
            async fn inspect(&self, _: &MemberRef) -> Result<EngineState, AdapterError> {
                Err(AdapterError::UnsupportedCapability)
            }
            async fn render_plan(&self, _: &PlanInput) -> Result<RenderedCommand, AdapterError> {
                Err(AdapterError::UnsupportedCapability)
            }
            async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, AdapterError> {
                Err(AdapterError::UnsupportedCapability)
            }
            async fn prepare_park(&self, _: &MemberRef) -> Result<Quiescence, AdapterError> {
                Err(AdapterError::UnsupportedCapability)
            }
            async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<ParkOutcome, AdapterError> {
                Err(AdapterError::UnsupportedCapability)
            }
            async fn restore(&self, _: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
                Err(AdapterError::UnsupportedCapability)
            }
            async fn reload_weights(&self, _: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
                Err(AdapterError::UnsupportedCapability)
            }
            async fn observe_work(&self, _: &MemberRef) -> Result<WorkObservation, AdapterError> {
                Err(AdapterError::UnsupportedCapability)
            }
            async fn cancel_work(
                &self,
                _: &MemberRef,
                _: &RequestRef,
                _: bool,
            ) -> Result<CancellationOutcome, AdapterError> {
                Err(AdapterError::UnsupportedCapability)
            }
            async fn engine_quiescent(&self, _: &MemberRef, _: i64) -> bool {
                true
            }
        }
        let state = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = Arc::new(std::sync::Mutex::new(
            crate::ownership::OwnedCoordinatorState::open(state.path()).unwrap(),
        ));
        let gate = InstallationGate {
            inner: Arc::new(Quiet),
            owner,
            installation: Arc::new(EmbeddedInstallation::register(
                "local",
                Engine::Vllm,
                &state.path().join("vllm"),
                None,
            )),
            policy: InstallationDrift::Warn,
            host: "host".into(),
            lmcache: None,
        };
        let member = MemberRef {
            deployment_id: "d".into(),
            member_id: "b".into(),
        };
        assert!(gate.engine_quiescent(&member, 0).await);
    }

    /// A server plan whose environment is `dir`.
    fn server_plan(dir: &Path) -> capyctl_adapters::lmcache::ServerPlan {
        capyctl_adapters::lmcache::ServerPlan {
            lmcache_bin: dir.join("bin/lmcache"),
            rpc_port: 25001,
            http_port: 25002,
            pool_gib: 1,
            path: "/usr/bin:/bin".into(),
            home: None,
            cuda_visible_devices: None,
            cuda_device_order: None,
            log: None,
            config_dir: dir.join("lmcache/inc"),
            start_bound: std::time::Duration::from_secs(30),
            proc_root: "/proc".into(),
            required: false,
        }
    }

    // T45: the launch-time LMCache check of an embedded launch that is to
    // start a server: LMCache missing from the environment turns it off for
    // the launch under `auto` (the engine starts without it, nothing
    // refused), and refuses the launch `lmcache_unavailable:not_installed`
    // under `required`; a launch the store planned off is not checked.
    #[tokio::test]
    async fn the_lmcache_check_turns_off_under_auto_and_refuses_under_required() {
        use capyctl_adapters::lmcache::{LmcacheLaunch, LmcacheReport, LmcacheState};
        use capyctl_config::lmcache::LmcacheOffReason;
        use std::os::unix::fs::PermissionsExt;
        let state_dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = Arc::new(std::sync::Mutex::new(
            crate::ownership::OwnedCoordinatorState::open(state_dir.path()).unwrap(),
        ));
        let env = venv();
        let gate = |state: Arc<LmcacheState>, required: bool| InstallationGate {
            inner: Arc::new(NeverInitialized),
            owner: owner.clone(),
            installation: Arc::new(EmbeddedInstallation::register(
                "local",
                Engine::Vllm,
                &env.path().join("bin/vllm"),
                None,
            )),
            policy: InstallationDrift::Warn,
            host: "host".into(),
            lmcache: Some(LmcacheCheck {
                state,
                required,
                registered: None,
            }),
        };
        let auto = LmcacheState::new(LmcacheLaunch::Start(server_plan(env.path())));
        assert!(gate(auto.clone(), false).admitted().await.is_ok());
        assert_eq!(auto.launch(), None, "nothing is started");
        assert!(matches!(
            auto.report(),
            Some(LmcacheReport::Off {
                reason: LmcacheOffReason::NotInstalled,
                connector_failed: false,
                ..
            })
        ));
        let required = LmcacheState::new(LmcacheLaunch::Start(server_plan(env.path())));
        match gate(required.clone(), true).admitted().await {
            Err(RuntimeError::Refused(code)) => {
                assert_eq!(code, "lmcache_unavailable:not_installed")
            }
            other => panic!("required LMCache missing must refuse, got {other:?}"),
        }
        assert!(required.report().is_none(), "a refusal reports nothing");
        let planned_off = LmcacheState::off(LmcacheOffReason::HostOff, "off here".into());
        assert!(gate(planned_off.clone(), true).admitted().await.is_ok());
        assert!(matches!(
            planned_off.report(),
            Some(LmcacheReport::Off {
                reason: LmcacheOffReason::HostOff,
                ..
            })
        ));
    }

    /// An inner adapter the LMCache check tests never reach.
    struct NeverInitialized;

    #[async_trait::async_trait]
    impl EngineAdapter for NeverInitialized {
        async fn inspect(&self, _: &MemberRef) -> Result<EngineState, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn render_plan(&self, _: &PlanInput) -> Result<RenderedCommand, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn prepare_park(&self, _: &MemberRef) -> Result<Quiescence, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<ParkOutcome, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn restore(&self, _: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn reload_weights(&self, _: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn observe_work(&self, _: &MemberRef) -> Result<WorkObservation, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn cancel_work(
            &self,
            _: &MemberRef,
            _: &RequestRef,
            _: bool,
        ) -> Result<CancellationOutcome, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
    }
}

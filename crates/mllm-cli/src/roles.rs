//! Role wiring for standalone: `start standalone` boots the embedded
//! server+host graph in-process against the engine installation this host
//! actually has, and returns an [`App`] handle over the controller and the
//! durable store. The binary mounts authenticated management and inference
//! separately; the management client uses that API for lifecycle commands.
//!
//! The installation arrives through an [`EngineProvider`] rather than being
//! assembled here. Standalone used to build a vLLM adapter and an exec launcher
//! from environment variables and then discard both, because the coordinator
//! resolves an adapter per binding from the frozen profile; what the environment
//! is actually for is saying which engine this host has, and that is all the
//! provider reports.

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mllm_config::defaults::{resolve_startup, LoadOutcome};
use mllm_config::effective::ModelSource;
use mllm_config::engine_policy::Engine;
use mllm_config::schema::ConfigKind;
use mllm_controller::coordinator::{
    CoordinatorOptions, EngineBindings, OwnedCoordinator, ServiceClock, ServiceObservation as _,
    ToolsFactory,
};
use mllm_controller::{CoordinatorLifecycle, OwnedCoordinatorState, ProfileBindings};
use mllm_launchers::DurableProcessLaunch;
use mllm_store::secrets::SecretsKey;
use mllm_store::Store;

use crate::host_observation::{system_clock, HostMemoryObservation};

use crate::grammar::Command as CliCommand;
use crate::output::{ExitCode, StructuredError};

/// The provider seam lives in the controller, so a test double can implement it
/// without depending on this binary. It is re-exported here because this is where
/// standalone is wired.
pub use mllm_controller::engine_provider::{EngineInstallation, EngineProvider, ProviderError};

pub const NOT_IMPLEMENTED_EXIT: ExitCode = ExitCode::UNSUPPORTED;

/// The engine's executable. Required: a host with no engine cannot serve.
/// One of these two must name the family this host publishes; a host that
/// names both is not publishing one installation, and refuses.
const ENGINE_BIN: &str = "MLLM_VLLM_BIN";
const SGLANG_BIN: &str = "MLLM_SGLANG_BIN";
/// The directory model weights live under (Spec §7). Required for the same reason:
/// a guessed store resolves relative paths somewhere the operator never named.
const MODELS_ROOT: &str = "MLLM_MODELS_ROOT";
const KV_CACHE_BYTES: &str = "MLLM_KV_CACHE_BYTES";
const ENGINE_ARGS: &str = "MLLM_ENGINE_ARGS";
const ENGINE_FINGERPRINT: &str = "MLLM_ENGINE_FINGERPRINT";
const DEEP_PARK: &str = "MLLM_DEEP_PARK";
const TRUST_REMOTE_CODE: &str = "MLLM_TRUST_REMOTE_CODE";
const RUNTIME_DIR: &str = "MLLM_RUNTIME_DIR";
const INSTALLATION_DRIFT: &str = "MLLM_INSTALLATION_DRIFT";
/// SPEC §15.2: a run-time override of the engines' loopback port range, as
/// `start-end`, so two roles on one machine lease different engine ports.
pub const ENGINE_PORTS_ENV: &str = "MLLM_STANDALONE_ENGINE_PORTS";
/// SPEC §16.5 default engine port range.
const DEFAULT_ENGINE_PORTS: (u16, u16) = (8100, 8199);

/// A conservative KV grant for a unified-memory host: the ledger's deployment
/// budget bounds the engine's pool, and a smaller grant keeps two engines from
/// overcommitting the domain during a stop-start overlap.
const DEFAULT_KV_CACHE: &str = "16GiB";
/// vLLM's startup check requires the model's context to fit the KV pool, and a
/// modern checkpoint's default context would demand far more than the grant.
const DEFAULT_ENGINE_ARGS: &str = "--max-model-len 4096";
/// How long `<engine> --version` is given before the probe is a refusal. A version
/// print that takes longer than this is not a healthy installation.
const FINGERPRINT_TIMEOUT: Duration = Duration::from_secs(20);

/// A booted standalone deployment graph: the controller operation engine
/// plus the durable store it runs against. Tests and the CLI drive
/// lifecycle through `controller`; the store is a separate connection to
/// the same WAL-backed file for direct observation. It is an `Rc`
/// (not `Arc`) because `Store` is not `Sync` and standalone observes it from one
/// thread; concurrent sharing comes with the F3 task architecture.
pub struct App {
    /// The lifecycle authority. One coordinator owns the durable state behind the
    /// controller lock, which is what makes a second authority impossible rather
    /// than merely discouraged.
    pub controller: Arc<CoordinatorLifecycle>,
    /// The coordinator's owned task. Dropping it requests shutdown, so the app holds
    /// it for as long as it serves.
    _coordinator: OwnedCoordinator,
    /// A separate read-only connection for direct observation in tests. It never
    /// writes: the coordinator is the only writer.
    pub store: Rc<Store>,
    /// The engine installation this host published. Deployments are qualified
    /// against it, so it is kept rather than recomposed per request — recomposing
    /// risks declaring one thing at boot and a different thing at deploy.
    installation: EngineInstallation,
    /// The environment fingerprint published with the installation, kept for the
    /// same reason.
    environment_fingerprint: String,
    /// Observed host capacity the published limits were derived from.
    capacity_bytes: i64,
    /// The NVIDIA device publication this boot observed, carried so every host
    /// document it builds states the same placement evidence.
    inventory: Option<crate::device_inventory::InventoryPublication>,
    /// Servable router (F1: the standalone role's inference surface).
    router: axum::Router,
    management: axum::Router,
    deps: mllm_router::RouterDeps,
    api_key: String,
    /// The embedded role's supervisors: readiness (SPEC §4.3, P3: re-proves
    /// the engines a restart adopted before any request is forwarded to
    /// them), engine exits (SPEC §13.2, W13) and, for bindings that read a
    /// checkpoint, checkpoint digests (ADR 0014 §7, WE3). Joined by
    /// [`App::shutdown`]; aborted when the app is dropped.
    supervision: crate::shutdown::Supervision,
    /// SPEC §10, ADR 0013 §8 (W10): the switcher, held so shutdown joins the
    /// `--evict` follow-ups it started.
    switcher: Arc<mllm_controller::switching::Switcher>,
    /// ADR 0008 (owner decision 2026-09-23): the engine installation this
    /// boot registered, and any drift its launches found since.
    engine_installation: Arc<mllm_controller::installation_gate::EmbeddedInstallation>,
    /// SPEC §15.2 (R13) / §15.3: values of the standalone document accepted but
    /// not honoured (only the `server.tls` block an older generator wrote),
    /// reported by the role at boot so nobody believes them in force.
    config_notices: Vec<String>,
}

impl App {
    /// SPEC §16.5: mount only on the separate loopback management listener.
    pub fn management_router(&self) -> axum::Router {
        self.management.clone()
    }

    /// SPEC §15.3: the standalone document's accepted-but-ignored values, one
    /// line each, for the role to report at boot.
    pub fn config_notices(&self) -> &[String] {
        &self.config_notices
    }

    pub fn router(&self) -> axum::Router {
        self.router.clone()
    }

    pub fn deps(&self) -> &mllm_router::RouterDeps {
        &self.deps
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// ADR 0008: the registered installation's status view.
    pub fn installation_view(&self) -> serde_json::Value {
        self.engine_installation.view()
    }

    /// SPEC §4.3 (owner decision P3): an ordinary stop of the standalone role is a
    /// service restart. The coordinator's worker is joined so its durable work is
    /// recorded before the controller lock is released; no engine is stopped, and
    /// the next boot adopts and re-proves every Ready engine it left running.
    pub async fn shutdown(
        self,
    ) -> Result<mllm_controller::coordinator::WorkerStatus, mllm_controller::coordinator::CoordinatorError>
    {
        // ADR 0015 invariant 6: nothing the role started outlives it. The
        // supervisors finish the pass they are in and are joined, then the
        // switcher's follow-ups, then the coordinator's worker.
        self.supervision
            .join(crate::shutdown::SUPERVISION_JOIN_BOUND)
            .await;
        self.switcher
            .shutdown(crate::shutdown::SUPERVISION_JOIN_BOUND)
            .await;
        self._coordinator.shutdown().await
    }
}

/// The standalone listeners' loopback addresses (SPEC §16.5 defaults), each
/// overridable for one run so two roles can share a machine. SPEC §15.2: a
/// run-time override of an ordinary setting, never of the safety limit, so an
/// address that is not loopback is refused.
pub const INFERENCE_ADDR_ENV: &str = "MLLM_STANDALONE_INFERENCE_ADDR";
pub const MANAGEMENT_ADDR_ENV: &str = "MLLM_STANDALONE_MANAGEMENT_ADDR";

fn loopback_address(variable: &str, default: &str) -> Result<std::net::SocketAddr, StartError> {
    let text = match std::env::var_os(variable) {
        None => default.to_owned(),
        Some(value) => value
            .into_string()
            .map_err(|_| StartError::Setting(format!("{variable} is not valid text")))?,
    };
    text.parse::<std::net::SocketAddr>()
        .ok()
        .filter(|address| address.ip().is_loopback() && address.port() != 0)
        .ok_or_else(|| {
            StartError::Setting(format!(
                "{variable} must be a loopback address with a port, e.g. 127.0.0.1:8443"
            ))
        })
}

pub fn standalone_inference_address() -> Result<std::net::SocketAddr, StartError> {
    loopback_address(INFERENCE_ADDR_ENV, "127.0.0.1:8443")
}

pub fn standalone_management_address() -> Result<std::net::SocketAddr, StartError> {
    loopback_address(MANAGEMENT_ADDR_ENV, "127.0.0.1:7443")
}

impl App {

    /// Create a deployment that can actually be started.
    ///
    /// A deployment comes into existence together with the effective configuration
    /// it will be qualified against, because the coordinator starts only what it can
    /// qualify. Creating a bare record first — which is what the previous path did —
    /// produces a deployment that can be named and never run.
    pub fn deploy(&self, name: &str, source: ModelSource) -> Result<String, StartError> {
        self.deploy_with_deadline(
            name,
            source,
            crate::standalone_config::DEFAULT_REQUEST_DEADLINE,
        )
    }

    /// Deploy while naming the request deadline the deployment carries.
    ///
    /// The deadline bounds how far ahead an operation on this deployment may be
    /// scheduled, so it is the deployment's property rather than the caller's, and
    /// stating it is how the live suite observes what the bound actually does.
    /// [`App::deploy`] passes the default.
    pub fn deploy_with_deadline(
        &self,
        name: &str,
        source: ModelSource,
        request_deadline: &str,
    ) -> Result<String, StartError> {
        let host = crate::standalone_config::host_policy(
            &self.installation,
            &self.environment_fingerprint,
            self.capacity_bytes,
            self.inventory.as_ref(),
        );
        let mut deployment = crate::standalone_config::deployment_document(
            name,
            name,
            &source,
            self.installation.engine,
            self.capacity_bytes,
            request_deadline,
            self.installation.deep_park,
        );
        // ADR 0014 §2: the engine configuration the environment asked for.
        deployment["engine_config"] = self.installation.engine_config.clone();
        let receipt = self
            .controller
            .create_configuration(
                "standalone",
                name,
                &serde_json::json!({ "config": deployment }).to_string(),
                &host,
            )
            .map_err(|error| StartError::Deploy(error.to_string()))?;
        Ok(receipt.deployment_id)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("config: {0}")]
    Config(#[from] mllm_config::error::ConfigError),
    #[error("store: {0}")]
    Store(#[from] mllm_store::StoreError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("credentials missing: refusing to serve without a generated api key")]
    MissingCredentials,
    /// Spec §8: no engine installation, no boot. The message names what was
    /// expected, because a host that cannot start an engine should say why rather
    /// than come up serving nothing.
    #[error("no engine installation: {0}")]
    NoEngineInstallation(String),
    #[error("controller ownership: {0}")]
    Ownership(#[from] mllm_controller::OwnedStateError),
    #[error("coordinator: {0}")]
    Coordinator(#[from] mllm_controller::coordinator::CoordinatorError),
    #[error("deploy: {0}")]
    Deploy(String),
    /// SPEC §15.3: a run-time setting that is present but malformed is refused.
    #[error("invalid setting: {0}")]
    Setting(String),
}

impl From<ProviderError> for StartError {
    fn from(error: ProviderError) -> Self {
        match error {
            ProviderError::NoEngineInstallation(what) => StartError::NoEngineInstallation(what),
        }
    }
}

impl From<StartError> for StructuredError {
    fn from(err: StartError) -> Self {
        let code = match err {
            StartError::Config(_) => "invalid_config",
            StartError::NoEngineInstallation(_) => "invalid_config",
            StartError::Setting(_) => "invalid_config",
            _ => "internal",
        };
        StructuredError {
            code,
            message: err.to_string(),
        }
    }
}

/// The engine installation this host declares through its environment.
///
/// Spec §7: everything the published host table needs is stated here, so what a
/// deployment is qualified against is what the operator configured. Nothing is
/// invented: without an executable and a model store there is no installation, and
/// standalone refuses to boot rather than come up unable to run anything.
pub struct EnvEngineProvider;

impl EnvEngineProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EnvEngineProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// A variable's value, or `None` when it is unset or empty. An empty value is not a
/// setting: it is the shape a mistyped export leaves behind.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn no_installation(what: impl Into<String>) -> ProviderError {
    ProviderError::NoEngineInstallation(what.into())
}

/// The standalone deep-park switch. SPEC §9.1 / ADR 0012: unset means on and
/// `off` is the host opt-out. SPEC §15.3 (T03): anything else, an empty export
/// included, is refused rather than read as either, because a mistyped opt-out
/// silently left on is the failure this switch exists to prevent.
fn deep_park_switch() -> Result<bool, ProviderError> {
    let Some(value) = std::env::var_os(DEEP_PARK) else {
        return Ok(true);
    };
    match value.to_str() {
        Some("on") => Ok(true),
        Some("off") => Ok(false),
        _ => Err(no_installation(format!(
            "{DEEP_PARK} must be `on` or `off` (unset means on; `off` opts this host \
             out of deep parking); got {value:?}"
        ))),
    }
}

/// ADR 0008 (owner decision 2026-09-23): the standalone host's
/// `security.installation_drift`. Unset means `warn`; SPEC §15.3 (T03):
/// anything other than `warn` or `refuse` is refused rather than guessed.
fn installation_drift_switch() -> Result<mllm_config::effective::InstallationDrift, ProviderError> {
    use mllm_config::effective::InstallationDrift;
    let Some(value) = std::env::var_os(INSTALLATION_DRIFT) else {
        return Ok(InstallationDrift::Warn);
    };
    match value.to_str() {
        Some("warn") => Ok(InstallationDrift::Warn),
        Some("refuse") => Ok(InstallationDrift::Refuse),
        _ => Err(no_installation(format!(
            "{INSTALLATION_DRIFT} must be `warn` or `refuse` (unset means warn); got {value:?}"
        ))),
    }
}

/// The engines' loopback port range: the default unless this run names one.
/// SPEC §15.3: a malformed, empty, reversed or privileged range is refused.
fn engine_ports() -> Result<(u16, u16), ProviderError> {
    let Some(value) = std::env::var_os(ENGINE_PORTS_ENV) else {
        return Ok(DEFAULT_ENGINE_PORTS);
    };
    value
        .to_str()
        .and_then(|text| text.split_once('-'))
        .and_then(|(start, end)| Some((start.parse::<u16>().ok()?, end.parse::<u16>().ok()?)))
        .filter(|&(start, end)| start >= 1024 && start <= end)
        .ok_or_else(|| {
            no_installation(format!(
                "{ENGINE_PORTS_ENV} must be an inclusive port range `start-end` with \
                 1024 <= start <= end, e.g. 8100-8199; got {value:?}"
            ))
        })
}

impl EngineProvider for EnvEngineProvider {
    fn installation(&self) -> Result<EngineInstallation, ProviderError> {
        let vllm = env_value(ENGINE_BIN);
        let sglang = env_value(SGLANG_BIN);
        let (executable, engine) = match (vllm, sglang) {
            (Some(_), Some(_)) => {
                return Err(no_installation(format!(
                    "this host declares two engines ({ENGINE_BIN} and {SGLANG_BIN}); \
                     standalone publishes exactly one"
                )))
            }
            (Some(vllm), None) => (PathBuf::from(vllm), Engine::Vllm),
            (None, Some(sglang)) => (PathBuf::from(sglang), Engine::Sglang),
            (None, None) => {
                return Err(no_installation(format!(
                    "this host declares no engine: set {ENGINE_BIN} (or {SGLANG_BIN} \
                     for SGLang) to the engine's executable and {MODELS_ROOT} to the \
                     directory its weights live under"
                )))
            }
        };
        let models_root = PathBuf::from(env_value(MODELS_ROOT).ok_or_else(|| {
            no_installation(format!(
                "this host names no model store: set {MODELS_ROOT} to the directory \
                 weights live under (the engine itself comes from {ENGINE_BIN})"
            ))
        })?);
        if !models_root.is_dir() {
            return Err(no_installation(format!(
                "{MODELS_ROOT} is not a directory: {}",
                models_root.display()
            )));
        }
        // SPEC §9.1 / T21 / ADR 0012: deep parking is on unless the host opts
        // out. Sleep mode follows the same switch as deep parking.
        let deep_park = deep_park_switch()?;
        let trust_remote_code = env_value(TRUST_REMOTE_CODE).is_some_and(|value| value == "1");
        let installation_drift = installation_drift_switch()?;
        let engine_ports = engine_ports()?;
        let build_fingerprint = match env_value(ENGINE_FINGERPRINT) {
            Some(declared) => declared,
            None => probe_fingerprint(&executable)?,
        };
        let kv_cache_bytes =
            env_value(KV_CACHE_BYTES).unwrap_or_else(|| DEFAULT_KV_CACHE.to_string());
        // ADR 0014 §1: the installation keeps host-fixed arguments only; engine
        // tuning belongs to the deployment. SGLang's protected entry takes no
        // argument vector (`engine_policy.rs` refuses any on that family).
        let args = match engine {
            Engine::Vllm => env_value(ENGINE_ARGS)
                .unwrap_or_else(|| DEFAULT_ENGINE_ARGS.to_string())
                .split(' ')
                .filter(|argument| !argument.is_empty())
                .map(str::to_owned)
                .collect(),
            Engine::Sglang => Vec::new(),
        };
        // ADR 0014 §2, §5: the generated standalone deployment states its KV
        // cache; its memory request is the Ready allocation it declares.
        let engine_config = serde_json::json!({"memory": {"kv_cache": kv_cache_bytes}});
        Ok(EngineInstallation {
            engine,
            executable,
            build_fingerprint,
            engine_config,
            deep_park,
            trust_remote_code,
            models_root,
            runtime_dir: runtime_dir(engine, deep_park)?,
            args,
            installation_drift,
            engine_ports,
        })
    }

    fn bindings(
        &self,
        _clock: ServiceClock,
        log_dir: PathBuf,
        runtime_dir: PathBuf,
    ) -> Arc<dyn EngineBindings> {
        // ADR 0014 §7, Q9: the checkpoint stat cache is private host state,
        // kept beside the logs in the standalone state directory.
        let cache = log_dir.with_file_name("checkpoints");
        let bindings = ProfileBindings::new(log_dir.clone(), runtime_dir).with_checkpoint_cache(cache);
        // SPEC §9.2 (W5): memory-saver SGLang launches enroll their saver
        // observation in a private directory beside the logs. One that cannot
        // be made private leaves the source unset, and Park is refused.
        let observation = log_dir.with_file_name("observation");
        let private = {
            use std::os::unix::fs::{DirBuilderExt, MetadataExt};
            let _ = std::fs::DirBuilder::new().mode(0o700).create(&observation);
            std::fs::symlink_metadata(&observation).is_ok_and(|meta| {
                meta.is_dir()
                    && meta.uid() == unsafe { libc::geteuid() }
                    && meta.mode() & 0o7777 == 0o700
            })
        };
        Arc::new(if private {
            bindings.with_saver_observation(observation)
        } else {
            bindings
        })
    }

    fn tools_factory(&self) -> ToolsFactory {
        // Spec §3: the builder owns the processes it launches, and the association
        // it is given is what records their identities before they can run.
        Arc::new(|association| Arc::new(DurableProcessLaunch::new(association)))
    }
}

/// Where mllm's own guard middleware lives.
///
/// Spec §3: the guard is imported by the engine over `PYTHONPATH`, so a directory
/// that does not contain it is not a runtime directory. The checkout layout is the
/// default because standalone is run from one; an installed layout names its own.
///
/// `deep_park` is the host's switch: with it on, a parking vLLM deployment
/// renders sleep mode, whose entry imports the capability probes (ADR 0008).
fn runtime_dir(engine: Engine, deep_park: bool) -> Result<PathBuf, ProviderError> {
    let dir = match env_value(RUNTIME_DIR) {
        Some(declared) => PathBuf::from(declared),
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("runtime"),
    };
    if !dir.join("mllm_vllm_guard.py").is_file() {
        return Err(no_installation(format!(
            "{} holds no mllm_vllm_guard.py, so the engine's control routes could \
             not be guarded; set {RUNTIME_DIR} to mllm's runtime directory",
            dir.display()
        )));
    }
    // ADR 0014 §6 / owner decision Q11: every vLLM launch runs through mllm's
    // protected entry in the same directory; refused here, where the
    // installation is resolved, rather than as an engine that exits at spawn.
    if !dir.join(mllm_adapters::vllm::VLLM_ENTRY).is_file() {
        return Err(no_installation(format!(
            "{} holds no {}, so vLLM's reserved settings could not be enforced; \
             set {RUNTIME_DIR} to mllm's runtime directory",
            dir.display(),
            mllm_adapters::vllm::VLLM_ENTRY
        )));
    }
    // The protected wrapper's validator refuses a path whose canonical form
    // differs from itself, so a runtime directory named through `..` is
    // canonicalized here, where the installation is resolved — a launch would
    // otherwise fail at render with a refusal this boot could have prevented
    // (found live: the default CARGO_MANIFEST_DIR-relative directory carries
    // `..` and every SGLang launch was refused before spawning).
    let dir = dir
        .canonicalize()
        .map_err(|error| no_installation(format!("runtime directory {}: {error}", dir.display())))?;
    // SPEC §9.1, §13.3 / T21 T37: the same integrity the host agent requires
    // before a launch. The engine imports mllm's modules from this directory,
    // so one another account could rewrite is refused here, before anything
    // is served.
    mllm_agent::runtime_integrity::verify(
        &dir,
        mllm_agent::runtime_integrity::required_files(engine, deep_park),
    )
    .map_err(|error| {
        no_installation(format!(
            "runtime_integrity: {error}; mllm's runtime modules must be regular files \
             owned by this user, never writable by other, and writable by group \
             only through this user's private group"
        ))
    })?;
    Ok(dir)
}

/// What the installed engine says it is.
///
/// The fingerprint pins the recipe, so it has to come from the installation rather
/// than from a constant that would keep claiming the same build after an upgrade.
/// A probe that fails or hangs is a refusal: an engine that cannot print its own
/// version is not one this host should publish.
fn probe_fingerprint(executable: &Path) -> Result<String, ProviderError> {
    let mut child = Command::new(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            no_installation(format!(
                "{} could not be run to read its version ({error}); set \
                 {ENGINE_FINGERPRINT} if this host publishes one another way",
                executable.display()
            ))
        })?;
    let deadline = Instant::now() + FINGERPRINT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(no_installation(format!(
                    "{} did not print its version within {} seconds",
                    executable.display(),
                    FINGERPRINT_TIMEOUT.as_secs()
                )));
            }
            Err(error) => {
                return Err(no_installation(format!(
                    "the version probe for {} could not be waited on: {error}",
                    executable.display()
                )))
            }
        }
    };
    let mut printed = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        // The child has exited, so this reads what it left in the pipe and returns.
        let _ = std::io::Read::read_to_string(&mut stdout, &mut printed);
    }
    let fingerprint = printed.trim().to_owned();
    if !status.success() || fingerprint.is_empty() {
        return Err(no_installation(format!(
            "{} printed no version, so there is nothing to pin this recipe to; set \
             {ENGINE_FINGERPRINT} to publish one explicitly",
            executable.display()
        )));
    }
    Ok(fingerprint)
}

/// Boot the embedded standalone graph (SPEC §15.2 no-config matrix) against the
/// engine installation this host's environment declares.
pub async fn start_standalone(state_dir: &Path) -> Result<App, StartError> {
    start_standalone_inner(
        state_dir,
        Arc::new(EnvEngineProvider::new()),
        crate::host_observation::proc_meminfo(),
    )
    .await
}

/// Boot against an explicit provider, which is how a test supplies an installation
/// it controls instead of one the environment happens to name.
pub async fn start_standalone_with(
    state_dir: &Path,
    provider: Arc<dyn EngineProvider>,
) -> Result<App, StartError> {
    start_standalone_inner(state_dir, provider, crate::host_observation::proc_meminfo()).await
}

/// As [`start_standalone_with`], reading host memory through `memory`. SPEC
/// §7: standalone derives its limits from observed capacity; a test states that
/// capacity explicitly so it passes the same on a machine with little memory
/// free as on one with plenty.
pub async fn start_standalone_with_memory(
    state_dir: &Path,
    provider: Arc<dyn EngineProvider>,
    memory: crate::host_observation::MemoryReader,
) -> Result<App, StartError> {
    start_standalone_inner(state_dir, provider, memory).await
}

async fn start_standalone_inner(
    state_dir: &Path,
    provider: Arc<dyn EngineProvider>,
    memory: crate::host_observation::MemoryReader,
) -> Result<App, StartError> {
    // Fail-closed credentials (SPEC §15.2): the generated api key lives in
    // the protected credentials file. The hardcoded fallback exists ONLY
    // for a boot that generated the config (and its credentials) this run
    // — an existing state dir missing its credentials refuses to serve
    // instead of serving with a guessable key.
    let outcome = resolve_startup(ConfigKind::Standalone, None, state_dir)?;
    let created_this_boot = matches!(
        outcome,
        LoadOutcome::Generated {
            created_identity: true,
            ..
        }
    );
    // SPEC §10 (W10): the switch drain bound, `server.switching.drain_timeout`
    // of the standalone document; 30 s when it names none.
    let (switch_drain_timeout, timing_header, config_notices) = {
        let path = match &outcome {
            LoadOutcome::Loaded(path) => PathBuf::from(path),
            LoadOutcome::Generated { config_path, .. } => config_path.clone(),
        };
        let text = std::fs::read_to_string(&path)?;
        let document = mllm_config::parse_strict(ConfigKind::Standalone, &text)
            .map_err(|error| StartError::Deploy(format!("standalone configuration: {error}")))?;
        // SPEC §15.3: a value this role would silently ignore (another state
        // directory or listener, TLS, a model store, profiles, numeric limits)
        // is refused before any side effect.
        let absolute = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        let config_dir = absolute(path.parent().unwrap_or(Path::new(".")));
        // SPEC §15.2 (R13): the `server.tls` block an older generator wrote is
        // accepted and reported, never rewritten; every other value is refused.
        let ignored =
            mllm_config::standalone::check_honoured(&document, &config_dir, &absolute(state_dir))
                .map_err(|error| StartError::Deploy(format!("standalone configuration: {error}")))?;
        (
            mllm_config::remote_roles::switch_drain_timeout(&document["server"])
                .map_err(|error| StartError::Deploy(format!("standalone configuration: {error}")))?,
            // SPEC §17 (M80): `server.observability.timing_header`, off unless set.
            mllm_config::remote_roles::timing_header(&document["server"])
                .map_err(|error| StartError::Deploy(format!("standalone configuration: {error}")))?,
            ignored.iter().map(ToString::to_string).collect::<Vec<_>>(),
        )
    };
    let db_path = state_dir.join("server").join("srv.sqlite3");
    let store = Rc::new(Store::open(&db_path)?);
    let api_key = match read_api_key(state_dir) {
        Some(k) => k,
        None if created_this_boot => "mllm-local".to_string(),
        None => return Err(StartError::MissingCredentials),
    };

    // Spec §8: what this host publishes about its engine is what it has. There is
    // no fallback installation: a host with none refuses to boot rather than come
    // up serving an engine nobody configured.
    let installation = provider.installation()?;
    let environment_fingerprint = format!("standalone-{}", installation.build_fingerprint);
    let capacity_bytes = memory()
        .map(|sample| sample.capacity_bytes)
        .map_err(|error| StartError::Deploy(format!("host capacity unreadable: {error}")))?;

    // SPEC §3: the NVIDIA device inventory is a host fact published at boot
    // like the fingerprints. The collector is bounded and closed-error: a
    // machine with no NVIDIA devices, or one whose collection fails or
    // overruns its bound, publishes nothing, and an SGLang deployment then
    // fails placement honestly at the native gate instead of the host
    // claiming placement it cannot corroborate.
    let inventory = crate::device_inventory::collect(
        installation
            .runtime_dir
            .parent()
            .unwrap_or(&installation.runtime_dir),
    );

    // The host's own accounting units, resolved before anything can observe or be
    // admitted against them. The coordinator's observation source is named by these,
    // so it has to exist before the coordinator does.
    let declared_host = {
        let host = crate::standalone_config::host_policy(
            &installation,
            &environment_fingerprint,
            capacity_bytes,
            inventory.as_ref(),
        );
        let probe = crate::standalone_config::deployment_document(
            "policy-probe",
            "policy-probe",
            &ModelSource::Local {
                path: "/dev/null".into(),
            },
            installation.engine,
            capacity_bytes,
            crate::standalone_config::DEFAULT_REQUEST_DEADLINE,
            installation.deep_park,
        );
        mllm_config::effective::resolve_effective(&probe, &host)
            .map_err(|error| StartError::Deploy(format!("host policy invalid: {error}")))?
            .host
    };

    // Spec §3: the identity key that seals every per-launch engine key lives in a
    // file beside the database, not in it, and it has to outlive the process or a
    // restart could not authenticate against an engine it left running.
    let secrets = SecretsKey::load_or_create(&state_dir.join("identity").join("secrets.key"))?;
    // The coordinator opens the durable state itself and holds the controller lock
    // for as long as it runs, so nothing else may act as an authority over it.
    let owner = Arc::new(std::sync::Mutex::new(
        OwnedCoordinatorState::open_with_secrets(&state_dir.join("server"), secrets)?,
    ));
    let options = CoordinatorOptions {
        // Spec §4: a cold start reads weights off disk, which this project measured
        // taking a minute on a small model; the protocol bound would give up on a
        // healthy engine mid-load. ADR 0014 amendment A1: each Initialize is
        // bounded by its step deadline, which carries the deployment's
        // `timeouts.initialize` (or the operator's override); this is only the
        // ceiling, the largest request deadline a host may declare.
        initialize_timeout: Duration::from_secs(3600),
        // Spec §5: what a terminated process group is given before it is killed.
        terminate_grace: Duration::from_secs(15),
        // SPEC §6.3: a Stop drains accepted requests for the same bound a
        // switch does before it terminates.
        stop_drain_timeout: switch_drain_timeout,
        ..Default::default()
    };
    // ADR 0008 (owner decision 2026-09-23): register the installation (its
    // version and a digest over its files) as a host agent does at start; each
    // Initialize measures it again for drift. Bounded, reads files only, and a
    // failure is `unmeasured`, never a refusal.
    let engine_installation = {
        let (engine, executable) = (installation.engine, installation.executable.clone());
        Arc::new(
            tokio::task::spawn_blocking(move || {
                mllm_controller::installation_gate::EmbeddedInstallation::register(
                    crate::standalone_config::STANDALONE_PROFILE,
                    engine,
                    &executable,
                )
            })
            .await
            .map_err(|_| StartError::Deploy("installation registration did not finish".into()))?,
        )
    };
    let bindings: Arc<dyn EngineBindings> =
        Arc::new(mllm_controller::installation_gate::InstalledBindings::new(
            provider.bindings(
                system_clock(),
                state_dir.join("logs"),
                installation.runtime_dir.clone(),
            ),
            engine_installation.clone(),
        ));
    // ADR 0014 §7 (WE3): the embedded host measures pending checkpoint digests
    // with the verifier its launches use, so they share one stat cache.
    let mut supervision = crate::shutdown::Supervision::new();
    if let Some(checkpoints) = bindings.checkpoint_verifier() {
        supervision.supervise(
            mllm_controller::checkpoint_digests::CheckpointDigests::new(
                owner.clone(),
                mllm_controller::checkpoint_digests::LocalDigests::new(checkpoints),
            )
            .spawn_until(supervision.cancel_signal()),
        );
    }
    let coordinator = OwnedCoordinator::spawn_resolved(
        owner.clone(),
        Arc::new(
            HostMemoryObservation::with_reader(declared_host.domains.keys().cloned(), memory.clone())
                // ADR 0007 (found live 2026-09-23, matrix M33): credit the
                // engines already resident here instead of charging them twice.
                .with_process_residency(mllm_agent::process_residency::ResidencySampler::nvidia()),
        ),
        system_clock(),
        options,
        bindings,
        provider.tools_factory(),
    )?;
    let credentials = std::fs::read_to_string(state_dir.join("identity/credentials"))?;
    let admin = credentials.lines().find_map(|line| line.strip_prefix("admin_token: "))
        .ok_or(StartError::MissingCredentials)?;
    let management_credentials = mllm_management::ManagementCredentials::from_trusted_resolver(admin, &api_key)
        .map_err(|_| StartError::MissingCredentials)?;
    let host = crate::standalone_config::host_policy(
        &installation, &environment_fingerprint, capacity_bytes, inventory.as_ref());
    // SPEC §4.3 (P3): the Ready engines a previous run left running are adopted
    // by the coordinator at start; this supervisor re-proves each one locally
    // before dispatch reopens.
    supervision.supervise(
        mllm_controller::local_readiness::LocalReadiness::new(owner.clone())
            .spawn_until(supervision.cancel_signal()),
    );
    // SPEC §13.2 (W13): an embedded engine that exits is closed and settled with
    // verified cleanup. Supervised with readiness, so it ends with it.
    supervision.supervise(
        mllm_controller::engine_exit::EngineExits::new(coordinator.commands())
            .spawn_local_until(supervision.cancel_signal()),
    );
    let configuration = Arc::new(mllm_management::configuration::SharedConfigurationSource::new(
        owner, host, "standalone").map_err(|_| StartError::Deploy("management configuration unavailable".into()))?);
    // SPEC §10, ADR 0013 §8 (W10): one switcher for request-driven switching
    // and the operator's `start --evict`, so both take the same host turns.
    let switcher = Arc::new(mllm_controller::switching::Switcher::new(
        coordinator.commands(),
        mllm_controller::switching::SwitchOptions {
            drain_timeout: switch_drain_timeout,
            ..Default::default()
        },
    ));
    let source = Arc::new(
        mllm_management::actions::OwnedActionSource::new(configuration, coordinator.commands())
            .map_err(|_| StartError::Deploy("management actions unavailable".into()))?
            .with_switcher(switcher.clone()),
    );
    // SPEC §4.3: the explicit drain of the embedded host, by its published name
    // or the `standalone` alias the CLI uses.
    let drain = mllm_management::drain::drain_router(
        mllm_management::ManagementCredentials::from_trusted_resolver(admin, &api_key)
            .map_err(|_| StartError::MissingCredentials)?,
        source.clone(),
        vec![declared_host.name.clone(), "standalone".to_owned()],
    );
    // ADR 0008: the registered installation, shown by standalone status.
    let installation_view = mllm_management::installation::installation_router(
        mllm_management::ManagementCredentials::from_trusted_resolver(admin, &api_key)
            .map_err(|_| StartError::MissingCredentials)?,
        engine_installation.clone(),
    );
    // SPEC §17 (M80): the router's per-request latency distributions. The
    // embedded engine has no host ingress or load report, so only the router
    // tier is measured here.
    let inflight = Arc::new(mllm_router::admission::InFlight::default());
    inflight.latency.set_timing_header(timing_header);
    let latency_view = {
        let recorder = inflight.latency.clone();
        mllm_management::metrics::latency_router(
            mllm_management::ManagementCredentials::from_trusted_resolver(admin, &api_key)
                .map_err(|_| StartError::MissingCredentials)?,
            Arc::new(move |deployment: Option<&str>| {
                mllm_router::timing::latency_report(&recorder, &[], deployment)
            }),
        )
    };
    let management = mllm_management::lifecycle_router(management_credentials, source)
        .merge(drain)
        .merge(installation_view)
        .merge(latency_view);
    let controller = Arc::new(
        CoordinatorLifecycle::new(coordinator.commands()).with_switcher(switcher.clone()),
    );
    let deps = mllm_router::RouterDeps {
        controller: controller.clone(),
        // Spec §3: a leased port and a per-launch key belong to one launch, so the
        // forwarder is built from what the coordinator recorded for the launch that
        // is running rather than from a table assembled at boot.
        forwards: Arc::new(mllm_router::forwarders::LiveForwarders::new(
            controller.clone(),
        )),
        limits: mllm_router::QueueLimits {
            max_requests_per_deployment: 32,
            max_buffered_bytes_total: 64 * 1024 * 1024,
        },
        api_key: Some(api_key.clone()),
        inflight,
        activation_join: Arc::new(mllm_router::WakeJoin::new()),
    };
    let router = mllm_router::serve_router(deps.clone());
    // Publish the ceiling before anything can be admitted against it. A deployment
    // cannot be qualified until the host has said what it will allow, and the policy
    // is imported with the observation that justifies it rather than on its own.
    {
        // The same source the coordinator will observe through, so the policy and
        // the evidence for it cannot disagree about what a domain is called.
        let observations =
            HostMemoryObservation::with_reader(declared_host.domains.keys().cloned(), memory.clone())
            .observe(declared_host.name.clone())
            .await
            .map_err(|error| StartError::Deploy(error.to_string()))?;
        controller
            .publish_resource_policy(&declared_host, &observations)
            .map_err(|error| StartError::Deploy(error.to_string()))?;
    }
    // SPEC §10 step 1, §16.2 (W10 gap b): waiting requests are bounded by the
    // embedded host's published `resource_policy.queue`.
    if let Some(queue) = controller
        .queue_policy()
        .map_err(|error| StartError::Deploy(error.to_string()))?
    {
        deps.inflight.waiting.set_limits(crate::remote_roles::wait_limits(&queue));
    }
    Ok(App {
        controller,
        _coordinator: coordinator,
        store,
        installation,
        environment_fingerprint,
        capacity_bytes,
        inventory,
        router,
        management,
        deps,
        api_key,
        supervision,
        switcher,
        engine_installation,
        config_notices,
    })
}

/// Read the generated API key from the protected credentials file (F0's
/// fail-closed generation; the key is printed never, only used).
fn read_api_key(state_dir: &Path) -> Option<String> {
    let creds = std::fs::read_to_string(state_dir.join("identity").join("credentials")).ok()?;
    creds
        .lines()
        .find_map(|l| l.strip_prefix("api_key: ").map(str::to_string))
}

pub fn dispatch(command: &CliCommand) -> Result<Infallible, StructuredError> {
    Err(StructuredError::not_yet_implemented(&command.label()))
}

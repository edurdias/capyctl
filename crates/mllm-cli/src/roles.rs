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
use mllm_agent::gpu_memory::{GpuSampler, HostShape};

use crate::grammar::Command as CliCommand;
use crate::output::{ExitCode, StructuredError};

/// The provider seam lives in the controller, so a test double can implement it
/// without depending on this binary. It is re-exported here because this is where
/// standalone is wired.
pub use mllm_controller::engine_provider::{
    EngineInstallation, EngineProvider, NamedInstallation, ProviderError,
};

pub const NOT_IMPLEMENTED_EXIT: ExitCode = ExitCode::UNSUPPORTED;

/// The engine's executable. A host with no engine cannot serve: one of these
/// two, or a profile registered with `mllm engine add`, must name one. ADR
/// 0018 §5: both set publish two profiles, `local-vllm` and `local-sglang`.
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

/// SPEC §8.2 / T21 (owner decision 2026-09-25): the standalone rendezvous
/// root under the state directory, the same name a host uses.
const RENDEZVOUS_DIR: &str = "rendezvous";

/// SPEC §8.2 / T21 (owner decision 2026-09-25): create the private rendezvous
/// root (0700, this user) or refuse one that is not, as a host does; no
/// permissions are changed. Then remove every directory in it that belongs to
/// no launch the store still retains: a stopped launch's directory whose
/// removal a crash interrupted, or one a previous run never recorded.
fn prepare_rendezvous_root(
    state_dir: &Path,
    retained: &std::collections::BTreeSet<String>,
) -> Result<PathBuf, StartError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let root = state_dir.join(RENDEZVOUS_DIR);
    match std::fs::DirBuilder::new().mode(0o700).create(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let private = std::fs::symlink_metadata(&root).is_ok_and(|meta| {
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o7777 == 0o700
    });
    if !private {
        return Err(StartError::Setting(format!(
            "the rendezvous directory `{RENDEZVOUS_DIR}` in the state directory must be a \
             directory owned by this user with mode 0700; no permissions were changed"
        )));
    }
    mllm_agent::rendezvous::RendezvousRoot::new(root.clone()).sweep(retained);
    Ok(root)
}

/// A conservative KV grant for a unified-memory host: the ledger's deployment
/// budget bounds the engine's pool, and a smaller grant keeps two engines from
/// overcommitting the domain during a stop-start overlap.
const DEFAULT_KV_CACHE: &str = "16GiB";
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
    /// ADR 0018 §5: the embedded host's installations and the host document
    /// they publish. Deployments are qualified against that document, so it is
    /// kept rather than recomposed per request — recomposing risks declaring
    /// one thing at boot and a different thing at deploy. `engine add` and
    /// `remove` replace it through the control socket.
    host: Arc<crate::standalone_engines::EmbeddedHost>,
    /// Observed host capacity the published limits were derived from.
    capacity_bytes: i64,
    /// Design §1: the host's GPU shape, sampled once at boot. It decides the
    /// domains the host publishes and, on a discrete host, the deployment
    /// template and the memory observation.
    pub gpu_shape: HostShape,
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

    /// ADR 0008: the first registered installation's status view.
    pub fn installation_view(&self) -> serde_json::Value {
        self.host
            .installations()
            .views()
            .into_iter()
            .next()
            .unwrap_or(serde_json::Value::Null)
    }

    /// ADR 0018 §5: the runtime profiles the embedded host publishes now.
    pub fn profiles(&self) -> Vec<String> {
        self.host.profiles()
    }

    /// The embedded host document deployments are qualified against now.
    pub fn host_document(&self) -> serde_json::Value {
        self.host.document()
    }

    /// SPEC §4.3 (owner decision P3): an ordinary stop of the standalone role is a
    /// service restart. The coordinator's worker is joined so its durable work is
    /// recorded before the controller lock is released; no engine is stopped, and
    /// the next boot adopts and re-proves every Ready engine it left running.
    pub async fn shutdown(
        self,
    ) -> Result<
        mllm_controller::coordinator::WorkerStatus,
        mllm_controller::coordinator::CoordinatorError,
    > {
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
        let host = self.host.document();
        // The first profile: the environment's when a variable names one.
        let first = self
            .host
            .named()
            .into_iter()
            .next()
            .ok_or_else(|| StartError::Deploy("the host publishes no engine".into()))?;
        // Design §3: a discrete host sizes the deployment from the checkpoint's
        // weights; a unified host from its observed capacity, as before.
        let memory = match &self.gpu_shape {
            HostShape::Discrete(gpus) => {
                let weights = checkpoint_weights(&first.installation.models_root, &source)?;
                crate::standalone_config::discrete_template_memory(
                    gpus,
                    self.capacity_bytes,
                    weights,
                    declared_kv_cache(&first.installation)?,
                )
                .ok_or_else(|| StartError::Deploy("the host publishes no GPU".into()))?
            }
            HostShape::Unified | HostShape::NoGpu => {
                crate::standalone_config::TemplateMemory::Unified {
                    capacity_bytes: self.capacity_bytes,
                }
            }
        };
        // Spec §3, §11: a request the device cannot hold is refused here,
        // before anything is stored.
        let mut deployment = crate::standalone_config::deployment_document(
            name,
            name,
            &source,
            first.installation.engine,
            &memory,
            request_deadline,
            first.installation.deep_park,
            &first.profile,
        )
        .map_err(StartError::Template)?;
        // ADR 0014 §2: the engine configuration the environment asked for. On a
        // discrete host its memory block is the template's: the device request
        // is sized from the checkpoint, not from a unified KV default.
        let template = deployment["engine_config"]["memory"].take();
        deployment["engine_config"] = first.installation.engine_config.clone();
        if matches!(
            memory,
            crate::standalone_config::TemplateMemory::Device { .. }
        ) {
            deployment["engine_config"]["memory"] = template;
        }
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
    /// ADR 0018 §5: an environment-variable profile and a registered one
    /// share a name.
    #[error("{0}")]
    ProfileExists(String),
    #[error("controller ownership: {0}")]
    Ownership(#[from] mllm_controller::OwnedStateError),
    #[error("coordinator: {0}")]
    Coordinator(#[from] mllm_controller::coordinator::CoordinatorError),
    #[error("deploy: {0}")]
    Deploy(String),
    /// SPEC §15.3: a run-time setting that is present but malformed is refused.
    #[error("invalid setting: {0}")]
    Setting(String),
    /// Design §1: integrated and discrete GPUs on one host are refused at
    /// boot, not guessed at (`unsupported_gpu_topology`).
    #[error("{0}")]
    GpuTopology(mllm_agent::gpu_memory::GpuShapeError),
    /// Spec §3, §11: the generated deployment cannot fit this host's device
    /// (`insufficient_device_memory`).
    #[error("{0}")]
    Template(crate::standalone_config::TemplateError),
}

/// Discrete GPU design §6: each discrete GPU's total memory by driver index,
/// from the boot sample. Empty on a unified host or one without a GPU.
fn device_totals(shape: &HostShape) -> std::collections::BTreeMap<u32, i64> {
    match shape {
        HostShape::Discrete(gpus) => gpus
            .iter()
            .filter_map(|gpu| Some((gpu.index, gpu.memory.as_ref()?.total_bytes)))
            .collect(),
        HostShape::Unified | HostShape::NoGpu => Default::default(),
    }
}

/// ADR 0014 §5: the sum of a local checkpoint's weight-file sizes, sized with
/// the same bounded, confined walk the digest uses (a stat per file, no hash).
/// A relative path resolves against the model store (spec §7).
///
/// Design §3: a discrete deployment's device request is sized from these
/// weights, so a local checkpoint that cannot be sized here (a path outside the
/// store, a missing directory) is refused rather than given a guessed request.
/// A Hugging Face or HTTP source is not on disk yet: `None`, and the request is
/// sized once the download is measured (controller ruling, ADR 0014 §7).
fn checkpoint_weights(models_root: &Path, source: &ModelSource) -> Result<Option<i64>, StartError> {
    let ModelSource::Local { path } = source else {
        return Ok(None);
    };
    let checkpoint = models_root.join(path);
    mllm_agent::checkpoint::CheckpointVerifier::in_memory()
        .size(models_root, &checkpoint)
        .map(|size| Some(size.weights_bytes))
        .map_err(|error| {
            StartError::Deploy(format!(
                "the checkpoint at {} could not be sized ({error:?}); a discrete GPU \
                 deployment is sized from its weights",
                checkpoint.display()
            ))
        })
}

/// Controller ruling (discrete GPU design §3): the KV cache the operator stated
/// with `MLLM_KV_CACHE_BYTES`, which a discrete template honours within the
/// card or refuses. `None` when the installation carries the unified default.
fn declared_kv_cache(installation: &EngineInstallation) -> Result<Option<i64>, StartError> {
    if !installation.kv_cache_declared {
        return Ok(None);
    }
    let stated = installation.engine_config["memory"]["kv_cache"]
        .as_str()
        .ok_or_else(|| StartError::Setting(format!("{KV_CACHE_BYTES} is not a byte size")))?;
    mllm_config::effective::parse_bytes(stated)
        .ok()
        .filter(|bytes| *bytes > 0)
        .map(Some)
        .ok_or_else(|| {
            StartError::Setting(format!("{KV_CACHE_BYTES} is not a byte size: {stated}"))
        })
}

impl From<ProviderError> for StartError {
    fn from(error: ProviderError) -> Self {
        match error {
            ProviderError::NoEngineInstallation(what) => StartError::NoEngineInstallation(what),
            error @ ProviderError::ProfileExists(_) => StartError::ProfileExists(error.to_string()),
        }
    }
}

impl From<StartError> for StructuredError {
    fn from(err: StartError) -> Self {
        let code = match err {
            // SPEC §13.2 / T33: a store written by a newer mllm is not a
            // transient fault; restarting this binary never heals it.
            StartError::Store(mllm_store::StoreError::FromNewerVersion { .. })
            | StartError::Ownership(mllm_controller::OwnedStateError::Store(
                mllm_store::StoreError::FromNewerVersion { .. },
            )) => crate::output::STORE_FROM_NEWER_VERSION,
            StartError::Config(_) => "invalid_config",
            StartError::NoEngineInstallation(_) => "invalid_config",
            StartError::ProfileExists(_) => "profile_exists",
            StartError::Setting(_) => "invalid_config",
            StartError::GpuTopology(error) => error.code(),
            StartError::Template(ref error) => error.code(),
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
pub struct EnvEngineProvider {
    /// The managed runtime directory (`<state root>/runtime`) used when
    /// `MLLM_RUNTIME_DIR` is unset. `None` means there is none, and a run
    /// must name its runtime directory.
    managed_runtime: Option<PathBuf>,
}

impl EnvEngineProvider {
    pub fn new() -> Self {
        Self {
            managed_runtime: None,
        }
    }

    /// SPEC §3.3 / ADR 0001: without `MLLM_RUNTIME_DIR`, the engine runs
    /// from the binary's embedded runtime, written to `dir`.
    pub fn with_managed_runtime(dir: PathBuf) -> Self {
        Self {
            managed_runtime: Some(dir),
        }
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

impl EnvEngineProvider {
    /// The role's installation of `engine` at `executable`: models root,
    /// ports, KV default, runtime directory and switches from the environment.
    fn role_installation(
        &self,
        engine: Engine,
        executable: PathBuf,
    ) -> Result<EngineInstallation, ProviderError> {
        self.role_installation_as(engine, executable, None, None)
    }

    /// As [`Self::role_installation`], with the version given instead of
    /// probed and the deep-park switch given instead of read. ADR 0018 §5: a
    /// registered profile's version was checked by `engine add`, so nothing is
    /// executed for it at start, and its own deep-park switch decides which
    /// runtime modules it needs.
    fn role_installation_as(
        &self,
        engine: Engine,
        executable: PathBuf,
        fingerprint: Option<&str>,
        deep_park: Option<bool>,
    ) -> Result<EngineInstallation, ProviderError> {
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
        // out. Sleep mode follows the same switch as deep parking. A malformed
        // switch is refused even when a registered profile states its own.
        let switch = deep_park_switch()?;
        let deep_park = deep_park.unwrap_or(switch);
        let trust_remote_code = env_value(TRUST_REMOTE_CODE).is_some_and(|value| value == "1");
        let installation_drift = installation_drift_switch()?;
        let engine_ports = engine_ports()?;
        let build_fingerprint = match (fingerprint, env_value(ENGINE_FINGERPRINT)) {
            (Some(registered), _) => registered.to_owned(),
            (None, Some(declared)) => declared,
            (None, None) => probe_fingerprint(&executable)?,
        };
        let declared_kv = env_value(KV_CACHE_BYTES);
        let kv_cache_declared = declared_kv.is_some();
        let kv_cache_bytes = declared_kv.unwrap_or_else(|| DEFAULT_KV_CACHE.to_string());
        // ADR 0014 §1: the installation keeps host-fixed arguments only; engine
        // tuning belongs to the deployment. SGLang's protected entry takes no
        // argument vector (`engine_policy.rs` refuses any on that family).
        // ADR 0014 §5 (owner decision 2026-09-25): no `--max-model-len`
        // default; an undeclared context is fitted to the KV grant at launch.
        // An explicit `MLLM_ENGINE_ARGS` is kept as the host's fixed args.
        let args = match engine {
            Engine::Vllm => env_value(ENGINE_ARGS)
                .unwrap_or_default()
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
            kv_cache_declared,
            deep_park,
            trust_remote_code,
            models_root,
            runtime_dir: runtime_dir(engine, deep_park, self.managed_runtime.as_deref())?,
            args,
            installation_drift,
            engine_ports,
        })
    }
}

impl EngineProvider for EnvEngineProvider {
    /// The environment's one installation. ADR 0018 §5: with both variables
    /// set this is the vLLM one; [`Self::installations`] publishes both.
    fn installation(&self) -> Result<EngineInstallation, ProviderError> {
        match (env_value(ENGINE_BIN), env_value(SGLANG_BIN)) {
            (Some(vllm), _) => self.role_installation(Engine::Vllm, vllm.into()),
            (None, Some(sglang)) => self.role_installation(Engine::Sglang, sglang.into()),
            (None, None) => Err(no_installation(format!(
                "this host declares no engine: set {ENGINE_BIN} (or {SGLANG_BIN} \
                 for SGLang) to the engine's executable and {MODELS_ROOT} to the \
                 directory its weights live under"
            ))),
        }
    }

    fn installations(
        &self,
        registered: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Vec<NamedInstallation>, ProviderError> {
        // ADR 0018 §5: one variable is `local`, as always; both (refused
        // before, so nothing depends on it) are `local-vllm` and `local-sglang`.
        let named = |profile: &str, installation| NamedInstallation {
            profile: profile.into(),
            installation,
        };
        let mut all = match (env_value(ENGINE_BIN), env_value(SGLANG_BIN)) {
            (Some(vllm), None) => vec![named(
                crate::standalone_config::STANDALONE_PROFILE,
                self.role_installation(Engine::Vllm, vllm.into())?,
            )],
            (None, Some(sglang)) => vec![named(
                crate::standalone_config::STANDALONE_PROFILE,
                self.role_installation(Engine::Sglang, sglang.into())?,
            )],
            (Some(vllm), Some(sglang)) => vec![
                named(
                    "local-vllm",
                    self.role_installation(Engine::Vllm, vllm.into())?,
                ),
                named(
                    "local-sglang",
                    self.role_installation(Engine::Sglang, sglang.into())?,
                ),
            ],
            (None, None) => Vec::new(),
        };
        for (name, profile) in registered {
            if all.iter().any(|n| &n.profile == name) {
                return Err(ProviderError::ProfileExists(name.clone()));
            }
            let engine = if profile["engine"] == "sglang" {
                Engine::Sglang
            } else {
                Engine::Vllm
            };
            let executable = PathBuf::from(profile["executable"].as_str().unwrap_or_default());
            let base = self.role_installation_as(
                engine,
                executable,
                Some(profile["build_fingerprint"].as_str().unwrap_or("unknown")),
                Some(profile["security"]["deep_park"].as_str() != Some("disabled")),
            )?;
            all.push(named(
                name,
                mllm_controller::engine_provider::from_profile(&base, profile),
            ));
        }
        if all.is_empty() {
            return Err(no_installation(format!(
                "this host declares no engine: set {ENGINE_BIN} or {SGLANG_BIN} to the \
                 engine's executable (and {MODELS_ROOT} to the directory its weights \
                 live under), or register one with `mllm engine add`"
            )));
        }
        Ok(all)
    }

    fn bindings(
        &self,
        clock: ServiceClock,
        log_dir: PathBuf,
        runtime_dir: PathBuf,
    ) -> Arc<dyn EngineBindings> {
        self.bindings_for_devices(clock, log_dir, runtime_dir, Default::default())
    }

    fn bindings_for_devices(
        &self,
        _clock: ServiceClock,
        log_dir: PathBuf,
        runtime_dir: PathBuf,
        device_totals: std::collections::BTreeMap<u32, i64>,
    ) -> Arc<dyn EngineBindings> {
        // ADR 0014 §7, Q9: the checkpoint stat cache is private host state,
        // kept beside the logs in the standalone state directory.
        let cache = log_dir.with_file_name("checkpoints");
        // SPEC §8.2 / T21 (owner decision 2026-09-25): SGLang launches keep
        // their file rendezvous in the private root the role created at start
        // (`<state>/rendezvous`, as on a host), never the entry's /tmp fallback.
        let bindings = ProfileBindings::new(log_dir.clone(), runtime_dir)
            .with_device_totals(device_totals)
            .with_checkpoint_cache(cache)
            .with_rendezvous_root(log_dir.with_file_name(RENDEZVOUS_DIR));
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
/// that does not contain it is not a runtime directory. SPEC §3.3 / ADR 0001
/// (owner decision 2026-09-24): by default it is the managed copy of the
/// runtime embedded in this binary, `<state root>/runtime`, written or
/// refreshed here; `MLLM_RUNTIME_DIR` names another (development), which is
/// never written to.
///
/// `deep_park` is the host's switch: with it on, a parking vLLM deployment
/// renders sleep mode, whose entry imports the capability probes (ADR 0008).
fn runtime_dir(
    engine: Engine,
    deep_park: bool,
    managed: Option<&Path>,
) -> Result<PathBuf, ProviderError> {
    let dir = match (env_value(RUNTIME_DIR), managed) {
        (Some(declared), _) => PathBuf::from(declared),
        (None, Some(managed)) => {
            crate::managed_runtime::prepare(managed)
                .map_err(|error| no_installation(format!("managed runtime directory: {error}")))?;
            managed.to_path_buf()
        }
        (None, None) => {
            return Err(no_installation(format!(
                "no runtime directory: set {RUNTIME_DIR} to mllm's runtime directory"
            )))
        }
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
    let dir = dir.canonicalize().map_err(|error| {
        no_installation(format!("runtime directory {}: {error}", dir.display()))
    })?;
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
    start_standalone_from(state_dir, None).await
}

/// As [`start_standalone`], with the role document named by `--config` when
/// `config` is given.
///
/// SPEC §15.2 (R13): an explicit document is the one that is honoured. A
/// missing or invalid explicit document refuses the boot; it is never replaced
/// by the generated default, and nothing is written under
/// `<state_dir>/config`. The state root is still `state_dir`, so a document
/// that names another `state_dir` is refused (SPEC §15.3).
pub async fn start_standalone_from(
    state_dir: &Path,
    config: Option<&Path>,
) -> Result<App, StartError> {
    start_standalone_inner(
        state_dir,
        config,
        Arc::new(EnvEngineProvider::with_managed_runtime(
            state_dir.join("runtime"),
        )),
        crate::host_observation::proc_meminfo(),
        // Design §1: the production boot samples the GPUs with the bounded
        // `nvidia-smi` collector. A machine without one samples nothing and
        // publishes the unified shape, exactly as before.
        Arc::new(mllm_agent::gpu_memory::sample),
    )
    .await
}

/// Boot against an explicit provider, which is how a test supplies an installation
/// it controls instead of one the environment happens to name.
///
/// The test entry points observe no GPU, so the published policy is the one a
/// test states rather than the one the machine running it happens to have;
/// [`start_standalone_with_gpu`] states a sample.
pub async fn start_standalone_with(
    state_dir: &Path,
    provider: Arc<dyn EngineProvider>,
) -> Result<App, StartError> {
    start_standalone_inner(
        state_dir,
        None,
        provider,
        crate::host_observation::proc_meminfo(),
        no_gpu(),
    )
    .await
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
    start_standalone_inner(state_dir, None, provider, memory, no_gpu()).await
}

/// As [`start_standalone_with_memory`], sampling the host's GPUs through `gpu`
/// instead of `nvidia-smi` (design §1).
pub async fn start_standalone_with_gpu(
    state_dir: &Path,
    provider: Arc<dyn EngineProvider>,
    memory: crate::host_observation::MemoryReader,
    gpu: Arc<GpuSampler>,
) -> Result<App, StartError> {
    start_standalone_inner(state_dir, None, provider, memory, gpu).await
}

/// The sampler of a boot that observes no GPU.
fn no_gpu() -> Arc<GpuSampler> {
    Arc::new(|| None)
}

/// As [`start_standalone_with_memory`], with an explicit role document
/// (`--config`); see [`start_standalone_from`].
pub async fn start_standalone_configured(
    state_dir: &Path,
    config: Option<&Path>,
    provider: Arc<dyn EngineProvider>,
    memory: crate::host_observation::MemoryReader,
) -> Result<App, StartError> {
    start_standalone_inner(state_dir, config, provider, memory, no_gpu()).await
}

async fn start_standalone_inner(
    state_dir: &Path,
    config: Option<&Path>,
    provider: Arc<dyn EngineProvider>,
    memory: crate::host_observation::MemoryReader,
    gpu: Arc<GpuSampler>,
) -> Result<App, StartError> {
    // Fail-closed credentials (SPEC §15.2): the generated api key lives in
    // the protected credentials file. The hardcoded fallback exists ONLY
    // for a boot that generated the config (and its credentials) this run
    // — an existing state dir missing its credentials refuses to serve
    // instead of serving with a guessable key.
    //
    // SPEC §15.2 (R13): an explicit `--config` that is missing or invalid is an
    // error here; it is never replaced by a generated default.
    let outcome = resolve_startup(ConfigKind::Standalone, config, state_dir)?;
    let mut created_this_boot = matches!(
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
            mllm_config::remote_roles::switch_drain_timeout(&document["server"]).map_err(
                |error| StartError::Deploy(format!("standalone configuration: {error}")),
            )?,
            // SPEC §17 (M80): `server.observability.timing_header`, off unless set.
            mllm_config::remote_roles::timing_header(&document["server"]).map_err(|error| {
                StartError::Deploy(format!("standalone configuration: {error}"))
            })?,
            ignored.iter().map(ToString::to_string).collect::<Vec<_>>(),
        )
    };
    let db_path = state_dir.join("server").join("srv.sqlite3");
    // SPEC §15.2: an explicit document is operator configuration, not state,
    // so it does not bring the credentials a generated default would. A state
    // root that has never served (no store, no credentials) gets them now, once,
    // exactly as a first implicit start would. A root that has served and lost
    // its credentials is not repaired: it refuses below (MissingCredentials).
    if config.is_some()
        && !db_path.try_exists()?
        && !state_dir
            .join("identity")
            .join("credentials")
            .try_exists()?
    {
        created_this_boot = mllm_config::defaults::create_standalone_credentials(state_dir)?;
    }
    let store = Rc::new(Store::open(&db_path)?);
    let api_key = match read_api_key(state_dir) {
        Some(k) => k,
        None if created_this_boot => "mllm-local".to_string(),
        None => return Err(StartError::MissingCredentials),
    };

    // Spec §8: what this host publishes about its engines is what it has. There
    // is no fallback installation: a host with none refuses to boot rather than
    // come up serving an engine nobody configured. ADR 0018 §5: the environment's
    // installations and the profiles registered in engines.yaml (beside
    // `--config`, else `<config home>/mllm/engines.yaml`); a name declared in
    // both is refused `profile_exists`. The standalone document is never written.
    let engines = crate::engine::role_engines(config, &|key| {
        std::env::var(key).ok().filter(|value| !value.is_empty())
    });
    let registered = match &engines {
        Some(path) => mllm_config::registration::EnginesFile::load(path)?.profiles,
        None => serde_json::Map::new(),
    };
    for (name, profile) in &registered {
        mllm_config::registration::check_profile(name, profile)?;
    }
    let named = provider.installations(&registered)?;
    // Role-level settings (runtime directory, ports, model store) are the same
    // for every installation; the first one states them.
    let installation = named
        .first()
        .map(|first| first.installation.clone())
        .ok_or_else(|| StartError::NoEngineInstallation("this host declares no engine".into()))?;
    let environment_fingerprint = format!("standalone-{}", installation.build_fingerprint);
    let capacity_bytes = memory()
        .map(|sample| sample.capacity_bytes)
        .map_err(|error| StartError::Deploy(format!("host capacity unreadable: {error}")))?;

    // Design §1: the GPUs are sampled once, and the shape they describe decides
    // the domains this host publishes. Integrated and discrete devices mixed on
    // one host are refused rather than guessed at. No sample (no `nvidia-smi`,
    // or a failed run) is a host with no GPU, which publishes as before.
    let gpu_sample = gpu();
    let gpu_shape =
        mllm_agent::gpu_memory::shape(gpu_sample.as_ref()).map_err(StartError::GpuTopology)?;

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
        gpu_sample.as_ref(),
    );

    // The host's own accounting units, resolved before anything can observe or be
    // admitted against them. The coordinator's observation source is named by these,
    // so it has to exist before the coordinator does.
    let declared_host = {
        let host = crate::standalone_config::host_policy(
            &named,
            &environment_fingerprint,
            capacity_bytes,
            inventory.as_ref(),
            &gpu_shape,
        );
        // Design §3: on a discrete host the probe is the discrete template,
        // sized for an empty checkpoint, on the GPU the picker would choose
        // first; its resolution states those zero weights.
        let (memory, facts) = match &gpu_shape {
            HostShape::Discrete(gpus) => (
                crate::standalone_config::discrete_template_memory(
                    gpus,
                    capacity_bytes,
                    Some(0),
                    None,
                )
                .ok_or_else(|| StartError::Deploy("host policy invalid: no GPU".into()))?,
                mllm_config::effective::CheckpointFacts {
                    weights_bytes: Some(0),
                    ..Default::default()
                },
            ),
            HostShape::Unified | HostShape::NoGpu => (
                crate::standalone_config::TemplateMemory::Unified { capacity_bytes },
                mllm_config::effective::CheckpointFacts::default(),
            ),
        };
        let probe = crate::standalone_config::deployment_document(
            "policy-probe",
            "policy-probe",
            &ModelSource::Local {
                path: "/dev/null".into(),
            },
            installation.engine,
            &memory,
            crate::standalone_config::DEFAULT_REQUEST_DEADLINE,
            installation.deep_park,
            &named[0].profile,
        )
        .map_err(|error| StartError::Deploy(format!("host policy invalid: {error}")))?;
        let probe = mllm_config::instances::device_choices(&probe, &host)
            .ok()
            .and_then(|choices| choices.into_iter().next())
            .map_or(probe, |(_, chosen)| chosen);
        mllm_config::effective::resolve_effective_with_checkpoint(&probe, &host, facts)
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
    // SPEC §8.2 / T21 (owner decision 2026-09-25): the private rendezvous root,
    // swept of directories no retained launch owns before any launch runs.
    {
        let retained = owner
            .lock()
            .map_err(|_| StartError::Deploy("ownership mutex poisoned".into()))?
            .store()
            .retained_incarnations()
            .map_err(|error| StartError::Deploy(format!("retained launches: {error}")))?;
        prepare_rendezvous_root(state_dir, &retained)?;
    }
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
    // ADR 0008 (owner decision 2026-09-23): register each installation (its
    // version and a digest over its files) as a host agent does at start; each
    // Initialize measures the one its profile names again for drift. Bounded,
    // reads files only, and a failure is `unmeasured`, never a refusal.
    let embedded = {
        let (named, fingerprint, inventory, shape) = (
            named.clone(),
            environment_fingerprint.clone(),
            inventory.clone(),
            gpu_shape.clone(),
        );
        tokio::task::spawn_blocking(move || {
            crate::standalone_engines::EmbeddedHost::new(
                named,
                fingerprint,
                capacity_bytes,
                inventory,
                shape,
            )
        })
        .await
        .map_err(|_| StartError::Deploy("installation registration did not finish".into()))?
    };
    let bindings: Arc<dyn EngineBindings> =
        Arc::new(mllm_controller::installation_gate::InstalledBindings::new(
            // Discrete GPU design §6: engines on a device domain are sized
            // against the total of the card the boot sample observed.
            provider.bindings_for_devices(
                system_clock(),
                state_dir.join("logs"),
                installation.runtime_dir.clone(),
                device_totals(&gpu_shape),
            ),
            embedded.installations(),
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
            // SPEC §7.2 / ADR 0019: host domains from host memory, each device
            // domain from its GPU; an unobserved device closes admission there.
            HostMemoryObservation::with_domains(crate::host_observation::observed_domains(
                &declared_host.domains,
            ))
            .with_memory_reader(memory.clone())
            .with_gpu_sampler(gpu.clone())
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
    let admin = credentials
        .lines()
        .find_map(|line| line.strip_prefix("admin_token: "))
        .ok_or(StartError::MissingCredentials)?;
    let management_credentials =
        mllm_management::ManagementCredentials::from_trusted_resolver(admin, &api_key)
            .map_err(|_| StartError::MissingCredentials)?;
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
    let configuration = Arc::new(
        mllm_management::configuration::SharedConfigurationSource::new_shared(
            owner,
            embedded.shared_document(),
            "standalone",
        )
        .map_err(|_| StartError::Deploy("management configuration unavailable".into()))?,
    );
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
        embedded.installations(),
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
    // ADR 0018 §4, §5: removal retires through the store, as a server does.
    let retirements = Arc::new(mllm_management::engines::StoreRetirements::new(
        source.clone(),
    ));
    let management = mllm_management::lifecycle_router(management_credentials, source)
        .merge(drain)
        .merge(installation_view)
        .merge(latency_view);
    let controller =
        Arc::new(CoordinatorLifecycle::new(coordinator.commands()).with_switcher(switcher.clone()));
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
        let observations = HostMemoryObservation::with_domains(
            crate::host_observation::observed_domains(&declared_host.domains),
        )
        .with_memory_reader(memory.clone())
        .with_gpu_sampler(gpu.clone())
        .observe(declared_host.name.clone())
        .await
        .map_err(|error| StartError::Deploy(error.to_string()))?;
        controller
            .publish_resource_policy(&declared_host, &observations)
            .map_err(|error| StartError::Deploy(error.to_string()))?;
    }
    // ADR 0018 §4, §5 (review decisions I2, I3): standalone keeps a removed
    // profile out of placement and never lets an abandoned retirement wedge a
    // name, as a server does: expiry at start and while running, and the
    // embedded host's profiles recorded as its publication.
    crate::standalone_engines::publish_at_start(
        &coordinator.commands(),
        &declared_host.name,
        &embedded.profiles(),
    )
    .map_err(StartError::Deploy)?;
    supervision.supervise(tokio::spawn(crate::standalone_engines::expire_retirements(
        coordinator.commands(),
        supervision.cancel_signal(),
    )));
    // SPEC §10 step 1, §16.2 (W10 gap b): waiting requests are bounded by the
    // embedded host's published `resource_policy.queue`.
    if let Some(queue) = controller
        .queue_policy()
        .map_err(|error| StartError::Deploy(error.to_string()))?
    {
        deps.inflight
            .waiting
            .set_limits(crate::remote_roles::wait_limits(&queue));
    }
    // ADR 0018 §3, §5: the local control channel `mllm engine add` and
    // `remove` use, bound only inside the role's 0700 state directory. A
    // socket that cannot be bound (unsafe directory, path too long, another
    // role, no engines file) is reported and the role runs without it, so
    // `engine add` answers `agent_unreachable` and takes effect at start.
    let mut config_notices = config_notices;
    match &engines {
        None => config_notices.push(
            "engine control socket not served: neither XDG_CONFIG_HOME nor HOME is set, \
             so there is no engines.yaml to reload"
                .into(),
        ),
        Some(engines) => {
            let socket = state_dir.join(mllm_agent::control_socket::SOCKET_NAME);
            match mllm_agent::control_socket::ControlServer::bind(&socket) {
                Ok(server) => {
                    let handler = crate::standalone_engines::StandaloneControl::new(
                        engines.clone(),
                        provider.clone(),
                        embedded.clone(),
                        retirements,
                        coordinator.commands(),
                        declared_host.name.clone(),
                    );
                    // SAFETY: geteuid has no preconditions and cannot fail.
                    let uid = unsafe { libc::geteuid() };
                    // Supervised: it stops, and removes its file, with the role.
                    supervision.supervise(tokio::spawn(server.serve(
                        handler,
                        uid,
                        supervision.cancel_signal(),
                    )));
                }
                Err(failure) => config_notices.push(format!(
                    "engine control socket unavailable ({failure}); `mllm engine add` \
                     takes effect when the role restarts"
                )),
            }
        }
    }
    Ok(App {
        controller,
        _coordinator: coordinator,
        store,
        host: embedded,
        capacity_bytes,
        gpu_shape,
        router,
        management,
        deps,
        api_key,
        supervision,
        switcher,
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

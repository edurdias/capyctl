//! Role wiring for standalone: `start standalone` boots the embedded
//! server+host graph in-process against the engine installation this host
//! actually has, and returns an [`App`] handle over the controller and the
//! durable store. Every other parsed action still reports a structured
//! not-yet-implemented diagnostic.
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
    deps: mllm_router::RouterDeps,
    api_key: String,
}

impl App {
    pub fn router(&self) -> axum::Router {
        self.router.clone()
    }

    pub fn deps(&self) -> &mllm_router::RouterDeps {
        &self.deps
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

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
        let deployment = crate::standalone_config::deployment_document(
            name,
            name,
            &source,
            self.installation.engine,
            self.capacity_bytes,
            request_deadline,
        );
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
        // SPEC §9.1 / T21: experimental controls require explicit host opt-in.
        // Sleep mode follows the same permission as deep parking.
        let deep_park = env_value(DEEP_PARK).is_some_and(|value| value == "on");
        let trust_remote_code = env_value(TRUST_REMOTE_CODE).is_some_and(|value| value == "1");
        let build_fingerprint = match env_value(ENGINE_FINGERPRINT) {
            Some(declared) => declared,
            None => probe_fingerprint(&executable)?,
        };
        let kv_cache_bytes =
            env_value(KV_CACHE_BYTES).unwrap_or_else(|| DEFAULT_KV_CACHE.to_string());
        // Spec §7: the whole family block, not a fragment. The pinned SGLang
        // recipe takes no profile flags (`engine_policy.rs` refuses any arg on
        // that family) and sizes its pool through the requested budget.
        let (args, launch_settings) = match engine {
            Engine::Vllm => (
                env_value(ENGINE_ARGS)
                    .unwrap_or_else(|| DEFAULT_ENGINE_ARGS.to_string())
                    .split(' ')
                    .filter(|argument| !argument.is_empty())
                    .map(str::to_owned)
                    .collect(),
                // The utilization gate is set low because the explicit KV grant
                // is what sizes the pool, and the gate must still pass when the
                // previous deployment's memory has not yet been released by the
                // operating system.
                serde_json::json!({
                    "engine": "vllm",
                    "tensor_parallel_size": 1,
                    "pipeline_parallel_size": 1,
                    "enable_sleep_mode": deep_park,
                    "kv_cache_dtype": "auto",
                    "block_size_tokens": 16,
                    "cpu_offload_bytes": "0B",
                    "requested_budget": {
                        "kv_cache_bytes": kv_cache_bytes,
                        "swap_space_bytes": "0B",
                        "gpu_utilization_pct": 10
                    }
                }),
            ),
            Engine::Sglang => (
                Vec::new(),
                // The pinned recipe's whole shape, not a fragment: the frozen
                // contract refuses any field that drifts from the recipe it was
                // written against, so the template carries every field the
                // contract validates (found live: an omitted memory_saver made
                // every SGLang launch fail at the frozen-shape check, and the
                // old error mapping reported it as a family mismatch).
                serde_json::json!({
                    "engine": "sglang",
                    "recipe": mllm_config::effective::sglang::NATIVE_SGLANG_RECIPE,
                    "tensor_parallel_size": 1,
                    "data_parallel_size": 1,
                    "tokenizer_workers": 1,
                    "model_dtype": "bfloat16",
                    "context_tokens": 4096,
                    "max_running_requests": 8,
                    "max_total_tokens": 4096,
                    "prefill_cuda_graphs": false,
                    "decode_cuda_graphs": false,
                    "memory_saver": true,
                    "cpu_weight_backup": false,
                    "speculative_decoding": false,
                    "lora": false,
                    "trust_remote_code": trust_remote_code,
                    "disaggregation": false,
                    "external_cache": false,
                    "cpu_kv_offload": false,
                    "native_grpc": false,
                    "weight_restore": "disk_reload",
                    "requested_budget": {
                        "kv_cache_bytes": kv_cache_bytes,
                        "static_memory_fraction_bps": 7500
                    }
                }),
            ),
        };
        Ok(EngineInstallation {
            engine,
            executable,
            // Spec §7: the whole family block, not a fragment.
            build_fingerprint,
            launch_settings,
            deep_park,
            trust_remote_code,
            models_root,
            runtime_dir: runtime_dir()?,
            args,
        })
    }

    fn bindings(
        &self,
        _clock: ServiceClock,
        log_dir: PathBuf,
        runtime_dir: PathBuf,
    ) -> Arc<dyn EngineBindings> {
        Arc::new(ProfileBindings::new(log_dir, runtime_dir))
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
fn runtime_dir() -> Result<PathBuf, ProviderError> {
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
    // The protected wrapper's validator refuses a path whose canonical form
    // differs from itself, so a runtime directory named through `..` is
    // canonicalized here, where the installation is resolved — a launch would
    // otherwise fail at render with a refusal this boot could have prevented
    // (found live: the default CARGO_MANIFEST_DIR-relative directory carries
    // `..` and every SGLang launch was refused before spawning).
    dir.canonicalize()
        .map_err(|error| no_installation(format!("runtime directory {}: {error}", dir.display())))
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
    start_standalone_inner(state_dir, Arc::new(EnvEngineProvider::new())).await
}

/// Boot against an explicit provider, which is how a test supplies an installation
/// it controls instead of one the environment happens to name.
pub async fn start_standalone_with(
    state_dir: &Path,
    provider: Arc<dyn EngineProvider>,
) -> Result<App, StartError> {
    start_standalone_inner(state_dir, provider).await
}

/// Compatibility shim for the owner's untracked live test, which this branch may
/// not edit.
///
/// The deep-park policy is no longer a boot argument: Spec §3 makes it the host's
/// switch, read from `MLLM_DEEP_PARK` with the rest of the installation, so the
/// argument is accepted and ignored. Remove this together with the test that calls
/// it.
pub async fn start_standalone_with_policy(
    state_dir: &Path,
    _policy: mllm_adapters::ParkPolicy,
) -> Result<App, StartError> {
    start_standalone(state_dir).await
}

/// Compatibility shim for the same test: the qualification profile it reads from
/// the environment.
///
/// Standalone no longer builds an adapter from these — the coordinator resolves one
/// per binding from the frozen profile, and the installation comes from
/// [`EnvEngineProvider`]. Nothing in the product reads this type; it exists so the
/// owner's live test keeps compiling, and it goes when that test is updated.
#[derive(Debug, Clone)]
pub struct LiveVllmProfile {
    pub engine_bin: PathBuf,
    /// Extra PATH entries the engine needs at runtime (the venv's bin directory).
    pub engine_path_extra: Option<PathBuf>,
    pub model_path: String,
    pub model_id: String,
    pub port: u16,
    pub fingerprint: String,
}

impl LiveVllmProfile {
    /// Read from the qualification environment, or `None` when it names no profile.
    pub fn from_env() -> Option<Self> {
        Some(Self {
            engine_path_extra: env_value("MLLM_ENGINE_PATH").map(PathBuf::from),
            engine_bin: PathBuf::from(env_value(ENGINE_BIN)?),
            model_path: env_value("MLLM_MODEL_PATH")?,
            model_id: env_value("MLLM_MODEL_ID")?,
            port: env_value("MLLM_PORT")?.parse().ok()?,
            fingerprint: env_value(ENGINE_FINGERPRINT).unwrap_or_else(|| "live-capture".into()),
        })
    }
}

async fn start_standalone_inner(
    state_dir: &Path,
    provider: Arc<dyn EngineProvider>,
) -> Result<App, StartError> {
    // Fail-closed credentials (SPEC §15.2): the generated api key lives in
    // the protected credentials file. The hardcoded fallback exists ONLY
    // for a boot that generated the config (and its credentials) this run
    // — an existing state dir missing its credentials refuses to serve
    // instead of serving with a guessable key.
    let created_this_boot = matches!(
        resolve_startup(ConfigKind::Standalone, None, state_dir)?,
        LoadOutcome::Generated {
            created_identity: true,
            ..
        }
    );
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
    let capacity_bytes = mllm_agent::memory::read_host_memory()
        .map(|sample| sample.memory.capacity_bytes)
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
        // healthy engine mid-load.
        initialize_timeout: Duration::from_secs(900),
        // Spec §5: what a terminated process group is given before it is killed.
        terminate_grace: Duration::from_secs(15),
        ..Default::default()
    };
    let bindings = provider.bindings(
        system_clock(),
        state_dir.join("logs"),
        installation.runtime_dir.clone(),
    );
    let coordinator = OwnedCoordinator::spawn_resolved(
        owner,
        Arc::new(HostMemoryObservation::new(
            declared_host.domains.keys().cloned(),
        )),
        system_clock(),
        options,
        bindings,
        provider.tools_factory(),
    )?;
    let controller = Arc::new(CoordinatorLifecycle::new(coordinator.commands()));
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
        inflight: Arc::new(mllm_router::admission::InFlight::default()),
        activation_join: Arc::new(mllm_router::WakeJoin::new()),
    };
    let router = mllm_router::serve_router(deps.clone());
    // Publish the ceiling before anything can be admitted against it. A deployment
    // cannot be qualified until the host has said what it will allow, and the policy
    // is imported with the observation that justifies it rather than on its own.
    {
        // The same source the coordinator will observe through, so the policy and
        // the evidence for it cannot disagree about what a domain is called.
        let observations = HostMemoryObservation::new(declared_host.domains.keys().cloned())
            .observe(declared_host.name.clone())
            .await
            .map_err(|error| StartError::Deploy(error.to_string()))?;
        controller
            .publish_resource_policy(&declared_host, &observations)
            .map_err(|error| StartError::Deploy(error.to_string()))?;
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
        deps,
        api_key,
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

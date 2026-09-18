//! Role wiring for the F0 exit gate: `start standalone` boots the
//! embedded server+host graph in-process (no enrollment, no listeners —
//! F3 wires real transports) and returns an [`App`] handle over the
//! controller and the durable store. Every other parsed action still
//! reports a structured not-yet-implemented diagnostic.

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::collections::HashMap;
use std::sync::Arc;

use mllm_agent::Host;
use mllm_config::defaults::{resolve_startup, LoadOutcome};
use mllm_config::schema::ConfigKind;
use mllm_controller::coordinator::{CoordinatorOptions, OwnedCoordinator, ServiceObservation as _};
use mllm_controller::{CoordinatorLifecycle, OwnedCoordinatorState, ProfileBindings};
use crate::host_observation::{system_clock, HostMemoryObservation};
use mllm_store::Store;

use crate::grammar::Command;
use crate::output::{ExitCode, StructuredError};

pub const NOT_IMPLEMENTED_EXIT: ExitCode = ExitCode::UNSUPPORTED;

/// A booted standalone deployment graph: the controller operation engine
/// plus the durable store it runs against. Tests and the CLI drive
/// lifecycle through `controller`; the store is a separate connection to
/// the same WAL-backed file for direct observation. It is an `Rc`
/// (not `Arc`) because `Store` is not `Sync` and F0 observes it from one
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
    /// The engine installation this host published, and the capacity its limits were
    /// derived from. Deployments are qualified against this, so it is kept rather
    /// than recomposed per request — recomposing risks declaring one thing at boot
    /// and a different thing at deploy.
    engine_declaration: (String, String, bool, i64),
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
    pub fn deploy(&self, name: &str, model_path: &str) -> Result<String, StartError> {
        let (engine, executable, experimental, capacity) = &self.engine_declaration;
        let host = crate::standalone_config::host_policy(
            engine,
            executable,
            "standalone-1",
            *experimental,
            *capacity,
        );
        let deployment =
            crate::standalone_config::deployment_document(name, name, model_path, *capacity);
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
    #[error("controller ownership: {0}")]
    Ownership(#[from] mllm_controller::OwnedStateError),
    #[error("coordinator: {0}")]
    Coordinator(#[from] mllm_controller::coordinator::CoordinatorError),
    #[error("deploy: {0}")]
    Deploy(String),
}

impl From<StartError> for StructuredError {
    fn from(err: StartError) -> Self {
        let code = match err {
            StartError::Config(_) => "invalid_config",
            _ => "internal",
        };
        StructuredError {
            code,
            message: err.to_string(),
        }
    }
}

/// Boot the embedded standalone graph (SPEC §15.2 no-config matrix):
/// resolve the startup config (generating the standalone default on
/// first start), open the server store, and run the embedded host
/// (fake engine + fake launcher, deep-park policy denied by default)
/// against a controller bound to the same state directory.
pub async fn start_standalone(state_dir: &Path) -> Result<App, StartError> {
    start_standalone_inner(state_dir, mllm_adapters::fake::ParkPolicy::Disabled).await
}

/// Boot with an explicit host deep-park policy (the Spark qualification
/// flow opts in for the isolated experimental profile).
pub async fn start_standalone_with_policy(
    state_dir: &Path,
    policy: mllm_adapters::fake::ParkPolicy,
) -> Result<App, StartError> {
    start_standalone_inner(state_dir, policy).await
}

type AdapterParts = (
    Arc<dyn mllm_adapters::traits::EngineAdapter>,
    Arc<dyn mllm_adapters::traits::Launcher>,
    HashMap<String, Arc<dyn mllm_adapters::traits::ChatForward>>,
);

/// Live vLLM profile for the Spark qualification (F1 design §8): the
/// standalone role drives the REAL vLLM adapter over the REAL exec
/// launcher. F1 qualification wiring via documented env vars; F2 replaces
/// this with the profile schema.
#[derive(Debug, Clone)]
pub struct LiveVllmProfile {
    pub engine_bin: PathBuf,
    /// Extra PATH entries the engine needs at runtime (venv bin: e.g. the
    /// JIT compile step needs the venv's `ninja`).
    pub engine_path_extra: Option<PathBuf>,
    pub model_path: String,
    pub model_id: String,
    pub port: u16,
    pub fingerprint: String,
}

impl LiveVllmProfile {
    /// Read from the qualification environment (documented in the runbook).
    pub fn from_env() -> Option<Self> {
        let engine_bin = std::env::var("MLLM_VLLM_BIN").ok()?;
        Some(Self {
            engine_path_extra: std::env::var("MLLM_ENGINE_PATH").ok().map(PathBuf::from),
            engine_bin: engine_bin.into(),
            model_path: std::env::var("MLLM_MODEL_PATH").ok()?,
            model_id: std::env::var("MLLM_MODEL_ID").ok()?,
            port: std::env::var("MLLM_PORT").ok()?.parse().ok()?,
            fingerprint: std::env::var("MLLM_ENGINE_FINGERPRINT").unwrap_or_else(|_| "live-capture".into()),
        })
    }
}

async fn start_standalone_inner(
    state_dir: &Path,
    policy: mllm_adapters::fake::ParkPolicy,
) -> Result<App, StartError> {
    // Fail-closed credentials (SPEC §15.2): the generated api key lives in
    // the protected credentials file. The hardcoded fallback exists ONLY
    // for a boot that generated the config (and its credentials) this run
    // — an existing state dir missing its credentials refuses to serve
    // instead of serving with a guessable key.
    let created_this_boot = matches!(
        resolve_startup(ConfigKind::Standalone, None, state_dir)?,
        LoadOutcome::Generated { created_identity: true, .. }
    );
    let db_path = state_dir.join("server").join("srv.sqlite3");
    let store = Rc::new(Store::open(&db_path)?);
    let api_key = match read_api_key(state_dir) {
        Some(k) => k,
        None if created_this_boot => "mllm-local".to_string(),
        None => return Err(StartError::MissingCredentials),
    };

    // Live profile: the REAL vLLM adapter + REAL exec launcher (F1 design
    // §8; the profile carries the pinned build's fingerprint).
    let (adapter, launcher, forwards): AdapterParts = match LiveVllmProfile::from_env() {
        Some(p) => {
            let base: reqwest::Url = format!("http://127.0.0.1:{}", p.port)
                .parse()
                .map_err(|e| StartError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;
            let launch = mllm_adapters::vllm::args::PlanInputVllm {
                engine_bin: p.engine_bin.to_string_lossy().to_string(),
                model_path: p.model_path.clone(),
                port: p.port,
                // Spec §3: mllm renders --served-model-name itself now
                // (the qualification args below no longer carry it).
                served_model_name: p.model_id.clone(),
                tensor_parallel_size: 1,
                pipeline_parallel_size: 1,
                kv_cache_dtype: "auto".into(),
                block_size_tokens: 16,
                cpu_offload_bytes: 0,
                // Conservative KV grant for the unified-memory Spark: the
                // ledger's deployment budget bounds the engine's KV pool
                // (activation peak stays well inside the managed limit).
                granted: mllm_adapters::vllm::args::GrantedBudget {
                    // 16 GiB KV grant: the qualification drives 8-token
                    // completions, so a large pool is not needed — and a
                    // smaller grant keeps two engine instances from
                    // overcommitting the 130 GiB unified domain during the
                    // stop→start overlap of a switch (freeze observed live
                    // with 64 GiB, 2026-09-12).
                    kv_cache_bytes: Some(16 * 1024 * 1024 * 1024),
                    // The utilization gate must pass when the OS has not yet
                    // fully released the previous deployment's memory: the
                    // explicit KV grant sizes the pool (vLLM 0.29 live
                    // capture), so the utilization gate is set low.
                    gpu_utilization_pct: Some(10),
                    ..Default::default()
                },
                engine_path_extra: p.engine_path_extra.clone().map(|p| p.to_string_lossy().to_string()),
                engine_log: Some(
                    state_dir
                        .join("engine.log")
                        .to_string_lossy()
                        .to_string(),
                ),
                // The served model id must match the deployment's route id
                // (readiness = /v1/models lists the served id); vLLM
                // otherwise serves the checkpoint filesystem path.
                engine_args: vec![
                    "--host".into(),
                    "127.0.0.1".into(),
                    "--served-model-name".into(),
                    p.model_id.clone(),
                    // Cap the context: vLLM's startup check requires the
                    // model's max context to fit the KV pool — the model's
                    // 262K default would demand far more than the grant.
                    // The qualification drives 8-token completions.
                    "--max-model-len".into(),
                    "4096".into(),
                ],
                // Development/sleep flags render only under the opt-in
                // (profile-level gate, F1 design §7): the isolated
                // experimental session boots with sleep mode enabled.
                sleep_flags: live_vllm_sleep_flags(policy),
                api_key: None,
                runtime_dir: std::env::var("MLLM_RUNTIME_DIR").ok(),
            };
            let adapter = Arc::new(
                mllm_adapters::vllm::VllmAdapter::new(
                    base,
                    None,
                    p.fingerprint,
                    policy,
                    p.model_id.clone(),
                )
                .with_launch(launch),
            );
            let launcher: Arc<dyn mllm_adapters::traits::Launcher> =
                Arc::new(mllm_launchers::ExecLauncher::new());
            let fwd = adapter.clone() as Arc<dyn mllm_adapters::traits::ChatForward>;
            (
                adapter.clone() as Arc<dyn mllm_adapters::traits::EngineAdapter>,
                launcher,
                // Dispatch resolves by deployment kind; stock and sleep
                // profiles share this adapter. Keep the model-id alias for
                // direct adapter qualification as well.
                HashMap::from([
                    ("model".to_string(), fwd.clone()),
                    ("vllm-sleep".to_string(), fwd.clone()),
                    (p.model_id.clone(), fwd),
                ]),
            )
        }
        None => {
            let fake = Arc::new(mllm_adapters::fake::FakeEngine::new());
            let host = Host::new();
            let fwd = fake.clone() as Arc<dyn mllm_adapters::traits::ChatForward>;
            (
                host.adapter(),
                host.launcher(),
                HashMap::from([
                    ("model".to_string(), fwd),
                    (
                        "attached".to_string(),
                        Arc::new(mllm_router::chat::NoForward)
                            as Arc<dyn mllm_adapters::traits::ChatForward>,
                    ),
                ]),
            )
        }
    };

    // What this host publishes about its engine. Derived from the live profile when
    // one is configured, otherwise the embedded fake, and from observed capacity
    // rather than a configured guess.
    let engine_declaration = {
        let (engine, executable) = match LiveVllmProfile::from_env() {
            Some(p) => ("vllm".to_string(), p.engine_bin.to_string_lossy().to_string()),
            None => ("fake".to_string(), "/bin/true".to_string()),
        };
        let capacity = mllm_agent::memory::read_host_memory()
            .map(|sample| sample.memory.capacity_bytes)
            .map_err(|error| StartError::Deploy(format!("host capacity unreadable: {error}")))?;
        (
            engine,
            executable,
            policy == mllm_adapters::fake::ParkPolicy::Enabled,
            capacity,
        )
    };
    // The host's own accounting units, resolved before anything can observe or be
    // admitted against them. The coordinator's observation source is named by these,
    // so it has to exist before the coordinator does.
    let declared_host = {
        let (engine, executable, experimental, capacity) = &engine_declaration;
        let host = crate::standalone_config::host_policy(
            engine,
            executable,
            "standalone-1",
            *experimental,
            *capacity,
        );
        let probe = crate::standalone_config::deployment_document(
            "policy-probe",
            "policy-probe",
            "/dev/null",
            *capacity,
        );
        mllm_config::effective::resolve_effective(&probe, &host)
            .map_err(|error| StartError::Deploy(format!("host policy invalid: {error}")))?
            .host
    };

    // The coordinator opens the durable state itself and holds the controller lock
    // for as long as it runs, so nothing else may act as an authority over it.
    let owner = Arc::new(std::sync::Mutex::new(OwnedCoordinatorState::open(
        &state_dir.join("server"),
    )?));
    let coordinator = OwnedCoordinator::spawn_resolved(
        owner,
        Arc::new(HostMemoryObservation::new(
            declared_host.domains.keys().cloned(),
        )),
        system_clock(),
        CoordinatorOptions::default(),
        Arc::new(ProfileBindings::new(system_clock())),
    )?;
    let controller = Arc::new(CoordinatorLifecycle::new(coordinator.commands()));
    // The adapter and launcher built above now inform only the forwarding table;
    // the coordinator resolves an adapter per binding from its frozen profile.
    let _ = (adapter, launcher);
    let deps = mllm_router::RouterDeps {
        controller: controller.clone(),
        forwards,
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
    Ok(App { controller, _coordinator: coordinator, store, engine_declaration, router, deps, api_key })
}

fn live_vllm_sleep_flags(policy: mllm_adapters::fake::ParkPolicy) -> Vec<String> {
    if policy == mllm_adapters::fake::ParkPolicy::Enabled {
        // The deep-park lab profile uses eager checkpoint loading to avoid
        // mmap-backed tensor copies during weight restoration on Spark.
        vec![
            "--enable-sleep-mode".into(),
            "--safetensors-load-strategy".into(),
            "eager".into(),
        ]
    } else {
        Vec::new()
    }
}

/// Read the generated API key from the protected credentials file (F0's
/// fail-closed generation; the key is printed never, only used).
fn read_api_key(state_dir: &Path) -> Option<String> {
    let creds = std::fs::read_to_string(state_dir.join("identity").join("credentials")).ok()?;
    creds
        .lines()
        .find_map(|l| l.strip_prefix("api_key: ").map(str::to_string))
}

pub fn dispatch(command: &Command) -> Result<Infallible, StructuredError> {
    Err(StructuredError::not_yet_implemented(&command.label()))
}

#[cfg(test)]
mod tests {
    use super::live_vllm_sleep_flags;
    use mllm_adapters::fake::ParkPolicy;

    #[test]
    fn deep_park_lab_profile_enables_eager_weight_loading() {
        assert_eq!(
            live_vllm_sleep_flags(ParkPolicy::Enabled),
            ["--enable-sleep-mode", "--safetensors-load-strategy", "eager"],
        );
    }

    #[test]
    fn denied_profile_has_no_sleep_or_loader_override() {
        assert!(live_vllm_sleep_flags(ParkPolicy::Disabled).is_empty());
    }
}

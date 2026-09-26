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
use mllm_config::listener_migration::{Migration, NEW_DEFAULT as NEW_INFERENCE_DEFAULT};
pub use mllm_config::model_settings::ModelOverrides;
use mllm_config::schema::ConfigKind;
pub use mllm_config::setting_overrides::SettingOverrides;
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
/// two (or `--vllm-bin` / `--sglang-bin`, or `host.local_engine`), or a
/// profile registered with `mllm engine add`, must name one. ADR 0018 §5:
/// both set publish two profiles, `local-vllm` and `local-sglang`.
const ENGINE_BIN: &str = mllm_config::engine_settings::VLLM_BIN_ENV;
const SGLANG_BIN: &str = mllm_config::engine_settings::SGLANG_BIN_ENV;
/// The directory model weights live under (Spec §7). Optional (owner decision
/// 2026-09-25): `~/models` unless `--models-root`, this variable or
/// `host.model_store.path` names another.
const MODELS_ROOT: &str = mllm_config::model_settings::MODELS_ROOT_ENV;
const KV_CACHE_BYTES: &str = mllm_config::engine_settings::KV_CACHE_ENV;
const ENGINE_FINGERPRINT: &str = mllm_config::engine_settings::ENGINE_FINGERPRINT_ENV;
const RUNTIME_DIR: &str = mllm_config::engine_settings::RUNTIME_DIR_ENV;
/// SPEC §15.2: a run-time override of the engines' loopback port range, as
/// `start-end`, so two roles on one machine lease different engine ports
/// (owner rule 2026-09-25: one name for both roles; the standalone-only
/// `MLLM_STANDALONE_ENGINE_PORTS` is a deprecated alias).
pub const ENGINE_PORTS_ENV: &str = mllm_config::engine_settings::ENGINE_PORTS_ENV;
pub use mllm_config::engine_settings::EngineOverrides;
/// SPEC §16.5 default engine port range.
const DEFAULT_ENGINE_PORTS: (u16, u16) = mllm_config::engine_settings::DEFAULT_ENGINE_PORTS;

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
    /// Design §9: the document's `server.listeners.inference.bind`, or the
    /// `0.0.0.0:8443` default when it states none.
    inference_bind: std::net::SocketAddr,
    /// Owner decision 2026-09-25: the document's
    /// `server.listeners.management.bind`, or `127.0.0.1:7443`.
    management_bind: std::net::SocketAddr,
    /// ADR 0019, design §9: what this start's one-time listener migration did.
    listener_migration: Migration,
    /// Design §9: the document's `server.listeners.inference.authentication`
    /// (`api_key` unless it states `none`).
    inference_auth: mllm_config::standalone::InferenceAuth,
    /// Design §9: the listener the role serves, reported to status.
    inference_listener: Arc<mllm_management::inference_listener::InferenceListenerView>,
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

    /// Design §9: the inference bind the standalone document states (default
    /// `0.0.0.0:8443`). The listener binds it unless `--listen` or
    /// `MLLM_INFERENCE_ADDR` overrides it
    /// ([`effective_inference_address`]).
    pub fn inference_bind(&self) -> std::net::SocketAddr {
        self.inference_bind
    }

    /// Owner decision 2026-09-25: the management bind the standalone document
    /// states (default `127.0.0.1:7443`), unless `--management-listen` or
    /// `MLLM_MANAGEMENT_ADDR` overrides it ([`management_override`]).
    pub fn management_bind(&self) -> std::net::SocketAddr {
        self.management_bind
    }

    /// ADR 0019, design §9: what this start's one-time migration of the old
    /// loopback inference bind did (reported on stderr as it happened).
    pub fn listener_migration(&self) -> &Migration {
        &self.listener_migration
    }

    /// Design §9: the inference authentication the standalone document
    /// states (`api_key` unless `none`). `--no-inference-auth` and
    /// `MLLM_INFERENCE_AUTH` override it for one run
    /// ([`crate::exposure::effective_inference_auth`]).
    pub fn inference_auth(&self) -> mllm_config::standalone::InferenceAuth {
        self.inference_auth
    }

    /// Design §9: the router to serve on `bind` with `auth` for this run, and
    /// the listener recorded for status. With [`InferenceAuth::None`] the
    /// router's key check is off (`RouterDeps.api_key: None`); with
    /// [`InferenceAuth::ApiKey`] it is [`App::router`], which requires the
    /// key on every route (SPEC §13.3, T37).
    ///
    /// [`InferenceAuth::None`]: mllm_config::standalone::InferenceAuth::None
    /// [`InferenceAuth::ApiKey`]: mllm_config::standalone::InferenceAuth::ApiKey
    pub fn inference_router(
        &self,
        bind: std::net::SocketAddr,
        auth: mllm_config::standalone::InferenceAuth,
    ) -> axum::Router {
        use mllm_config::standalone::InferenceAuth;
        self.inference_listener
            .set(crate::exposure::listener_view(bind, auth));
        match auth {
            InferenceAuth::ApiKey => self.router.clone(),
            InferenceAuth::None => mllm_router::serve_router(mllm_router::RouterDeps {
                api_key: None,
                ..self.deps.clone()
            }),
        }
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

/// The standalone listeners' addresses, each overridable for one run so two
/// roles can share a machine (SPEC §15.2: a run-time override of an ordinary
/// setting). Management stays on loopback (SPEC §16.5), so a management
/// address that is not loopback is refused. Inference follows the document's
/// rule (design §9): any unicast address with a non-zero port.
///
/// Design §9: one variable moves the inference listener of either role, the
/// server's and the standalone one's alike.
pub const INFERENCE_ADDR_ENV: &str = "MLLM_INFERENCE_ADDR";
/// The standalone-only name [`INFERENCE_ADDR_ENV`] replaces. Still read, after
/// it, with a deprecation warning.
pub const DEPRECATED_INFERENCE_ADDR_ENV: &str = "MLLM_STANDALONE_INFERENCE_ADDR";
/// Owner decision 2026-09-25: the standalone management address for one
/// run, for the role and its client commands alike (loopback only).
pub const MANAGEMENT_ADDR_ENV: &str = "MLLM_MANAGEMENT_ADDR";
/// The standalone-only name [`MANAGEMENT_ADDR_ENV`] replaces. Still read,
/// after it, with a deprecation warning.
pub const DEPRECATED_MANAGEMENT_ADDR_ENV: &str = "MLLM_STANDALONE_MANAGEMENT_ADDR";

/// SPEC §16.5, owner decision 2026-09-25: the run-time override of the
/// standalone management bind, if any: `--management-listen`, then
/// `MLLM_MANAGEMENT_ADDR`, then the deprecated
/// `MLLM_STANDALONE_MANAGEMENT_ADDR`. Each must be a loopback address with a
/// non-zero port; a bad one is refused with its name.
pub fn management_override(
    flag: Option<std::net::SocketAddr>,
) -> Result<Option<std::net::SocketAddr>, StartError> {
    use mllm_config::standalone::management_address;
    if let Some(address) = flag {
        return management_address(&address.to_string())
            .map(Some)
            .ok_or_else(|| {
                StartError::Setting(format!(
                    "--management-listen {address} must be a loopback address with a non-zero port"
                ))
            });
    }
    let (variable, value) = match std::env::var_os(MANAGEMENT_ADDR_ENV) {
        Some(value) => (MANAGEMENT_ADDR_ENV, value),
        None => match std::env::var_os(DEPRECATED_MANAGEMENT_ADDR_ENV) {
            Some(value) => (DEPRECATED_MANAGEMENT_ADDR_ENV, value),
            None => return Ok(None),
        },
    };
    value
        .into_string()
        .ok()
        .and_then(|text| management_address(&text))
        .map(Some)
        .ok_or_else(|| {
            StartError::Setting(format!(
                "{variable} must be a loopback address with a port, e.g. 127.0.0.1:7443"
            ))
        })
}

/// The warning a role prints once at start when the deprecated
/// `MLLM_STANDALONE_MANAGEMENT_ADDR` is set, or `None`.
pub fn deprecated_management_env_warning(flag: Option<std::net::SocketAddr>) -> Option<String> {
    std::env::var_os(DEPRECATED_MANAGEMENT_ADDR_ENV)?;
    let ignored = flag.is_some() || std::env::var_os(MANAGEMENT_ADDR_ENV).is_some();
    Some(format!(
        "warning: {DEPRECATED_MANAGEMENT_ADDR_ENV} is deprecated; use {MANAGEMENT_ADDR_ENV}{}",
        if ignored {
            " (ignored for this run: --management-listen or MLLM_MANAGEMENT_ADDR is set)"
        } else {
            ""
        }
    ))
}

/// Design §9: the run-time override of the inference bind, if any, for the
/// server and standalone roles alike. Precedence: `--listen`, then
/// `MLLM_INFERENCE_ADDR`, then the deprecated `MLLM_STANDALONE_INFERENCE_ADDR`
/// ([`deprecated_inference_env_warning`]). Each must be a unicast address with a
/// non-zero port ([`mllm_config::standalone::inference_address`]). Checked
/// before the role boots, so a bad value refuses without side effects.
pub fn inference_override(
    listen: Option<std::net::SocketAddr>,
) -> Result<Option<std::net::SocketAddr>, StartError> {
    use mllm_config::standalone::inference_address;
    if let Some(address) = listen {
        return inference_address(&address.to_string())
            .map(Some)
            .ok_or_else(|| {
                StartError::Setting(format!(
                    "--listen {address} must have a non-zero port and not be multicast"
                ))
            });
    }
    let (variable, value) = match std::env::var_os(INFERENCE_ADDR_ENV) {
        Some(value) => (INFERENCE_ADDR_ENV, value),
        None => match std::env::var_os(DEPRECATED_INFERENCE_ADDR_ENV) {
            Some(value) => (DEPRECATED_INFERENCE_ADDR_ENV, value),
            None => return Ok(None),
        },
    };
    value
        .into_string()
        .ok()
        .and_then(|text| inference_address(&text))
        .map(Some)
        .ok_or_else(|| {
            StartError::Setting(format!(
                "{variable} must be an address with a non-zero port that is not \
                 multicast, e.g. 0.0.0.0:8443 or 127.0.0.1:8443"
            ))
        })
}

/// The warning a role prints once at start when the deprecated
/// `MLLM_STANDALONE_INFERENCE_ADDR` is set, or `None`.
pub fn deprecated_inference_env_warning(listen: Option<std::net::SocketAddr>) -> Option<String> {
    std::env::var_os(DEPRECATED_INFERENCE_ADDR_ENV)?;
    let ignored = listen.is_some() || std::env::var_os(INFERENCE_ADDR_ENV).is_some();
    Some(format!(
        "warning: {DEPRECATED_INFERENCE_ADDR_ENV} is deprecated; use {INFERENCE_ADDR_ENV}{}",
        if ignored {
            " (ignored for this run: --listen or MLLM_INFERENCE_ADDR is set)"
        } else {
            ""
        }
    ))
}

/// Design §9, owner rule (flag > environment > document > default): the
/// address the inference listener of either role binds for this run.
/// `--listen` > `MLLM_INFERENCE_ADDR` (or its deprecated alias) > the
/// document's inference `bind` (`document_bind`, which is `0.0.0.0:8443` when
/// the document states none).
pub fn effective_inference_address(
    document_bind: std::net::SocketAddr,
    listen: Option<std::net::SocketAddr>,
) -> Result<std::net::SocketAddr, StartError> {
    Ok(inference_override(listen)?.unwrap_or(document_bind))
}

/// Owner decision 2026-09-25: the management address a client command uses
/// for the standalone role under `state_dir`: `MLLM_MANAGEMENT_ADDR` (or its
/// deprecated alias), else `server.listeners.management.bind` of the
/// standalone document under the state root with this environment's
/// `MLLM_SET__…` overrides, else `127.0.0.1:7443`. A `--management-listen`
/// the role was started with is not visible here; a client of such a role
/// names the same address with the variable.
pub fn standalone_management_address(state_dir: &Path) -> Result<std::net::SocketAddr, StartError> {
    if let Some(address) = management_override(None)? {
        return Ok(address);
    }
    let document = state_dir.join("config").join("standalone.yaml");
    let text = match std::fs::read_to_string(&document) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(mllm_config::standalone::DEFAULT_MANAGEMENT_BIND
                .parse()
                .expect("valid default"))
        }
        Err(error) => return Err(error.into()),
    };
    let refused = |error: mllm_config::ConfigError| {
        StartError::Setting(format!(
            "{}: {}",
            document.display(),
            crate::settings::describe(&error)
        ))
    };
    let overrides =
        mllm_config::setting_overrides::SettingOverrides::from_process(ConfigKind::Standalone, &[])
            .map_err(refused)?;
    let parsed = mllm_config::parse_document(&text)
        .and_then(|parsed| overrides.apply_and_validate(parsed))
        .map_err(refused)?;
    mllm_config::standalone::management_bind(&parsed).map_err(refused)
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
/// sized once the download is measured (review decision, ADR 0014 §7).
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

/// Review decision (discrete GPU design §3): the KV cache the operator stated
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
    /// no layer names a runtime directory. `None` means there is none, and a
    /// run must name its runtime directory.
    managed_runtime: Option<PathBuf>,
    /// Owner rule 2026-09-25: this run's engine flags, the top layer.
    flags: EngineOverrides,
    /// The standalone document's `host:` block settings, the YAML layer
    /// ([`EngineProvider::configure`]).
    document: std::sync::Mutex<EngineOverrides>,
}

impl EnvEngineProvider {
    pub fn new() -> Self {
        Self {
            managed_runtime: None,
            flags: EngineOverrides::default(),
            document: Default::default(),
        }
    }

    /// SPEC §3.3 / ADR 0001: without `MLLM_RUNTIME_DIR`, the engine runs
    /// from the binary's embedded runtime, written to `dir`.
    pub fn with_managed_runtime(dir: PathBuf) -> Self {
        Self {
            managed_runtime: Some(dir),
            ..Self::new()
        }
    }

    /// Owner rule 2026-09-25: this run's engine flags, which win over the
    /// environment and the document.
    pub fn with_flags(mut self, flags: EngineOverrides) -> Self {
        self.flags = flags;
        self
    }

    /// The engine settings in force: flag > environment > document > default
    /// (`mllm_config::engine_settings`).
    fn settings(&self) -> Result<mllm_config::engine_settings::EngineSettings, ProviderError> {
        let env = EngineOverrides::from_process_env()
            .map_err(|error| no_installation(format!("{}: {}", error.path, error.detail)))?;
        let document = self
            .document
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Ok(mllm_config::engine_settings::resolve(
            &self.flags,
            &env,
            &document,
        ))
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
        // Owner decision 2026-09-25: the models directory is optional here.
        // Unset, the installation names none (an empty path) and the role
        // resolves `model_store.path` or `~/models` with the shared rule
        // (`mllm_config::model_settings`); set, it must be a directory.
        let models_root = match env_value(MODELS_ROOT) {
            None => PathBuf::new(),
            Some(value) => {
                let root = mllm_config::model_settings::absolute(MODELS_ROOT, &value)
                    .map_err(|error| no_installation(error.detail))?;
                if !root.is_dir() {
                    return Err(no_installation(format!(
                        "{MODELS_ROOT} is not a directory: {}",
                        root.display()
                    )));
                }
                root
            }
        };
        // Owner rule 2026-09-25: every setting below is resolved flag >
        // environment > `host:` block > default (`engine_settings`); a
        // malformed value in any layer is refused, even when a registered
        // profile states its own.
        let settings = self.settings()?;
        // SPEC §9.1 / T21 / ADR 0012: deep parking is on unless the host opts
        // out. Sleep mode follows the same switch as deep parking.
        let deep_park = deep_park.unwrap_or(settings.deep_park);
        let trust_remote_code = settings.trust_remote_code;
        let installation_drift = settings.installation_drift;
        let engine_ports = settings.engine_ports.unwrap_or(DEFAULT_ENGINE_PORTS);
        let build_fingerprint = match (fingerprint, settings.build_fingerprint) {
            (Some(registered), _) => registered.to_owned(),
            (None, Some(declared)) => declared,
            (None, None) => probe_fingerprint(&executable)?,
        };
        let declared_kv = settings.kv_cache;
        let kv_cache_declared = declared_kv.is_some();
        let kv_cache_bytes = declared_kv.unwrap_or_else(|| DEFAULT_KV_CACHE.to_string());
        // ADR 0014 §1: the installation keeps host-fixed arguments only; engine
        // tuning belongs to the deployment. SGLang's protected entry takes no
        // argument vector (`engine_policy.rs` refuses any on that family).
        // ADR 0014 §5 (owner decision 2026-09-25): no `--max-model-len`
        // default; an undeclared context is fitted to the KV grant at launch.
        // Explicit engine args are kept as the host's fixed args.
        let args = match engine {
            Engine::Vllm => settings.args,
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
            runtime_dir: runtime_dir(
                engine,
                deep_park,
                settings.runtime_dir,
                self.managed_runtime.as_deref(),
            )?,
            args,
            installation_drift,
            // SPEC §13.3 amendment (owner decision 2026-09-25): the role's own
            // installation names its CUDA toolkit explicitly (`--cuda-home`,
            // `MLLM_CUDA_HOME` or `local_engine.cuda_home`); nothing is detected.
            cuda_home: settings.cuda_home,
            engine_ports,
        })
    }
}

impl EngineProvider for EnvEngineProvider {
    /// The environment's one installation. ADR 0018 §5: with both variables
    /// set this is the vLLM one; [`Self::installations`] publishes both.
    fn installation(&self) -> Result<EngineInstallation, ProviderError> {
        let settings = self.settings()?;
        match (settings.vllm, settings.sglang) {
            (Some(vllm), _) => self.role_installation(Engine::Vllm, vllm),
            (None, Some(sglang)) => self.role_installation(Engine::Sglang, sglang),
            (None, None) => Err(no_installation(format!(
                "this host declares no engine: set {ENGINE_BIN} (or {SGLANG_BIN} \
                 for SGLang, or --vllm-bin / --sglang-bin, or host.local_engine) to \
                 the engine's executable"
            ))),
        }
    }

    fn configure(&self, host: &serde_json::Value) -> Result<(), ProviderError> {
        let stated = EngineOverrides::from_document(host)
            .map_err(|error| no_installation(format!("host.{}: {}", error.path, error.detail)))?;
        *self
            .document
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = stated;
        Ok(())
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
        // Owner rule 2026-09-25: the same names a host gives its
        // `local_engine` profiles (`engine_settings::EngineSettings::installations`).
        let mut all = Vec::new();
        for (profile, engine, executable) in self.settings()?.installations() {
            all.push(named(profile, self.role_installation(engine, executable)?));
        }
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
                "this host declares no engine: set {ENGINE_BIN} or {SGLANG_BIN} (or \
                 --vllm-bin / --sglang-bin, or host.local_engine) to the engine's \
                 executable, or register one with `mllm engine add`"
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
    declared: Option<PathBuf>,
    managed: Option<&Path>,
) -> Result<PathBuf, ProviderError> {
    let dir = match (declared, managed) {
        (Some(declared), _) => declared,
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

/// [`probe_fingerprint`] for the host role, which states `local_engine`
/// profiles in its document (owner rule 2026-09-25).
pub(crate) fn engine_version(executable: &Path) -> Result<String, String> {
    probe_fingerprint(executable).map_err(|error| error.to_string())
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
    start_standalone_with_models(state_dir, config, &ModelOverrides::default()).await
}

/// As [`start_standalone_from`], with this run's `--models-root`,
/// `--model-sources` and `--model-sources-max` (owner decision 2026-09-25:
/// flag > environment > document > default).
pub async fn start_standalone_with_models(
    state_dir: &Path,
    config: Option<&Path>,
    flags: &ModelOverrides,
) -> Result<App, StartError> {
    start_standalone_with_settings(state_dir, config, flags, &EngineOverrides::default()).await
}

/// As [`start_standalone_with_models`], with this run's engine flags too
/// (owner rule 2026-09-25: `--vllm-bin`, `--engine-ports` and the rest win
/// over their variables and the `host:` block of the document).
pub async fn start_standalone_with_settings(
    state_dir: &Path,
    config: Option<&Path>,
    flags: &ModelOverrides,
    engines: &EngineOverrides,
) -> Result<App, StartError> {
    start_standalone_with_overrides(
        state_dir,
        config,
        flags,
        engines,
        &SettingOverrides::none(ConfigKind::Standalone),
    )
    .await
}

/// As [`start_standalone_with_settings`], with this run's generic overrides
/// (owner decision 2026-09-25: `--set` > `MLLM_SET__…` > the document),
/// applied to the standalone document before it is validated.
pub async fn start_standalone_with_overrides(
    state_dir: &Path,
    config: Option<&Path>,
    flags: &ModelOverrides,
    engines: &EngineOverrides,
    overrides: &SettingOverrides,
) -> Result<App, StartError> {
    start_standalone_inner(
        state_dir,
        config,
        Arc::new(
            EnvEngineProvider::with_managed_runtime(state_dir.join("runtime"))
                .with_flags(engines.clone()),
        ),
        crate::host_observation::proc_meminfo(),
        // Design §1: the production boot samples the GPUs with the bounded
        // `nvidia-smi` collector. A machine without one samples nothing and
        // publishes the unified shape, exactly as before.
        Arc::new(mllm_agent::gpu_memory::sample),
        flags,
        overrides,
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
        &ModelOverrides::default(),
        &no_overrides(),
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
    start_standalone_inner(
        state_dir,
        None,
        provider,
        memory,
        no_gpu(),
        &ModelOverrides::default(),
        &no_overrides(),
    )
    .await
}

/// As [`start_standalone_with_memory`], sampling the host's GPUs through `gpu`
/// instead of `nvidia-smi` (design §1).
pub async fn start_standalone_with_gpu(
    state_dir: &Path,
    provider: Arc<dyn EngineProvider>,
    memory: crate::host_observation::MemoryReader,
    gpu: Arc<GpuSampler>,
) -> Result<App, StartError> {
    start_standalone_inner(
        state_dir,
        None,
        provider,
        memory,
        gpu,
        &ModelOverrides::default(),
        &no_overrides(),
    )
    .await
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
    start_standalone_inner(
        state_dir,
        config,
        provider,
        memory,
        no_gpu(),
        &ModelOverrides::default(),
        &no_overrides(),
    )
    .await
}

/// As [`start_standalone_configured`], sampling the GPUs through `gpu`, with
/// this run's model flags ([`start_standalone_with_models`]).
pub async fn start_standalone_configured_with_models(
    state_dir: &Path,
    config: Option<&Path>,
    provider: Arc<dyn EngineProvider>,
    memory: crate::host_observation::MemoryReader,
    gpu: Arc<GpuSampler>,
    flags: &ModelOverrides,
) -> Result<App, StartError> {
    start_standalone_inner(
        state_dir,
        config,
        provider,
        memory,
        gpu,
        flags,
        &no_overrides(),
    )
    .await
}

/// As [`start_standalone_configured`], with generic overrides (owner decision
/// 2026-09-25: `--set` > `MLLM_SET__…` > the document).
pub async fn start_standalone_configured_with_overrides(
    state_dir: &Path,
    config: Option<&Path>,
    provider: Arc<dyn EngineProvider>,
    memory: crate::host_observation::MemoryReader,
    overrides: &SettingOverrides,
) -> Result<App, StartError> {
    start_standalone_inner(
        state_dir,
        config,
        provider,
        memory,
        no_gpu(),
        &ModelOverrides::default(),
        overrides,
    )
    .await
}

/// Design §9: the owner-only file of a standalone state root that holds the
/// API key and the admin token (`identity/credentials`).
pub fn credentials_path(state_dir: &Path) -> PathBuf {
    state_dir.join("identity").join("credentials")
}

/// No generic override.
fn no_overrides() -> SettingOverrides {
    SettingOverrides::none(ConfigKind::Standalone)
}

/// ADR 0019, design §9: run the one-time migration of the role `document`
/// under `state_dir` and report it on stderr, which is the role's log: the
/// notice on the start that migrated, and `config_migration_failed` when the
/// document could not be rewritten (the role then binds 0.0.0.0:8443 for this
/// run). Shared by the standalone and server roles.
pub fn listener_migration(
    document: &Path,
    state_dir: &Path,
    parsed_bind: Option<&str>,
) -> Migration {
    let outcome = mllm_config::listener_migration::migrate(document, state_dir, parsed_bind);
    if let Migration::BindOnly { reason } = &outcome {
        eprintln!(
            "warning: config_migration_failed: {} was not rewritten ({reason}); \
             inference binds {NEW_INFERENCE_DEFAULT} for this run",
            document.display()
        );
    }
    if let Some(notice) = mllm_config::listener_migration::notice(document, &outcome) {
        eprintln!("{notice}");
    }
    outcome
}

/// Owner decision 2026-09-25: the embedded host's models directory and
/// model-source policy, resolved by the shared rule
/// (`mllm_config::model_settings`): flag > environment > `host:` block of the
/// standalone document > default. An installation that names a models
/// directory (`MLLM_MODELS_ROOT`, or a test provider's own) is the
/// environment's layer; the default is `~/models`, created when missing. A
/// named directory must already exist.
fn standalone_models(
    stated_host: &serde_json::Value,
    named: &[NamedInstallation],
    flags: &ModelOverrides,
) -> Result<mllm_config::model_settings::ModelSettings, StartError> {
    use mllm_config::model_settings::{default_models_root, resolve, RootSource};
    let mut env = ModelOverrides::from_process_env()
        .map_err(|error| StartError::Setting(format!("{}: {}", error.path, error.detail)))?;
    env.models_root = named
        .first()
        .map(|first| first.installation.models_root.clone())
        .filter(|root| !root.as_os_str().is_empty());
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let settings = resolve(
        stated_host,
        flags,
        &env,
        default_models_root(home.as_deref()).as_deref(),
    )
    .map_err(|error| StartError::Setting(format!("{}: {}", error.path, error.detail)))?;
    if settings.root_source == RootSource::Default {
        std::fs::create_dir_all(&settings.models_root)?;
    } else if !settings.models_root.is_dir() {
        return Err(StartError::Setting(format!(
            "the models directory is not a directory: {}",
            settings.models_root.display()
        )));
    }
    Ok(settings)
}

async fn start_standalone_inner(
    state_dir: &Path,
    config: Option<&Path>,
    provider: Arc<dyn EngineProvider>,
    memory: crate::host_observation::MemoryReader,
    gpu: Arc<GpuSampler>,
    flags: &ModelOverrides,
    overrides: &SettingOverrides,
) -> Result<App, StartError> {
    // Fail-closed credentials (SPEC §15.2, design §9): the generated api key
    // lives in the protected credentials file. There is no constant fallback:
    // credentials that cannot be read, even ones created by this boot, refuse
    // to serve (MissingCredentials) instead of serving with a guessable key.
    //
    // SPEC §15.2 (R13): an explicit `--config` that is missing or invalid is an
    // error here; it is never replaced by a generated default.
    let outcome = resolve_startup(ConfigKind::Standalone, config, state_dir)?;
    // SPEC §10 (W10): the switch drain bound, `server.switching.drain_timeout`
    // of the standalone document; 30 s when it names none.
    let (
        switch_drain_timeout,
        timing_header,
        config_notices,
        inference_bind,
        management_bind,
        inference_auth,
        listener_migration,
        stated_host,
    ) = {
        let path = match &outcome {
            LoadOutcome::Loaded(path) => PathBuf::from(path),
            LoadOutcome::Generated { config_path, .. } => config_path.clone(),
        };
        let absolute = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        let config_dir = absolute(path.parent().unwrap_or(Path::new(".")));
        let load = || -> Result<_, StartError> {
            let text = std::fs::read_to_string(&path)?;
            let invalid = |error: mllm_config::ConfigError| {
                StartError::Deploy(format!(
                    "standalone configuration: {}",
                    overrides.annotate(error)
                ))
            };
            // Owner decision 2026-09-25: the generic overrides are applied to
            // the document before it is validated, exactly as if it stated
            // them. The one-time listener migration reads the file's own bind.
            let raw = mllm_config::parse_document(&text).map_err(invalid)?;
            let file_bind = raw["server"]["listeners"]["inference"]["bind"]
                .as_str()
                .map(str::to_owned);
            let document = overrides.apply_and_validate(raw).map_err(invalid)?;
            // SPEC §15.3: a value this role would silently ignore (another state
            // directory or listener, TLS, a model store, profiles, numeric limits)
            // is refused before any side effect.
            // SPEC §15.2 (R13): the `server.tls` block an older generator wrote is
            // accepted and reported; every other value is refused.
            let ignored = mllm_config::standalone::check_honoured(
                &document,
                &config_dir,
                &absolute(state_dir),
            )
            .map_err(invalid)?;
            Ok((document, ignored, file_bind))
        };
        let (mut document, mut ignored, file_bind) = load()?;
        // ADR 0019, design §9: a document still stating the old loopback
        // default is migrated to 0.0.0.0:8443 once, after it has been accepted
        // and before its listeners are read. A refused document is never
        // touched.
        let migration = listener_migration(&path, state_dir, file_bind.as_deref());
        if matches!(migration, Migration::Rewritten { .. }) {
            (document, ignored, _) = load()?;
        }
        (
            mllm_config::remote_roles::switch_drain_timeout(&document["server"]).map_err(
                |error| StartError::Deploy(format!("standalone configuration: {error}")),
            )?,
            // SPEC §17 (M80): `server.observability.timing_header`, off unless set.
            mllm_config::remote_roles::timing_header(&document["server"]).map_err(|error| {
                StartError::Deploy(format!("standalone configuration: {error}"))
            })?,
            ignored.iter().map(ToString::to_string).collect::<Vec<_>>(),
            // Design §9: validated by `check_honoured` above. A document the
            // migration could not rewrite still serves on the new default.
            match migration {
                Migration::BindOnly { .. }
                    if overrides.get("server.listeners.inference.bind").is_none() =>
                {
                    NEW_INFERENCE_DEFAULT.parse().expect("valid default")
                }
                _ => mllm_config::standalone::inference_bind(&document).map_err(|error| {
                    StartError::Deploy(format!("standalone configuration: {error}"))
                })?,
            },
            // Owner decision 2026-09-25: validated by `check_honoured` above.
            mllm_config::standalone::management_bind(&document).map_err(|error| {
                StartError::Deploy(format!("standalone configuration: {error}"))
            })?,
            // Design §9: `server.listeners.inference.authentication`
            // (`api_key` unless the document states `none`). The flag and
            // MLLM_INFERENCE_AUTH are applied by the caller for this run.
            mllm_config::standalone::inference_auth(&document, false).map_err(|error| {
                StartError::Deploy(format!("standalone configuration: {error}"))
            })?,
            migration,
            // Owner decision 2026-09-25: `host.model_store` and
            // `host.model_sources`, validated by `check_honoured` above.
            document["host"].clone(),
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
        mllm_config::defaults::create_standalone_credentials(state_dir)?;
    }
    let store = Rc::new(Store::open(&db_path)?);
    let api_key = read_api_key(state_dir)?;

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
    // Owner rule 2026-09-25: the `host:` block's engine settings are the
    // YAML layer of the installation (flag > environment > YAML > default).
    provider.configure(&stated_host)?;
    let mut named = provider.installations(&registered)?;
    // Owner decision 2026-09-25 (standalone is a server plus one host): the
    // models directory and the model-source policy, by the rule a host uses.
    let models = standalone_models(&stated_host, &named, flags)?;
    for n in &mut named {
        n.installation.models_root = models.models_root.clone();
    }
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
        let mut host = crate::standalone_config::host_policy(
            &named,
            &environment_fingerprint,
            capacity_bytes,
            inventory.as_ref(),
            &gpu_shape,
        );
        models.write_into(&mut host);
        // The host's own policy, normalized exactly as resolution normalizes
        // it (`resolve_effective(..).host` is this same value). Nothing here
        // sizes a deployment: a card too small for any template boots and
        // refuses each deployment with `insufficient_device_memory` and its
        // numbers, instead of the whole start failing on a sized probe.
        mllm_config::effective::normalize_host_policy(&host)
            .map_err(|error| StartError::Deploy(format!("host policy invalid: {error}")))?
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
        let (named, fingerprint, inventory, shape, models) = (
            named.clone(),
            environment_fingerprint.clone(),
            inventory.clone(),
            gpu_shape.clone(),
            models.clone(),
        );
        tokio::task::spawn_blocking(move || {
            crate::standalone_engines::EmbeddedHost::new(
                named,
                fingerprint,
                capacity_bytes,
                inventory,
                shape,
                models,
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
    // ADR 0008 (owner decision 2026-09-25): declared remote sources are
    // materialized by the embedded host into its sources store
    // (`<model_store>/sources` unless `host.model_sources.path` names
    // another), exactly as an enrolled host does; activation and the
    // checkpoint digest wait for the verified copy (ADR 0014 §7).
    {
        let root = models.policy.root(&models.models_root).to_path_buf();
        let secrets = Some(state_dir.join("secrets"));
        let store = match provider.model_source_origin() {
            Some(origin) => mllm_agent::sources::SourceStore::with_loopback_origin(
                &root,
                models.policy.clone(),
                secrets,
                &origin,
            ),
            None => mllm_agent::sources::SourceStore::new(&root, models.policy.clone(), secrets),
        };
        supervision.supervise(
            mllm_controller::model_sources::SourceMaterializer::new(
                owner.clone(),
                mllm_controller::model_sources::LocalSources::new(store),
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
    // ADR 0019 (upgrade of a generated policy): a machine whose shape changed
    // since the stored policy was generated gets its policy replaced here,
    // before it is published; charged engines are stopped with verified
    // cleanup first and deployments are re-sized for the new shape.
    let migration_notices = crate::policy_migration::migrate(
        &coordinator.commands(),
        configuration.as_ref(),
        &HostMemoryObservation::with_domains(crate::host_observation::observed_domains(
            &declared_host.domains,
        ))
        .with_memory_reader(memory.clone())
        .with_gpu_sampler(gpu.clone()),
        &declared_host,
        switch_drain_timeout + Duration::from_secs(60),
    )
    .await?;
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
    // Design §9: the inference listener's bind and authentication, set when
    // the caller serves it (`App::inference_router`), for status.
    let inference_listener =
        Arc::new(mllm_management::inference_listener::InferenceListenerView::default());
    let inference_listener_view = mllm_management::inference_listener::inference_listener_router(
        mllm_management::ManagementCredentials::from_trusted_resolver(admin, &api_key)
            .map_err(|_| StartError::MissingCredentials)?,
        inference_listener.clone(),
    );
    let management = mllm_management::lifecycle_router(management_credentials, source)
        .merge(drain)
        .merge(installation_view)
        .merge(latency_view)
        .merge(inference_listener_view);
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
    config_notices.extend(migration_notices);
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
        listener_migration,
        inference_bind,
        management_bind,
        inference_auth,
        inference_listener,
    })
}

/// Read the generated API key from the protected credentials file (F0's
/// fail-closed generation; the key is printed never, only used).
///
/// Design §9 (T37): there is no constant key. A file that is missing,
/// unreadable or has no non-empty `api_key` line, including one this boot
/// just created, is [`StartError::MissingCredentials`].
fn read_api_key(state_dir: &Path) -> Result<String, StartError> {
    std::fs::read_to_string(state_dir.join("identity").join("credentials"))
        .ok()
        .and_then(|creds| {
            creds
                .lines()
                .find_map(|l| l.strip_prefix("api_key: ").map(str::to_string))
        })
        .filter(|key| !key.trim().is_empty())
        .ok_or(StartError::MissingCredentials)
}

pub fn dispatch(command: &CliCommand) -> Result<Infallible, StructuredError> {
    Err(StructuredError::not_yet_implemented(&command.label()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // T37 (design §9): there is no constant key. Freshly created credentials
    // that cannot be read back are a start failure; readable ones give the
    // generated key.
    #[test]
    fn fresh_credentials_must_be_readable() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_api_key(dir.path()),
            Err(StartError::MissingCredentials)
        ));
        assert!(mllm_config::defaults::create_standalone_credentials(dir.path()).unwrap());
        let path = dir.path().join("identity/credentials");
        let key = read_api_key(dir.path()).expect("the generated key");
        assert!(!key.is_empty());
        assert_ne!(key, "mllm-local");
        let text = std::fs::read_to_string(&path).unwrap();
        for corrupt in [
            text.lines()
                .filter(|line| !line.starts_with("api_key: "))
                .map(|line| format!("{line}\n"))
                .collect::<String>(),
            text.replace(&key, ""),
        ] {
            std::fs::write(&path, corrupt).unwrap();
            assert!(matches!(
                read_api_key(dir.path()),
                Err(StartError::MissingCredentials)
            ));
        }
    }
}

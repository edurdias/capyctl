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

use capyctl_config::defaults::{resolve_startup, LoadOutcome};
use capyctl_config::effective::ModelSource;
use capyctl_config::engine_policy::Engine;
pub use capyctl_config::model_settings::ModelOverrides;
use capyctl_config::schema::ConfigKind;
pub use capyctl_config::setting_overrides::SettingOverrides;
use capyctl_controller::coordinator::{
    CoordinatorOptions, EngineBindings, OwnedCoordinator, ServiceClock, ServiceObservation as _,
    ToolsFactory,
};
use capyctl_controller::{CoordinatorLifecycle, OwnedCoordinatorState, ProfileBindings};
use capyctl_launchers::DurableProcessLaunch;
use capyctl_store::secrets::SecretsKey;
use capyctl_store::Store;

use crate::host_observation::{system_clock, HostMemoryObservation};
use capyctl_agent::gpu_memory::{GpuSampler, HostShape};

use crate::grammar::Command as CliCommand;
use crate::output::{ExitCode, StructuredError};

/// The provider seam lives in the controller, so a test double can implement it
/// without depending on this binary. It is re-exported here because this is where
/// standalone is wired.
pub use capyctl_controller::engine_provider::{
    EngineInstallation, EngineProvider, NamedInstallation, ProviderError, RoleSettings,
};

pub const NOT_IMPLEMENTED_EXIT: ExitCode = ExitCode::UNSUPPORTED;

/// The engine's executable. A host with no engine cannot serve: one of these
/// two (or `--vllm-bin` / `--sglang-bin`, or `host.local_engine`), or a
/// profile registered with `capyctl engine add`, must name one. ADR 0018 §5:
/// both set publish two profiles, `local-vllm` and `local-sglang`.
const ENGINE_BIN: &str = capyctl_config::engine_settings::VLLM_BIN_ENV;
const SGLANG_BIN: &str = capyctl_config::engine_settings::SGLANG_BIN_ENV;
const TENSORFOLD_BIN: &str = capyctl_config::engine_settings::TENSORFOLD_BIN_ENV;
/// The directory model weights live under (Spec §7). Optional (owner decision
/// 2026-09-25): `~/models` unless `--models-root`, this variable or
/// `host.model_store.path` names another.
const MODELS_ROOT: &str = capyctl_config::model_settings::MODELS_ROOT_ENV;
const KV_CACHE_BYTES: &str = capyctl_config::engine_settings::KV_CACHE_ENV;
const ENGINE_FINGERPRINT: &str = capyctl_config::engine_settings::ENGINE_FINGERPRINT_ENV;
const RUNTIME_DIR: &str = capyctl_config::engine_settings::RUNTIME_DIR_ENV;
/// SPEC §15.2: a run-time override of the engines' loopback port range, as
/// `start-end`, so two roles on one machine lease different engine ports
/// (owner rule 2026-09-25: one name for both roles; the standalone-only
/// `CAPYCTL_STANDALONE_ENGINE_PORTS` is a deprecated alias).
pub const ENGINE_PORTS_ENV: &str = capyctl_config::engine_settings::ENGINE_PORTS_ENV;
pub use capyctl_config::engine_settings::EngineOverrides;
/// SPEC §16.5 default engine port range.
const DEFAULT_ENGINE_PORTS: (u16, u16) = capyctl_config::engine_settings::DEFAULT_ENGINE_PORTS;

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
    capyctl_agent::rendezvous::RendezvousRoot::new(root.clone()).sweep(retained);
    Ok(root)
}

/// Create a private directory (0700, this user) or refuse one that is not.
fn prepare_private_dir(root: &Path) -> Result<(), StartError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let private = std::fs::symlink_metadata(root).is_ok_and(|meta| {
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o7777 == 0o700
    });
    if private {
        Ok(())
    } else {
        Err(StartError::Setting(format!(
            "{} must be a directory owned by this user with mode 0700; no permissions were changed",
            root.display()
        )))
    }
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
    deps: capyctl_router::RouterDeps,
    api_key: String,
    /// The embedded role's supervisors: readiness (SPEC §4.3, P3: re-proves
    /// the engines a restart adopted before any request is forwarded to
    /// them), engine exits (SPEC §13.2, W13) and, for bindings that read a
    /// checkpoint, checkpoint digests (ADR 0014 §7, WE3). Joined by
    /// [`App::shutdown`]; aborted when the app is dropped.
    supervision: crate::shutdown::Supervision,
    /// SPEC §10, ADR 0013 §8 (W10): the switcher, held so shutdown joins the
    /// `--evict` follow-ups it started.
    switcher: Arc<capyctl_controller::switching::Switcher>,
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
    /// Design §9: the document's `server.listeners.inference.authentication`
    /// (`api_key` unless it states `none`).
    inference_auth: capyctl_config::standalone::InferenceAuth,
    /// Design §9: the listener the role serves, reported to status.
    inference_listener: Arc<capyctl_management::inference_listener::InferenceListenerView>,
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
    /// `CAPYCTL_INFERENCE_ADDR` overrides it
    /// ([`effective_inference_address`]).
    pub fn inference_bind(&self) -> std::net::SocketAddr {
        self.inference_bind
    }

    /// Owner decision 2026-09-25: the management bind the standalone document
    /// states (default `127.0.0.1:7443`), unless `--management-listen` or
    /// `CAPYCTL_MANAGEMENT_ADDR` overrides it ([`management_override`]).
    pub fn management_bind(&self) -> std::net::SocketAddr {
        self.management_bind
    }

    /// Design §9: the inference authentication the standalone document
    /// states (`api_key` unless `none`). `--no-inference-auth` and
    /// `CAPYCTL_INFERENCE_AUTH` override it for one run
    /// ([`crate::exposure::effective_inference_auth`]).
    pub fn inference_auth(&self) -> capyctl_config::standalone::InferenceAuth {
        self.inference_auth
    }

    /// Design §9: the router to serve on `bind` with `auth` for this run, and
    /// the listener recorded for status. With [`InferenceAuth::None`] the
    /// router's key check is off (`RouterDeps.api_key: None`); with
    /// [`InferenceAuth::ApiKey`] it is [`App::router`], which requires the
    /// key on every route (SPEC §13.3, T37).
    ///
    /// [`InferenceAuth::None`]: capyctl_config::standalone::InferenceAuth::None
    /// [`InferenceAuth::ApiKey`]: capyctl_config::standalone::InferenceAuth::ApiKey
    pub fn inference_router(
        &self,
        bind: std::net::SocketAddr,
        auth: capyctl_config::standalone::InferenceAuth,
    ) -> axum::Router {
        use capyctl_config::standalone::InferenceAuth;
        self.inference_listener
            .set(crate::exposure::listener_view(bind, auth));
        match auth {
            InferenceAuth::ApiKey => self.router.clone(),
            InferenceAuth::None => capyctl_router::serve_router(capyctl_router::RouterDeps {
                api_key: None,
                ..self.deps.clone()
            }),
        }
    }

    pub fn deps(&self) -> &capyctl_router::RouterDeps {
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
        capyctl_controller::coordinator::WorkerStatus,
        capyctl_controller::coordinator::CoordinatorError,
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
pub const INFERENCE_ADDR_ENV: &str = "CAPYCTL_INFERENCE_ADDR";
/// The standalone-only name [`INFERENCE_ADDR_ENV`] replaces. Still read, after
/// it, with a deprecation warning.
pub const DEPRECATED_INFERENCE_ADDR_ENV: &str = "CAPYCTL_STANDALONE_INFERENCE_ADDR";
/// Owner decision 2026-09-25: the standalone management address for one
/// run, for the role and its client commands alike (loopback only).
pub const MANAGEMENT_ADDR_ENV: &str = "CAPYCTL_MANAGEMENT_ADDR";
/// The standalone-only name [`MANAGEMENT_ADDR_ENV`] replaces. Still read,
/// after it, with a deprecation warning.
pub const DEPRECATED_MANAGEMENT_ADDR_ENV: &str = "CAPYCTL_STANDALONE_MANAGEMENT_ADDR";

/// SPEC §16.5, owner decision 2026-09-25: the run-time override of the
/// standalone management bind, if any: `--management-listen`, then
/// `CAPYCTL_MANAGEMENT_ADDR`, then the deprecated
/// `CAPYCTL_STANDALONE_MANAGEMENT_ADDR`. Each must be a loopback address with a
/// non-zero port; a bad one is refused with its name.
pub fn management_override(
    flag: Option<std::net::SocketAddr>,
) -> Result<Option<std::net::SocketAddr>, StartError> {
    use capyctl_config::standalone::management_address;
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

/// Print a role warning that may carry the legacy `warning: ` prefix; the sink
/// adds its own.
pub fn role_warning(line: &str) {
    let text = line.strip_prefix("warning: ").unwrap_or(line);
    capyctl_domain::role_log::notice(capyctl_domain::role_log::Level::Warning, text);
}

/// The warning a role prints once at start when the deprecated
/// `CAPYCTL_STANDALONE_MANAGEMENT_ADDR` is set, or `None`.
pub fn deprecated_management_env_warning(flag: Option<std::net::SocketAddr>) -> Option<String> {
    std::env::var_os(DEPRECATED_MANAGEMENT_ADDR_ENV)?;
    let ignored = flag.is_some() || std::env::var_os(MANAGEMENT_ADDR_ENV).is_some();
    Some(format!(
        "warning: {DEPRECATED_MANAGEMENT_ADDR_ENV} is deprecated; use {MANAGEMENT_ADDR_ENV}{}",
        if ignored {
            " (ignored for this run: --management-listen or CAPYCTL_MANAGEMENT_ADDR is set)"
        } else {
            ""
        }
    ))
}

/// Design §9: the run-time override of the inference bind, if any, for the
/// server and standalone roles alike. Precedence: `--listen`, then
/// `CAPYCTL_INFERENCE_ADDR`, then the deprecated `CAPYCTL_STANDALONE_INFERENCE_ADDR`
/// ([`deprecated_inference_env_warning`]). Each must be a unicast address with a
/// non-zero port ([`capyctl_config::standalone::inference_address`]). Checked
/// before the role boots, so a bad value refuses without side effects.
pub fn inference_override(
    listen: Option<std::net::SocketAddr>,
) -> Result<Option<std::net::SocketAddr>, StartError> {
    use capyctl_config::standalone::inference_address;
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
/// `CAPYCTL_STANDALONE_INFERENCE_ADDR` is set, or `None`.
pub fn deprecated_inference_env_warning(listen: Option<std::net::SocketAddr>) -> Option<String> {
    std::env::var_os(DEPRECATED_INFERENCE_ADDR_ENV)?;
    let ignored = listen.is_some() || std::env::var_os(INFERENCE_ADDR_ENV).is_some();
    Some(format!(
        "warning: {DEPRECATED_INFERENCE_ADDR_ENV} is deprecated; use {INFERENCE_ADDR_ENV}{}",
        if ignored {
            " (ignored for this run: --listen or CAPYCTL_INFERENCE_ADDR is set)"
        } else {
            ""
        }
    ))
}

/// Design §9, owner rule (flag > environment > document > default): the
/// address the inference listener of either role binds for this run.
/// `--listen` > `CAPYCTL_INFERENCE_ADDR` (or its deprecated alias) > the
/// document's inference `bind` (`document_bind`, which is `0.0.0.0:8443` when
/// the document states none).
pub fn effective_inference_address(
    document_bind: std::net::SocketAddr,
    listen: Option<std::net::SocketAddr>,
) -> Result<std::net::SocketAddr, StartError> {
    Ok(inference_override(listen)?.unwrap_or(document_bind))
}

/// Final review I8: where a role records the management address it serves
/// on this run, under its own owner-only state directory, so client commands
/// find a role started with `--management-listen`.
pub fn recorded_management_path(state_dir: &Path) -> PathBuf {
    state_dir.join("run").join("management-address")
}

/// Record the management address a role serves on (`<state>/run`, 0700; the
/// file 0600, replaced atomically). A failure is reported, never fatal: a
/// client still finds the role by `CAPYCTL_MANAGEMENT_ADDR` or its document.
pub fn record_management_address(state_dir: &Path, address: std::net::SocketAddr) {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    let path = recorded_management_path(state_dir);
    let written = (|| -> std::io::Result<()> {
        let dir = path.parent().expect("a parent");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let temporary = dir.join(".management-address.tmp");
        let _ = std::fs::remove_file(&temporary);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        writeln!(file, "{address}")?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)
    })();
    if let Err(error) = written {
        capyctl_domain::role_log::notice(
            capyctl_domain::role_log::Level::Warning,
            &format!(
                "could not record the management address in {} ({error}); clients \
                 find it through CAPYCTL_MANAGEMENT_ADDR or the role document",
                path.display()
            ),
        );
    }
}

/// The management address a role under `state_dir` recorded, when it is a
/// loopback address (SPEC §16.5: management never leaves loopback, so a
/// recorded value that is not one is ignored).
pub fn recorded_management_address(state_dir: &Path) -> Option<std::net::SocketAddr> {
    let text = std::fs::read_to_string(recorded_management_path(state_dir)).ok()?;
    capyctl_config::standalone::management_address(text.trim())
}

/// Owner decision 2026-09-25: the management address a client command uses
/// for the standalone role under `state_dir`: `CAPYCTL_MANAGEMENT_ADDR` (or its
/// deprecated alias), else `server.listeners.management.bind` of the
/// standalone document under the state root with this environment's
/// `CAPYCTL_SET__…` overrides, else `127.0.0.1:7443`. Between the variable and
/// the document: the address the role recorded when it started
/// ([`recorded_management_address`]), so a role started with
/// `--management-listen` is found without the variable.
pub fn standalone_management_address(state_dir: &Path) -> Result<std::net::SocketAddr, StartError> {
    if let Some(address) = management_override(None)? {
        return Ok(address);
    }
    // Final review I8: the address the role serves on this run, which a
    // `--management-listen` start recorded.
    if let Some(address) = recorded_management_address(state_dir) {
        return Ok(address);
    }
    let document = state_dir.join("config").join("standalone.yaml");
    let text = match std::fs::read_to_string(&document) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(capyctl_config::standalone::DEFAULT_MANAGEMENT_BIND
                .parse()
                .expect("valid default"))
        }
        Err(error) => return Err(error.into()),
    };
    let refused = |error: capyctl_config::ConfigError| {
        StartError::Setting(format!(
            "{}: {}",
            document.display(),
            crate::settings::describe(&error)
        ))
    };
    let overrides = capyctl_config::setting_overrides::SettingOverrides::from_process(
        ConfigKind::Standalone,
        &[],
    )
    .map_err(refused)?;
    let parsed = capyctl_config::parse_document(&text)
        .and_then(|parsed| overrides.apply_and_validate(parsed))
        .map_err(refused)?;
    capyctl_config::standalone::management_bind(&parsed).map_err(refused)
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
            .ok_or_else(|| {
                StartError::Deploy(
                    "this role has no engine: register one with `capyctl engine add <path>`, then deploy"
                        .into(),
                )
            })?;
        // Design §3: a discrete host sizes the deployment from the checkpoint's
        // weights; a unified host from its observed capacity, as before.
        let memory = match &self.gpu_shape {
            HostShape::Discrete(gpus) => {
                let weights = checkpoint_weights(&first.installation.models_root, &source)?;
                crate::standalone_config::discrete_template_memory(
                    gpus,
                    &host,
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

/// A standalone listener that cannot bind names itself and its address; the
/// error keeps its kind, so the exit code is unchanged.
pub fn listen_failed(
    listener: &str,
    address: std::net::SocketAddr,
    failure: std::io::Error,
) -> StartError {
    let hint = match failure.kind() {
        std::io::ErrorKind::AddrInUse => {
            " (another capyctl role or program listens there; stop it or choose another address)"
        }
        _ => "",
    };
    StartError::Io(std::io::Error::new(
        failure.kind(),
        format!("cannot listen on {address} for the {listener} listener: {failure}{hint}"),
    ))
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("config: {0}")]
    Config(#[from] capyctl_config::error::ConfigError),
    #[error("store: {0}")]
    Store(#[from] capyctl_store::StoreError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("credentials missing: refusing to serve without a generated api key")]
    MissingCredentials,
    /// The message names what was expected, because a bare refusal leaves an
    /// operator guessing.
    #[error("no engine installation: {0}")]
    NoEngineInstallation(String),
    /// ADR 0018 §5: an environment-variable profile and a registered one
    /// share a name.
    #[error("{0}")]
    ProfileExists(String),
    #[error("controller ownership: {0}")]
    Ownership(#[from] capyctl_controller::OwnedStateError),
    #[error("coordinator: {0}")]
    Coordinator(#[from] capyctl_controller::coordinator::CoordinatorError),
    #[error("deploy: {0}")]
    Deploy(String),
    /// SPEC §15.3: a run-time setting that is present but malformed is refused.
    #[error("invalid setting: {0}")]
    Setting(String),
    /// Design §1: integrated and discrete GPUs on one host are refused at
    /// boot, not guessed at (`unsupported_gpu_topology`).
    #[error("{0}")]
    GpuTopology(capyctl_agent::gpu_memory::GpuShapeError),
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
    capyctl_agent::checkpoint::CheckpointVerifier::in_memory()
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
/// with `CAPYCTL_KV_CACHE_BYTES`, which a discrete template honours within the
/// card or refuses. `None` when the installation carries the unified default.
fn declared_kv_cache(installation: &EngineInstallation) -> Result<Option<i64>, StartError> {
    if !installation.kv_cache_declared {
        return Ok(None);
    }
    let stated = installation.engine_config["memory"]["kv_cache"]
        .as_str()
        .ok_or_else(|| StartError::Setting(format!("{KV_CACHE_BYTES} is not a byte size")))?;
    capyctl_config::effective::parse_bytes(stated)
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
        let code =
            match err {
                // SPEC §13.2 / T33: a store written by a newer capyctl is not a
                // transient fault; restarting this binary never heals it.
                StartError::Store(capyctl_store::StoreError::FromNewerVersion { .. })
                | StartError::Ownership(capyctl_controller::OwnedStateError::Store(
                    capyctl_store::StoreError::FromNewerVersion { .. },
                )) => crate::output::STORE_FROM_NEWER_VERSION,
                StartError::Config(_) => "invalid_config",
                // Found live 2026-10-03: a state path that breaks the ownership
                // rules is the operator's to change, not an internal fault.
                StartError::Ownership(capyctl_controller::OwnedStateError::Ownership(
                    ref error,
                )) if error.kind() == std::io::ErrorKind::PermissionDenied => "invalid_config",
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
    /// ADR 0023 §2: where a TensorFold's build toolchain is looked for.
    toolchain: capyctl_config::toolchain::ToolchainSearch,
}

impl EnvEngineProvider {
    pub fn new() -> Self {
        Self {
            managed_runtime: None,
            flags: EngineOverrides::default(),
            document: Default::default(),
            toolchain: Default::default(),
        }
    }

    /// Looks for a TensorFold's build toolchain in `search` (a test's own
    /// directories) instead of the system's.
    pub fn with_toolchain_search(
        mut self,
        search: capyctl_config::toolchain::ToolchainSearch,
    ) -> Self {
        self.toolchain = search;
        self
    }

    /// SPEC §3.3 / ADR 0001: without `CAPYCTL_RUNTIME_DIR`, the engine runs
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
    /// (`capyctl_config::engine_settings`).
    fn settings(&self) -> Result<capyctl_config::engine_settings::EngineSettings, ProviderError> {
        let env = EngineOverrides::from_process_env()
            .map_err(|error| no_installation(format!("{}: {}", error.path, error.detail)))?;
        let document = self
            .document
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Ok(capyctl_config::engine_settings::resolve(
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
        // Owner rule 2026-09-25: every setting below is resolved flag >
        // environment > `host:` block > default (`engine_settings`); a
        // malformed value in any layer is refused, even when a registered
        // profile states its own.
        let settings = self.settings()?;
        let role = self.role_settings()?;
        // SPEC §9.1 / T21 / ADR 0012: deep parking is on unless the host opts
        // out. Sleep mode follows the same switch as deep parking.
        let mut deep_park = deep_park.unwrap_or(settings.deep_park);
        // ADR 0023 §2, §6: an environment TensorFold install is checked
        // for its build toolchain as `engine add` checks it, and never parks.
        if engine == Engine::Tensorfold {
            deep_park = false;
            if fingerprint.is_none() {
                let bin = executable.parent().unwrap_or(Path::new(""));
                capyctl_config::toolchain::check(
                    bin,
                    role.cuda_home.as_deref(),
                    &self.toolchain.system,
                )
                .map_err(|missing| {
                    no_installation(format!("toolchain_missing: {}", missing.for_local_engine()))
                })?;
            }
        }
        let trust_remote_code = settings.trust_remote_code;
        let installation_drift = settings.installation_drift;
        let build_fingerprint = match (fingerprint, settings.build_fingerprint) {
            (Some(registered), _) => registered.to_owned(),
            (None, Some(declared)) => declared,
            (None, None) => probe_fingerprint(&executable, engine)?,
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
            Engine::Vllm | Engine::Tensorfold => settings.args,
            Engine::Sglang => Vec::new(),
        };
        // ADR 0014 §2, §5: the generated standalone deployment states its KV
        // cache; its memory request is the Ready allocation it declares.
        let engine_config = serde_json::json!({"memory": {"kv_cache": kv_cache_bytes}});
        verify_runtime(&role.runtime_dir, engine, deep_park)?;
        Ok(EngineInstallation {
            engine,
            executable,
            build_fingerprint,
            engine_config,
            kv_cache_declared,
            deep_park,
            trust_remote_code,
            models_root: role.models_root,
            runtime_dir: role.runtime_dir,
            args,
            installation_drift,
            cuda_home: role.cuda_home,
            engine_ports: role.engine_ports,
            registered: None,
        })
    }
}

impl EngineProvider for EnvEngineProvider {
    /// The environment's one installation. ADR 0018 §5: with both variables
    /// set this is the vLLM one; [`Self::installations`] publishes both.
    fn installation(&self) -> Result<EngineInstallation, ProviderError> {
        let settings = self.settings()?;
        match settings.installations().into_iter().next() {
            Some((_, engine, executable)) => self.role_installation(engine, executable),
            None => Err(no_installation(format!(
                "this host declares no engine: set {ENGINE_BIN} (or {SGLANG_BIN} \
                 for SGLang, {TENSORFOLD_BIN} for TensorFold, or --vllm-bin / \
                 --sglang-bin / --tensorfold-bin, or host.local_engine) to the \
                 engine's executable"
            ))),
        }
    }

    fn role_settings(&self) -> Result<RoleSettings, ProviderError> {
        let settings = self.settings()?;
        // Owner decision 2026-09-25: the models directory is optional here.
        // Unset, the installation names none (an empty path) and the role
        // resolves `model_store.path` or `~/models` with the shared rule
        // (`capyctl_config::model_settings`); set, it must be a directory.
        let models_root = match env_value(MODELS_ROOT) {
            None => PathBuf::new(),
            Some(value) => {
                let root = capyctl_config::model_settings::absolute(MODELS_ROOT, &value)
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
        let groups = settings.groups();
        Ok(RoleSettings {
            runtime_dir: runtime_dir(settings.runtime_dir, self.managed_runtime.as_deref())?,
            engine_ports: settings.engine_ports.unwrap_or(DEFAULT_ENGINE_PORTS),
            models_root,
            // SPEC §13.3 amendment (owner decision 2026-09-25): the role's own
            // installation names its CUDA toolkit explicitly (`--cuda-home`,
            // `CAPYCTL_CUDA_HOME` or `local_engine.cuda_home`); nothing is detected.
            cuda_home: settings.cuda_home,
            groups,
        })
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
            let engine = profile["engine"]
                .as_str()
                .and_then(Engine::from_name)
                .unwrap_or(Engine::Vllm);
            let executable = PathBuf::from(profile["executable"].as_str().unwrap_or_default());
            let base = self.role_installation_as(
                engine,
                executable,
                Some(profile["build_fingerprint"].as_str().unwrap_or("unknown")),
                Some(profile["security"]["deep_park"].as_str() != Some("disabled")),
            )?;
            all.push(named(
                name,
                capyctl_controller::engine_provider::from_profile(&base, profile),
            ));
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
            .with_rendezvous_root(log_dir.with_file_name(RENDEZVOUS_DIR))
            .with_engine_cache_root(log_dir.with_file_name("engines"));
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

/// Where capyctl's own guard middleware lives.
///
/// Spec §3: the guard is imported by the engine over `PYTHONPATH`, so a directory
/// that does not contain it is not a runtime directory. SPEC §3.3 / ADR 0001
/// (owner decision 2026-09-24): by default it is the managed copy of the
/// runtime embedded in this binary, `<state root>/runtime`, written or
/// refreshed here; `CAPYCTL_RUNTIME_DIR` names another (development), which is
/// never written to.
fn runtime_dir(
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
                "no runtime directory: set {RUNTIME_DIR} to capyctl's runtime directory"
            )))
        }
    };
    if !dir.join("capyctl_vllm_guard.py").is_file() {
        return Err(no_installation(format!(
            "{} holds no capyctl_vllm_guard.py, so the engine's control routes could \
             not be guarded; set {RUNTIME_DIR} to capyctl's runtime directory",
            dir.display()
        )));
    }
    // ADR 0014 §6 / owner decision Q11: every vLLM launch runs through capyctl's
    // protected entry in the same directory; refused here, where the
    // installation is resolved, rather than as an engine that exits at spawn.
    if !dir.join(capyctl_adapters::vllm::VLLM_ENTRY).is_file() {
        return Err(no_installation(format!(
            "{} holds no {}, so vLLM's reserved settings could not be enforced; \
             set {RUNTIME_DIR} to capyctl's runtime directory",
            dir.display(),
            capyctl_adapters::vllm::VLLM_ENTRY
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

/// The runtime modules one installation of `engine` imports from `dir`.
/// `deep_park` is the host's switch: with it on, a parking vLLM deployment
/// renders sleep mode, whose entry imports the capability probes (ADR 0008).
fn verify_runtime(dir: &Path, engine: Engine, deep_park: bool) -> Result<(), ProviderError> {
    // SPEC §9.1, §13.3 / T21 T37: the same integrity the host agent requires
    // before a launch. The engine imports capyctl's modules from this directory,
    // so one another account could rewrite is refused here, before anything
    // is served.
    capyctl_agent::runtime_integrity::verify(
        dir,
        capyctl_agent::runtime_integrity::required_files(engine, deep_park),
    )
    .map_err(|error| {
        no_installation(format!(
            "runtime_integrity: {error}; capyctl's runtime modules must be regular files \
             owned by this user, never writable by other, and writable by group \
             only through this user's private group"
        ))
    })
}

/// [`probe_fingerprint`] for the host role, which states `local_engine`
/// profiles in its document (owner rule 2026-09-25).
pub(crate) fn engine_version(engine: Engine, executable: &Path) -> Result<String, String> {
    probe_fingerprint(executable, engine).map_err(|error| error.to_string())
}

/// What the installed engine says it is.
///
/// The fingerprint pins the recipe, so it has to come from the installation rather
/// than from a constant that would keep claiming the same build after an upgrade.
/// A probe that fails or hangs is a refusal: an engine that cannot print its own
/// version is not one this host should publish.
fn probe_fingerprint(executable: &Path, engine: Engine) -> Result<String, ProviderError> {
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
    // ADR 0023 §2: `tensorfold 0.6.0` publishes `0.6.0`, as `engine add`
    // records it. vLLM and SGLang publish what they print, as before.
    let fingerprint = match engine {
        Engine::Tensorfold => printed
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .and_then(|line| line.split_whitespace().last())
            .unwrap_or_default(),
        Engine::Vllm | Engine::Sglang => printed.trim(),
    }
    .to_owned();
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
/// (owner decision 2026-09-25: `--set` > `CAPYCTL_SET__…` > the document),
/// applied to the standalone document before it is validated.
pub async fn start_standalone_with_overrides(
    state_dir: &Path,
    config: Option<&Path>,
    flags: &ModelOverrides,
    engines: &EngineOverrides,
    overrides: &SettingOverrides,
) -> Result<App, StartError> {
    start_standalone_production(
        state_dir,
        config,
        flags,
        engines,
        overrides,
        ConfigHome::Process,
    )
    .await
}

/// Final review I14: the production boot with its registered engines read
/// from `<config_home>/capyctl/engines.yaml` instead of this process's config
/// home, for a test of the production path that must not read the
/// developer's registrations.
pub async fn start_standalone_with_config_home(
    state_dir: &Path,
    config_home: &Path,
) -> Result<App, StartError> {
    start_standalone_production(
        state_dir,
        None,
        &ModelOverrides::default(),
        &EngineOverrides::default(),
        &no_overrides(),
        ConfigHome::Isolated(config_home.to_path_buf()),
    )
    .await
}

async fn start_standalone_production(
    state_dir: &Path,
    config: Option<&Path>,
    flags: &ModelOverrides,
    engines: &EngineOverrides,
    overrides: &SettingOverrides,
    config_home: ConfigHome,
) -> Result<App, StartError> {
    start_standalone_in(
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
        Arc::new(capyctl_agent::gpu_memory::sample),
        flags,
        overrides,
        config_home,
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
/// 2026-09-25: `--set` > `CAPYCTL_SET__…` > the document).
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

/// Owner decision 2026-09-25: the embedded host's models directory and
/// model-source policy, resolved by the shared rule
/// (`capyctl_config::model_settings`): flag > environment > `host:` block of the
/// standalone document > default. An installation that names a models
/// directory (`CAPYCTL_MODELS_ROOT`, or a test provider's own) is the
/// environment's layer; the default is `~/models`, created when missing. A
/// named directory must already exist.
fn standalone_models(
    stated_host: &serde_json::Value,
    role: &RoleSettings,
    flags: &ModelOverrides,
) -> Result<capyctl_config::model_settings::ModelSettings, StartError> {
    use capyctl_config::model_settings::{default_models_root, resolve, RootSource};
    let mut env = ModelOverrides::from_process_env()
        .map_err(|error| StartError::Setting(format!("{}: {}", error.path, error.detail)))?;
    env.models_root = Some(role.models_root.clone()).filter(|root| !root.as_os_str().is_empty());
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

/// Where a standalone boot looks for `<config home>/capyctl/engines.yaml` when
/// no role document is named.
#[derive(Debug, Clone)]
enum ConfigHome {
    /// `XDG_CONFIG_HOME`, else `~/.config`, of this process (production).
    Process,
    /// Final review I14: a boot with an explicit provider (a test's) reads its
    /// own, never the developer's, registered engines.
    Isolated(PathBuf),
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
    start_standalone_in(
        state_dir,
        config,
        provider,
        memory,
        gpu,
        flags,
        overrides,
        ConfigHome::Isolated(state_dir.join(".config")),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_standalone_in(
    state_dir: &Path,
    config: Option<&Path>,
    provider: Arc<dyn EngineProvider>,
    memory: crate::host_observation::MemoryReader,
    gpu: Arc<GpuSampler>,
    flags: &ModelOverrides,
    overrides: &SettingOverrides,
    config_home: ConfigHome,
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
        idle,
        timing_header,
        config_notices,
        inference_bind,
        management_bind,
        inference_auth,
        stated_host,
        group_stall_timeout,
    ) = {
        let path = match &outcome {
            LoadOutcome::Loaded(path) => PathBuf::from(path),
            LoadOutcome::Generated { config_path, .. } => config_path.clone(),
        };
        let absolute = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        let config_dir = absolute(path.parent().unwrap_or(Path::new(".")));
        let load = || -> Result<_, StartError> {
            let text = std::fs::read_to_string(&path)?;
            let invalid = |error: capyctl_config::ConfigError| {
                StartError::Deploy(format!(
                    "standalone configuration: {}",
                    overrides.annotate(error)
                ))
            };
            // Owner decision 2026-09-25: the generic overrides are applied to
            // the document before it is validated, exactly as if it stated
            // them.
            let raw = capyctl_config::parse_document(&text).map_err(invalid)?;
            let document = overrides.apply_and_validate(raw).map_err(invalid)?;
            // SPEC §15.3: a value this role would silently ignore (another state
            // directory or listener, TLS, a model store, profiles, numeric limits)
            // is refused before any side effect.
            // SPEC §15.2 (R13): the `server.tls` block an older generator wrote is
            // accepted and reported; every other value is refused.
            let ignored = capyctl_config::standalone::check_honoured(
                &document,
                &config_dir,
                &absolute(state_dir),
            )
            .map_err(invalid)?;
            Ok((document, ignored))
        };
        let (document, ignored) = load()?;
        (
            capyctl_config::remote_roles::switch_drain_timeout(&document["server"]).map_err(
                |error| StartError::Deploy(format!("standalone configuration: {error}")),
            )?,
            // SPEC §6.5 (W5): `server.lifecycle_defaults`, off unless named, as
            // in a server document.
            capyctl_config::remote_roles::idle_timeouts(&document["server"]).map_err(|error| {
                StartError::Deploy(format!("standalone configuration: {error}"))
            })?,
            // SPEC §17 (M80): `server.observability.timing_header`, off unless set.
            capyctl_config::remote_roles::timing_header(&document["server"]).map_err(|error| {
                StartError::Deploy(format!("standalone configuration: {error}"))
            })?,
            ignored.iter().map(ToString::to_string).collect::<Vec<_>>(),
            // Design §9: validated by `check_honoured` above. An explicit bind
            // is always honoured (owner decision 2026-09-29).
            capyctl_config::standalone::inference_bind(&document).map_err(|error| {
                StartError::Deploy(format!("standalone configuration: {error}"))
            })?,
            // Owner decision 2026-09-25: validated by `check_honoured` above.
            capyctl_config::standalone::management_bind(&document).map_err(|error| {
                StartError::Deploy(format!("standalone configuration: {error}"))
            })?,
            // Design §9: `server.listeners.inference.authentication`
            // (`api_key` unless the document states `none`). The flag and
            // CAPYCTL_INFERENCE_AUTH are applied by the caller for this run.
            capyctl_config::standalone::inference_auth(&document, false).map_err(|error| {
                StartError::Deploy(format!("standalone configuration: {error}"))
            })?,
            // Owner decision 2026-09-25: `host.model_store` and
            // `host.model_sources`, validated by `check_honoured` above.
            document["host"].clone(),
            // ADR 0028 §11 (decided 2026-10-06), owner rule (standalone is a
            // server and one host): `server.groups.stall_timeout` and
            // CAPYCTL_GROUP_STALL_TIMEOUT are honoured as on a server (a
            // malformed or zero value refuses the start); standalone never
            // runs a group, so no request of it is watched.
            capyctl_config::remote_roles::server_groups(&document["server"])
                .and_then(|groups| groups.with_overrides(None, &|name| std::env::var(name).ok()))
                .map_err(|error| StartError::Deploy(format!("standalone configuration: {error}")))?
                .stall_timeout,
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
        capyctl_config::defaults::create_standalone_credentials(state_dir)?;
    }
    let store = Rc::new(Store::open(&db_path)?);
    let api_key = read_api_key(state_dir)?;

    // SPEC §8 (amended 2026-10-01): what this host publishes about its engines
    // is what it has. There is no fallback installation: a host with none
    // starts and publishes none. ADR 0018 §5: the environment's
    // installations and the profiles registered in engines.yaml (beside
    // `--config`, else `<config home>/capyctl/engines.yaml`); a name declared in
    // both is refused `profile_exists`. The standalone document is never written.
    let engines = crate::engine::role_engines(config, &|key| match &config_home {
        ConfigHome::Process => std::env::var(key).ok().filter(|value| !value.is_empty()),
        ConfigHome::Isolated(home) => {
            (key == "XDG_CONFIG_HOME").then(|| home.to_string_lossy().into_owned())
        }
    });
    // ADR 0018 amendment A3: a profile for an engine kind this release does
    // not know (a newer release wrote it) is skipped with a warning.
    let registered = match &engines {
        Some(path) => {
            let (runnable, skipped) =
                capyctl_config::registration::EnginesFile::load(path)?.runnable();
            for unknown in skipped {
                role_warning(&unknown.warning(path));
            }
            runnable
        }
        None => serde_json::Map::new(),
    };
    for (name, profile) in &registered {
        capyctl_config::registration::check_profile(name, profile)?;
    }
    // Owner rule 2026-09-25: the `host:` block's engine settings are the
    // YAML layer of the installation (flag > environment > YAML > default).
    provider.configure(&stated_host)?;
    // Role-level settings (runtime directory, ports, model store) are the same
    // for every installation, and a role with none still has them.
    let role = provider.role_settings()?;
    let mut named = provider.installations(&registered)?;
    // Owner decision 2026-09-25 (standalone is a server plus one host): the
    // models directory and the model-source policy, by the rule a host uses.
    let models = standalone_models(&stated_host, &role, flags)?;
    for n in &mut named {
        n.installation.models_root = models.models_root.clone();
    }
    let environment_fingerprint = match named.first() {
        Some(first) => format!("standalone-{}", first.installation.build_fingerprint),
        None => "standalone-none".to_owned(),
    };
    let capacity_bytes = memory()
        .map(|sample| sample.capacity_bytes)
        .map_err(|error| StartError::Deploy(format!("host capacity unreadable: {error}")))?;

    // Design §1: the GPUs are sampled once, and the shape they describe decides
    // the domains this host publishes. Integrated and discrete devices mixed on
    // one host are refused rather than guessed at. No sample (no `nvidia-smi`,
    // or a failed run) is a host with no GPU, which publishes as before.
    let gpu_sample = gpu();
    let gpu_shape =
        capyctl_agent::gpu_memory::shape(gpu_sample.as_ref()).map_err(StartError::GpuTopology)?;

    // SPEC §3: the NVIDIA device inventory is a host fact published at boot
    // like the fingerprints. The collector is bounded and closed-error: a
    // machine with no NVIDIA devices, or one whose collection fails or
    // overruns its bound, publishes nothing, and an SGLang deployment then
    // fails placement honestly at the native gate instead of the host
    // claiming placement it cannot corroborate.
    let inventory = crate::device_inventory::collect(&role.runtime_dir, gpu_sample.as_ref());

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
            role.engine_ports,
        );
        models.write_into(&mut host);
        crate::standalone_config::apply_stated_queue(&mut host, &stated_host);
        // ADR 0028 §3: the host's group policy, by the shared writer.
        role.groups
            .write_into(&mut host)
            .map_err(|error| StartError::Deploy(format!("host policy invalid: {error}")))?;
        crate::standalone_config::apply_stated_memory(&mut host, &stated_host, capacity_bytes)
            .map_err(|error| StartError::Deploy(format!("host policy invalid: {error}")))?;
        // The host's own policy, normalized exactly as resolution normalizes
        // it (`resolve_effective(..).host` is this same value). Nothing here
        // sizes a deployment: a card too small for any template boots and
        // refuses each deployment with `insufficient_device_memory` and its
        // numbers, instead of the whole start failing on a sized probe.
        capyctl_config::effective::normalize_host_policy(&host)
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
    // ADR 0023 §3: the private TensorFold build cache root.
    prepare_private_dir(&state_dir.join("engines"))?;
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
        // SPEC §6.5 (W5): the document's idle policy, off unless named.
        idle: capyctl_store::ordinary_lifecycle::park::IdlePolicy {
            ready_idle_ms: idle.ready_idle.map(|d| d.as_millis() as i64),
            parked_idle_ms: idle.parked_idle.map(|d| d.as_millis() as i64),
        },
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
        let engine_ports = role.engine_ports;
        let groups = role.groups.clone();
        let stated = stated_host.clone();
        tokio::task::spawn_blocking(move || {
            crate::standalone_engines::EmbeddedHost::new(
                named,
                fingerprint,
                capacity_bytes,
                inventory,
                shape,
                models,
                engine_ports,
                groups,
                stated,
            )
        })
        .await
        .map_err(|_| StartError::Deploy("installation registration did not finish".into()))?
    };
    let bindings: Arc<dyn EngineBindings> = Arc::new(
        capyctl_controller::installation_gate::InstalledBindings::new(
            // Discrete GPU design §6: engines on a device domain are sized
            // against the total of the card the boot sample observed.
            provider.bindings_for_devices(
                system_clock(),
                state_dir.join("logs"),
                role.runtime_dir.clone(),
                device_totals(&gpu_shape),
            ),
            embedded.installations(),
        ),
    );
    // ADR 0014 §7 (WE3): the embedded host measures pending checkpoint digests
    // with the verifier its launches use, so they share one stat cache.
    let mut supervision = crate::shutdown::Supervision::new();
    if let Some(checkpoints) = bindings.checkpoint_verifier() {
        supervision.supervise(
            capyctl_controller::checkpoint_digests::CheckpointDigests::new(
                owner.clone(),
                capyctl_controller::checkpoint_digests::LocalDigests::new(checkpoints),
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
            Some(origin) => capyctl_agent::sources::SourceStore::with_loopback_origin(
                &root,
                models.policy.clone(),
                secrets,
                &origin,
            ),
            None => capyctl_agent::sources::SourceStore::new(&root, models.policy.clone(), secrets),
        };
        supervision.supervise(
            capyctl_controller::model_sources::SourceMaterializer::new(
                owner.clone(),
                capyctl_controller::model_sources::LocalSources::new(store),
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
            .with_process_residency(capyctl_agent::process_residency::ResidencySampler::nvidia()),
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
        capyctl_management::ManagementCredentials::from_trusted_resolver(admin, &api_key)
            .map_err(|_| StartError::MissingCredentials)?;
    // SPEC §4.3 (P3): the Ready engines a previous run left running are adopted
    // by the coordinator at start; this supervisor re-proves each one locally
    // before dispatch reopens.
    supervision.supervise(
        capyctl_controller::local_readiness::LocalReadiness::new(owner.clone())
            .spawn_until(supervision.cancel_signal()),
    );
    // SPEC §13.2 (W13): an embedded engine that exits is closed and settled with
    // verified cleanup. Supervised with readiness, so it ends with it.
    supervision.supervise(
        capyctl_controller::engine_exit::EngineExits::new(coordinator.commands())
            .spawn_local_until(supervision.cancel_signal()),
    );
    let configuration = Arc::new(
        capyctl_management::configuration::SharedConfigurationSource::new_shared(
            owner.clone(),
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
    let switcher = Arc::new(capyctl_controller::switching::Switcher::new(
        coordinator.commands(),
        capyctl_controller::switching::SwitchOptions {
            drain_timeout: switch_drain_timeout,
            ..Default::default()
        },
    ));
    let source = Arc::new(
        capyctl_management::actions::OwnedActionSource::new(configuration, coordinator.commands())
            .map_err(|_| StartError::Deploy("management actions unavailable".into()))?
            .with_switcher(switcher.clone()),
    );
    // SPEC §4.3: the explicit drain of the embedded host, by its published name
    // or the `standalone` alias the CLI uses.
    let drain = capyctl_management::drain::drain_router(
        capyctl_management::ManagementCredentials::from_trusted_resolver(admin, &api_key)
            .map_err(|_| StartError::MissingCredentials)?,
        source.clone(),
        vec![declared_host.name.clone(), "standalone".to_owned()],
    );
    // ADR 0008: the registered installation, shown by standalone status.
    let installation_view = capyctl_management::installation::installation_router(
        capyctl_management::ManagementCredentials::from_trusted_resolver(admin, &api_key)
            .map_err(|_| StartError::MissingCredentials)?,
        embedded.installations(),
    );
    // SPEC §4.2: the same host and engine inventory a server serves, for the
    // one embedded host: its published document (so `engine add` shows at
    // once) and its memory as observed now (one `/proc` read, plus a bounded
    // GPU sample on a discrete-GPU host).
    let hosts_view = capyctl_management::hosts::standalone_hosts_router(
        capyctl_management::ManagementCredentials::from_trusted_resolver(admin, &api_key)
            .map_err(|_| StartError::MissingCredentials)?,
        owner,
        capyctl_management::hosts::StandaloneHost {
            host_id: declared_host.name.clone(),
            document: {
                let embedded = embedded.clone();
                Arc::new(move || embedded.document())
            },
            installations: embedded.installations(),
            domains: {
                let observed = crate::host_observation::observed_domains(&declared_host.domains);
                let (memory, gpu) = (memory.clone(), gpu.clone());
                let host = declared_host.name.clone();
                Arc::new(move || {
                    let (observed, memory, gpu, host) =
                        (observed.clone(), memory.clone(), gpu.clone(), host.clone());
                    Box::pin(async move {
                        let kinds = observed.clone();
                        let Ok(observations) = HostMemoryObservation::with_domains(observed)
                            .with_memory_reader(memory)
                            .with_gpu_sampler(gpu)
                            .observe(host)
                            .await
                        else {
                            return Vec::new();
                        };
                        observations
                            .into_iter()
                            .map(|o| {
                                let device = kinds.iter().any(|d| {
                                    matches!(d, crate::host_observation::ObservedDomain::Device { domain, .. } if *domain == o.domain)
                                });
                                serde_json::json!({
                                    "domain_id": o.domain,
                                    "kind": if device { "device" } else { "host" },
                                    "capacity_bytes": o.capacity_bytes,
                                    "available_bytes": o.available_bytes,
                                })
                            })
                            .collect()
                    })
                })
            },
        },
    );
    // SPEC §17 (M80): the router's per-request latency distributions. The
    // embedded engine has no host ingress or load report, so only the router
    // tier is measured here.
    let inflight = Arc::new(capyctl_router::admission::InFlight::default());
    inflight.latency.set_timing_header(timing_header);
    let latency_view = {
        let recorder = inflight.latency.clone();
        capyctl_management::metrics::latency_router(
            capyctl_management::ManagementCredentials::from_trusted_resolver(admin, &api_key)
                .map_err(|_| StartError::MissingCredentials)?,
            Arc::new(move |deployment: Option<&str>| {
                capyctl_router::timing::latency_report(&recorder, &[], deployment)
            }),
        )
    };
    // ADR 0018 §4, §5: removal retires through the store, as a server does.
    let retirements = Arc::new(capyctl_management::engines::StoreRetirements::new(
        source.clone(),
    ));
    // Design §9: the inference listener's bind and authentication, set when
    // the caller serves it (`App::inference_router`), for status.
    let inference_listener =
        Arc::new(capyctl_management::inference_listener::InferenceListenerView::default());
    let inference_listener_view = capyctl_management::inference_listener::inference_listener_router(
        capyctl_management::ManagementCredentials::from_trusted_resolver(admin, &api_key)
            .map_err(|_| StartError::MissingCredentials)?,
        inference_listener.clone(),
    );
    let management = capyctl_management::lifecycle_router(management_credentials, source)
        .merge(drain)
        .merge(installation_view)
        .merge(hosts_view)
        .merge(latency_view)
        .merge(inference_listener_view);
    let controller = Arc::new(
        CoordinatorLifecycle::new(coordinator.commands())
            .with_switcher(switcher.clone())
            .with_group_stall_timeout(group_stall_timeout),
    );
    let deps = capyctl_router::RouterDeps {
        controller: controller.clone(),
        // Spec §3: a leased port and a per-launch key belong to one launch, so the
        // forwarder is built from what the coordinator recorded for the launch that
        // is running rather than from a table assembled at boot.
        forwards: Arc::new(capyctl_router::forwarders::LiveForwarders::new(
            controller.clone(),
        )),
        limits: capyctl_router::QueueLimits {
            max_requests_per_deployment: capyctl_domain::launch::MAX_REQUESTS_PER_DEPLOYMENT
                as usize,
            max_buffered_bytes_total: 64 * 1024 * 1024,
        },
        api_key: Some(api_key.clone()),
        inflight,
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    };
    let router = capyctl_router::serve_router(deps.clone());
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
    // ADR 0018 §3, §5: the local control channel `capyctl engine add` and
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
            let socket = state_dir.join(capyctl_agent::control_socket::SOCKET_NAME);
            match capyctl_agent::control_socket::ControlServer::bind(&socket) {
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
                    "engine control socket unavailable ({failure}); `capyctl engine add` \
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

    // T41 T03 (ADR 0023 §2): only TensorFold's version is its last word, as
    // `engine add` records it; vLLM and SGLang publish what they print, so
    // the fingerprints pinned by earlier releases still match.
    #[test]
    fn only_tensorfold_publishes_the_last_word_of_its_version() {
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, printed: &str| {
            let path = dir.path().join(name);
            capyctl_config::test_support::write_executable(
                &path,
                format!("#!/bin/sh\nprintf '{printed}'\n"),
                0o700,
            )
            .unwrap();
            path
        };
        let python = script("python3", "Python 3.12.3\\n");
        let vllm = script("vllm", "INFO loading\\n0.29.0\\n");
        let tensorfold = script("tensorfold", "tensorfold 0.6.0\\n\\n");
        assert_eq!(
            probe_fingerprint(&python, Engine::Sglang).unwrap(),
            "Python 3.12.3"
        );
        assert_eq!(
            probe_fingerprint(&vllm, Engine::Vllm).unwrap(),
            "INFO loading\n0.29.0"
        );
        assert_eq!(
            probe_fingerprint(&tensorfold, Engine::Tensorfold).unwrap(),
            "0.6.0"
        );
        assert_eq!(
            engine_version(Engine::Tensorfold, &tensorfold).unwrap(),
            "0.6.0"
        );
        assert_eq!(
            engine_version(Engine::Sglang, &python).unwrap(),
            "Python 3.12.3"
        );
    }

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
        assert!(capyctl_config::defaults::create_standalone_credentials(dir.path()).unwrap());
        let path = dir.path().join("identity/credentials");
        let key = read_api_key(dir.path()).expect("the generated key");
        assert!(!key.is_empty());
        assert_ne!(key, "capyctl-local");
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

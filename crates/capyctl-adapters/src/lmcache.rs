//! ADR 0033: the LMCache server of one vLLM or SGLang launch.
//!
//! The server is a second owned process group of the same launch, started by
//! the engine adapter's Initialize before the engine (§3 "Order"): spawned
//! gated as role `lmcache`, proven up by `GET /status` on its loopback HTTP
//! port within the start bound, its group's listeners checked against
//! loopback (ADR 0031 §7), and only then the engine launched with the
//! connector rendered for it (§4). A server that does not come up, or that
//! listens off loopback, is stopped with a verified exit and the launch runs
//! without LMCache, or fails under `lmcache: required` (§9).
//!
//! [`LmcacheState`] is the launch's shared record: the builder of the launch
//! creates it with what to start, the host's launch-time check may turn it
//! off before Initialize, the adapter starts the server and reports what
//! happened, and the caller reads the report when the launch is Ready or has
//! failed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use capyctl_config::lmcache::{LmcacheOffReason, GIB};
use capyctl_domain::completion::{
    lmcache_members, EffectObservation, ExecutionIdentities, Milestone, Presence, ProcessIdentity,
    LMCACHE_ROLE,
};

use crate::custom::listeners::{check_loopback, ListenerCheck};
use crate::traits::{OwnedProcessLaunch, RenderedCommand, RuntimeCommand, RuntimeError};

/// ADR 0033 §3: the only address either of the server's ports binds.
pub const SERVER_HOST: &str = "127.0.0.1";

/// The file SGLang's connector reads (ADR 0033 §4), in the launch's own
/// private directory.
pub const SGLANG_CONFIG_FILE: &str = "config.yaml";

/// How long one `GET /status` may take; the start bound bounds them all.
const STATUS_TRY: Duration = Duration::from_secs(1);

/// The pause between two `GET /status` tries.
const STATUS_POLL: Duration = Duration::from_millis(250);

/// ADR 0033 §3: the SIGTERM grace a server stopped by its launch gets before
/// SIGKILL (the same bound ADR 0031 §7 gives an exposed custom engine).
pub const SERVER_STOP_GRACE: Duration = Duration::from_secs(5);

/// ADR 0033 §7: the longest SIGTERM grace the engine of a restart-parked
/// launch gets when its park stops it; half the step's remaining budget when
/// that is less.
const ENGINE_STOP_GRACE: Duration = Duration::from_secs(10);

/// The most bytes of engine-log tail an `engine_start_failed` report carries
/// (the wire bound of `LaunchLmcacheReport.detail`).
pub const MAX_DETAIL_BYTES: usize = 2048;

/// What starting one launch's server needs. The builder fills it from the
/// launch's grant (ports, pool) and the profile's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerPlan {
    /// `<environment>/bin/lmcache`, beside the engine's own executable.
    pub lmcache_bin: PathBuf,
    /// The leased request (ZMQ) port.
    pub rpc_port: u16,
    /// The leased admin HTTP port (`/status`, `/metrics`).
    pub http_port: u16,
    /// The L1 pool, whole GiB (ADR 0033 §6).
    pub pool_gib: u64,
    /// The server's `PATH`.
    pub path: String,
    /// The server's `HOME`, when the role has one.
    pub home: Option<String>,
    /// The instance's `CUDA_VISIBLE_DEVICES`, when the launch narrows it.
    pub cuda_visible_devices: Option<String>,
    /// `PCI_BUS_ID` when the instance's GPU is pinned by its driver index,
    /// exactly as the engine gets it, so the index names the same GPU.
    pub cuda_device_order: Option<String>,
    /// The server's own log, beside the engine's (`<incarnation>.lmcache.log`).
    pub log: Option<PathBuf>,
    /// The launch's private directory (`<state>/lmcache/<incarnation>`): SGLang's
    /// connector configuration is written there.
    pub config_dir: PathBuf,
    /// ADR 0033 §6: how long the server may take to answer `GET /status`.
    pub start_bound: Duration,
    /// Where the listener check reads processes (`/proc` on a host).
    pub proc_root: PathBuf,
    /// `lmcache: required`: a server that does not start fails the launch.
    pub required: bool,
}

impl ServerPlan {
    /// The pool in bytes.
    pub fn pool_bytes(&self) -> i64 {
        i64::try_from(self.pool_gib.saturating_mul(GIB)).unwrap_or(i64::MAX)
    }
}

/// What the adapter does about the server at Initialize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LmcacheLaunch {
    /// Start a new server (a launch's first Initialize).
    Start(ServerPlan),
    /// The launch's server already runs (a restart-parked launch's engine is
    /// started again on it, ADR 0033 §7): re-prove it and connect.
    Attached {
        server: Vec<ProcessIdentity>,
        plan: ServerPlan,
    },
}

/// What happened to LMCache for one launch, read by the caller at Ready or
/// at failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LmcacheReport {
    On {
        server: Vec<ProcessIdentity>,
        rpc_port: u16,
        http_port: u16,
        pool_bytes: i64,
    },
    Off {
        reason: LmcacheOffReason,
        /// The closed sentence, the server's failure, or (for
        /// `engine_start_failed`) the redacted engine-log tail, bounded.
        detail: String,
        /// The engine was launched with the connector and failed before Ready
        /// (ADR 0033 §8): the caller relaunches without LMCache.
        connector_failed: bool,
    },
}

/// Per-launch shared state: built by the launch builder, held by the adapter
/// and its caller alike.
#[derive(Debug, Default)]
pub struct LmcacheState {
    launch: Mutex<Option<LmcacheLaunch>>,
    report: Mutex<Option<LmcacheReport>>,
}

impl LmcacheState {
    /// A launch that uses LMCache: Initialize does what `launch` says.
    pub fn new(launch: LmcacheLaunch) -> Arc<Self> {
        Arc::new(Self {
            launch: Mutex::new(Some(launch)),
            report: Mutex::new(None),
        })
    }

    /// A launch without LMCache, for `reason`; nothing is started.
    pub fn off(reason: LmcacheOffReason, detail: String) -> Arc<Self> {
        let state = Self::default();
        state.turn_off(reason, detail);
        Arc::new(state)
    }

    /// ADR 0033 §7: a running launch whose LMCache is on, rebuilt by a host
    /// that keeps no adapter between steps (the remote host's Park and
    /// Restore of a restart-parked launch): the server `server` started on
    /// `plan`, connected. Restore re-proves it before the engine starts again.
    pub fn attached(server: Vec<ProcessIdentity>, plan: ServerPlan) -> Arc<Self> {
        let state = Self::default();
        state.set_report(LmcacheReport::On {
            server: server.clone(),
            rpc_port: plan.rpc_port,
            http_port: plan.http_port,
            pool_bytes: plan.pool_bytes(),
        });
        *lock(&state.launch) = Some(LmcacheLaunch::Attached { server, plan });
        Arc::new(state)
    }

    /// ADR 0033 §1: turn LMCache off for this launch before Initialize (the
    /// launch-time check failed under `auto`); nothing is started.
    pub fn turn_off(&self, reason: LmcacheOffReason, detail: String) {
        *lock(&self.launch) = None;
        self.set_report(LmcacheReport::Off {
            reason,
            detail: bounded(&detail),
            connector_failed: false,
        });
    }

    /// What happened, once something did.
    pub fn report(&self) -> Option<LmcacheReport> {
        lock(&self.report).clone()
    }

    /// What Initialize does about the server.
    pub fn launch(&self) -> Option<LmcacheLaunch> {
        lock(&self.launch).clone()
    }

    /// ADR 0033 §1.7 of the lifecycle design: the server's log beside the
    /// engine's, when the builder named none (`<incarnation>.lmcache.log` for
    /// the engine log `<incarnation>.log`).
    pub fn log_beside(&self, engine_log: &str) {
        let mut launch = lock(&self.launch);
        let plan = match launch.as_mut() {
            Some(LmcacheLaunch::Start(plan)) | Some(LmcacheLaunch::Attached { plan, .. }) => plan,
            None => return,
        };
        if plan.log.is_none() {
            plan.log = Some(server_log_path(Path::new(engine_log)));
        }
    }

    /// Whether LMCache is on for this launch (its server started and the
    /// engine became Ready connected to it).
    pub fn is_on(&self) -> bool {
        matches!(self.report(), Some(LmcacheReport::On { .. }))
    }

    fn set_report(&self, report: LmcacheReport) {
        *lock(&self.report) = Some(report);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The last [`MAX_DETAIL_BYTES`] of `text`, cut at a character boundary.
fn bounded(text: &str) -> String {
    if text.len() <= MAX_DETAIL_BYTES {
        return text.into();
    }
    let mut start = text.len() - MAX_DETAIL_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].into()
}

/// The server's log for the engine log `engine_log`: `<stem>.lmcache.log`
/// in the same directory.
pub fn server_log_path(engine_log: &Path) -> PathBuf {
    let stem = engine_log
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.strip_suffix(".log").unwrap_or(name))
        .unwrap_or("engine");
    engine_log.with_file_name(format!("{stem}.lmcache.log"))
}

/// ADR 0033 §3: the server's command. The argument vector is CapyCTL's alone:
/// loopback request and HTTP ports, the ZMQ transport, the pool, LRU, and an
/// eager pool (`--no-l1-use-lazy`: LMCache 0.5.6 `distributed/config.py`
/// defines `--l1-use-lazy` as a `BooleanOptionalAction` defaulting to true, so
/// the pool the charge covers is allocated at start only when it is negated).
/// Never `--run-script-api-enabled`, never an L2, disk, GDS, Device-DAX or P2P
/// option. The environment is exactly `PATH`, `HOME` and the instance's
/// `CUDA_VISIBLE_DEVICES`, when set; `CAPYCTL_ENGINE_LOG` names the server's
/// log to the launcher, which never passes it to the server.
pub fn render_server(plan: &ServerPlan) -> RenderedCommand {
    let argv = [
        plan.lmcache_bin.to_string_lossy().into_owned(),
        "server".into(),
        "--host".into(),
        SERVER_HOST.into(),
        "--port".into(),
        plan.rpc_port.to_string(),
        "--transport".into(),
        "zmq".into(),
        "--http-host".into(),
        SERVER_HOST.into(),
        "--http-port".into(),
        plan.http_port.to_string(),
        "--l1-size-gb".into(),
        plan.pool_gib.to_string(),
        "--eviction-policy".into(),
        "LRU".into(),
        "--no-l1-use-lazy".into(),
    ]
    .into();
    let mut env = std::collections::BTreeMap::from([("PATH".to_owned(), plan.path.clone())]);
    if let Some(home) = &plan.home {
        env.insert("HOME".into(), home.clone());
    }
    if let Some(devices) = &plan.cuda_visible_devices {
        env.insert("CUDA_VISIBLE_DEVICES".into(), devices.clone());
    }
    if let Some(order) = &plan.cuda_device_order {
        env.insert("CUDA_DEVICE_ORDER".into(), order.clone());
    }
    if let Some(log) = &plan.log {
        env.insert(
            "CAPYCTL_ENGINE_LOG".into(),
            log.to_string_lossy().into_owned(),
        );
    }
    RenderedCommand { argv, env }
}

/// ADR 0033 §4: vLLM's `--kv-transfer-config`: LMCache's external MP
/// connector by module path (never a bundled connector of the same name),
/// `kv_both`, the server's loopback request port, and no autostart (an engine
/// never starts a server CapyCTL does not own). LMCache 0.5.6
/// `lmcache_mp_connector.py` reads exactly `lmcache.mp.host`,
/// `lmcache.mp.port` and `lmcache.mp.autostart` from this extra config.
pub fn vllm_kv_transfer_config(rpc_port: u16) -> String {
    format!(
        "{{\"kv_connector\":\"LMCacheMPConnector\",\
         \"kv_connector_module_path\":\"lmcache.integration.vllm.lmcache_mp_connector\",\
         \"kv_role\":\"kv_both\",\
         \"kv_connector_extra_config\":{{\"lmcache.mp.host\":\"{SERVER_HOST}\",\
         \"lmcache.mp.port\":{rpc_port},\"lmcache.mp.autostart\":false}}}}"
    )
}

/// ADR 0033 §4: SGLang's `--lmcache-config-file`, written into `dir` (made
/// owner-only, 0700) as an owner-only (0600) file holding exactly
/// `mp_host: 127.0.0.1` and `mp_port: <port>`, the two keys LMCache 0.5.6's
/// `load_engine_config_with_overrides` reads for SGLang's MP connector. Never
/// written through a symlink.
pub fn write_sglang_config(dir: &Path, rpc_port: u16) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.uid() != nix::unistd::geteuid().as_raw() {
        return Err(std::io::Error::other(
            "the LMCache directory is not a directory this user owns",
        ));
    }
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = dir.join(SGLANG_CONFIG_FILE);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != nix::unistd::geteuid().as_raw() {
        return Err(std::io::Error::other(
            "the LMCache configuration is not a file this user owns",
        ));
    }
    if metadata.permissions().mode() & 0o7777 != 0o600 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(format!("mp_host: {SERVER_HOST}\nmp_port: {rpc_port}\n").as_bytes())?;
    file.sync_all()?;
    Ok(path)
}

/// The leader of a recorded server group.
fn leader(server: &[ProcessIdentity]) -> Option<&ProcessIdentity> {
    server.iter().find(|identity| identity.role == LMCACHE_ROLE)
}

/// ADR 0033 §3: whether the launch's server is alive now (its `lmcache`
/// leader, by boot id and start ticks). `Unknown` without a recorded leader.
pub async fn server_alive(
    tools: &Arc<dyn OwnedProcessLaunch>,
    server: &[ProcessIdentity],
) -> Presence {
    let Some(leader) = leader(server).cloned() else {
        return Presence::Unknown;
    };
    let tools = tools.clone();
    tokio::task::spawn_blocking(move || tools.present(&leader))
        .await
        .unwrap_or(Presence::Unknown)
}

/// One `GET /status` on the server's loopback HTTP port, never through a proxy.
async fn status_answers(http_port: u16) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(STATUS_TRY)
        .build()
    else {
        return false;
    };
    client
        .get(format!("http://{SERVER_HOST}:{http_port}/status"))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

/// ADR 0033 §3: start the launch's server and prove it: spawn it gated, wait
/// for `GET /status` within the start bound while it lives, observe its
/// group, and run the loopback listener check on that group. Anything else
/// stops it (verified gone) and answers `server_start_failed` or
/// `listener_exposed` with a detail; a stop that cannot be verified says so,
/// and the launch's own stop still owns the server's identities.
pub async fn start_server(
    tools: Arc<dyn OwnedProcessLaunch>,
    incarnation: &str,
    plan: &ServerPlan,
) -> Result<Vec<ProcessIdentity>, (LmcacheOffReason, String)> {
    let cmd = render_server(plan);
    let spawn_tools = tools.clone();
    let key = incarnation.to_owned();
    let leader = tokio::task::spawn_blocking(move || spawn_tools.spawn_lmcache(&key, &cmd))
        .await
        .map_err(|_| RuntimeError::Uncertain("spawn task failed".into()))
        .and_then(|spawned| spawned)
        .map_err(|error| {
            (
                LmcacheOffReason::ServerStartFailed,
                format!("the LMCache server could not be started: {error}"),
            )
        })?;
    let deadline = Instant::now() + plan.start_bound;
    let started = loop {
        match server_alive(&tools, std::slice::from_ref(&leader)).await {
            Presence::Alive => {}
            Presence::Gone => {
                break Err((
                    LmcacheOffReason::ServerStartFailed,
                    "the LMCache server exited before it answered GET /status".to_owned(),
                ))
            }
            Presence::Unknown => {
                break Err((
                    LmcacheOffReason::ServerStartFailed,
                    "the LMCache server's presence could not be established".to_owned(),
                ))
            }
        }
        if status_answers(plan.http_port).await {
            break Ok(());
        }
        if Instant::now() >= deadline {
            break Err((
                LmcacheOffReason::ServerStartFailed,
                format!(
                    "the LMCache server did not answer GET /status within {} s",
                    plan.start_bound.as_secs_f64()
                ),
            ));
        }
        tokio::time::sleep(STATUS_POLL).await;
    };
    let failed = match started {
        Ok(()) => match checked_group(&tools, plan, &leader).await {
            Ok(group) => return Ok(group),
            Err(failure) => failure,
        },
        Err((reason, detail)) => (reason, detail, vec![leader]),
    };
    let (reason, detail, stopping) = failed;
    Err((reason, stop_after(&tools, stopping, detail).await))
}

/// ADR 0033 §§3, 5: re-prove a running server (a restart-parked launch's
/// wake, a host's re-proof): its leader alive, `GET /status` answering and
/// its group's listeners on loopback. An exposed server is stopped (verified
/// gone); one that does not answer is left to the launch's stop.
pub async fn prove_server(
    tools: Arc<dyn OwnedProcessLaunch>,
    plan: &ServerPlan,
    server: &[ProcessIdentity],
) -> Result<Vec<ProcessIdentity>, (LmcacheOffReason, String)> {
    let Some(leader) = leader(server).cloned() else {
        return Err((
            LmcacheOffReason::ServerStartFailed,
            "no LMCache server is recorded for the launch".into(),
        ));
    };
    if server_alive(&tools, server).await != Presence::Alive {
        return Err((
            LmcacheOffReason::ServerStartFailed,
            "the LMCache server is not running".into(),
        ));
    }
    if !status_answers(plan.http_port).await {
        return Err((
            LmcacheOffReason::ServerStartFailed,
            "the LMCache server did not answer GET /status".into(),
        ));
    }
    match checked_group(&tools, plan, &leader).await {
        Ok(group) => Ok(group),
        Err((reason @ LmcacheOffReason::ListenerExposed, detail, stopping)) => {
            Err((reason, stop_after(&tools, stopping, detail).await))
        }
        Err((reason, detail, _)) => Err((reason, detail)),
    }
}

type GroupFailure = (LmcacheOffReason, String, Vec<ProcessIdentity>);

/// The server's group, observed and checked against loopback (ADR 0031 §7).
async fn checked_group(
    tools: &Arc<dyn OwnedProcessLaunch>,
    plan: &ServerPlan,
    leader: &ProcessIdentity,
) -> Result<Vec<ProcessIdentity>, GroupFailure> {
    let observe_tools = tools.clone();
    let led_by = leader.clone();
    let group = tokio::task::spawn_blocking(move || observe_tools.observe_lmcache_group(&led_by))
        .await
        .map_err(|_| RuntimeError::Uncertain("group task failed".into()))
        .and_then(|observed| observed)
        .map_err(|error| {
            (
                LmcacheOffReason::ServerStartFailed,
                format!("the LMCache server's process group could not be observed: {error}"),
                vec![leader.clone()],
            )
        })?;
    if group.first() != Some(leader) {
        return Err((
            LmcacheOffReason::ServerStartFailed,
            "the LMCache server's process group has no live leader".into(),
            vec![leader.clone()],
        ));
    }
    let root = plan.proc_root.clone();
    let checked = group.clone();
    let check_tools = tools.clone();
    let check = tokio::task::spawn_blocking(move || {
        check_loopback(&root, &checked, &|identity| {
            check_tools.present(identity) == Presence::Alive
        })
    })
    .await
    .map_err(|_| {
        (
            LmcacheOffReason::ServerStartFailed,
            "the listener check did not run".to_owned(),
            group.clone(),
        )
    })?;
    match check {
        Ok(()) => Ok(group),
        Err(ListenerCheck::Exposed(address)) => Err((
            LmcacheOffReason::ListenerExposed,
            format!("the LMCache server listens on {address}, not on loopback"),
            group,
        )),
        Err(ListenerCheck::Unreadable(reason)) => Err((
            LmcacheOffReason::ServerStartFailed,
            format!("the LMCache server's listeners could not be read: {reason}"),
            group,
        )),
    }
}

/// Stop `server` (verified gone) and say how that went after `detail`.
async fn stop_after(
    tools: &Arc<dyn OwnedProcessLaunch>,
    server: Vec<ProcessIdentity>,
    detail: String,
) -> String {
    let stop_tools = tools.clone();
    let stopped =
        tokio::task::spawn_blocking(move || stop_tools.stop_group(&server, SERVER_STOP_GRACE))
            .await
            .map_err(|_| RuntimeError::Uncertain("stop task failed".into()))
            .and_then(|stopped| stopped);
    match stopped {
        Ok(()) => format!("{detail}; the server was stopped"),
        Err(error) => format!(
            "{detail}; stopping the server was not verified ({error}); the launch's stop still owns it"
        ),
    }
}

/// The running server an engine connects to (ADR 0033 §2.2 of the design).
pub(crate) struct Connector {
    state: Arc<LmcacheState>,
    server: Vec<ProcessIdentity>,
    plan: ServerPlan,
    /// A launch's first engine: a failure before Ready is ADR 0033 §8's
    /// engine start failure with the connector. A restart-parked launch's
    /// wake fails as a wake instead.
    first_start: bool,
}

/// ADR 0033 §§3, 9: before the engine is rendered, start (or re-prove) the
/// launch's server. `None`: the launch runs without LMCache (none planned, or
/// the server did not start under `auto`, which turns the state off).
/// `lmcache: required` turns a server that did not start into the launch
/// failure `lmcache_unavailable:<reason>`, before any engine is spawned.
pub(crate) async fn connect(
    state: Option<&Arc<LmcacheState>>,
    tools: &Arc<dyn OwnedProcessLaunch>,
    incarnation: &str,
    binding_id: &str,
) -> Result<Option<Connector>, RuntimeError> {
    let Some(state) = state else {
        return Ok(None);
    };
    match state.launch() {
        None => Ok(None),
        Some(LmcacheLaunch::Start(plan)) => {
            match start_server(tools.clone(), incarnation, &plan).await {
                Ok(server) => Ok(Some(Connector {
                    state: state.clone(),
                    server,
                    plan,
                    first_start: true,
                })),
                Err((reason, detail)) => {
                    state.turn_off(reason, detail.clone());
                    if plan.required {
                        // ADR 0033 §9: nothing is left running; the engine is
                        // never spawned.
                        return Err(RuntimeError::LaunchFailed(format!(
                            "{}; {detail}",
                            reason.required_refusal()
                        )));
                    }
                    capyctl_domain::role_log::notice(
                        capyctl_domain::role_log::Level::Warning,
                        &format!(
                            "LMCache off for binding {binding_id} ({}): {detail}",
                            reason.code()
                        ),
                    );
                    Ok(None)
                }
            }
        }
        Some(LmcacheLaunch::Attached { server, plan }) => {
            match prove_server(tools.clone(), &plan, &server).await {
                Ok(server) => Ok(Some(Connector {
                    state: state.clone(),
                    server,
                    plan,
                    first_start: false,
                })),
                // ADR 0033 §7: the engine of a restart-parked launch only
                // starts again on its own server; without it the wake fails.
                Err((reason, detail)) => Err(RuntimeError::LaunchFailed(format!(
                    "lmcache_unavailable:{}; {detail}",
                    reason.code()
                ))),
            }
        }
    }
}

impl Connector {
    pub(crate) fn rpc_port(&self) -> u16 {
        self.plan.rpc_port
    }

    pub(crate) fn plan(&self) -> &ServerPlan {
        &self.plan
    }

    /// ADR 0033 §3: whether the server has exited (a server exiting while
    /// its engine starts is an engine start failure with the connector).
    pub(crate) async fn server_gone(&self, tools: &Arc<dyn OwnedProcessLaunch>) -> bool {
        server_alive(tools, &self.server).await == Presence::Gone
    }

    /// The failure a server that exited while the engine started is reported as.
    pub(crate) fn server_exited(&self, engine_log: Option<&str>) -> RuntimeError {
        let tail = crate::launch_failure::log_tail(engine_log);
        RuntimeError::LaunchFailed(format!(
            "the LMCache server exited while the engine was starting; log tail:\n{tail}"
        ))
    }

    /// ADR 0033 §8: the engine, launched with the connector, failed before
    /// Ready: the report says so with the engine log's redacted tail, and the
    /// step's own error is returned unchanged.
    pub(crate) fn engine_failed(
        &self,
        engine_log: Option<&str>,
        error: RuntimeError,
    ) -> RuntimeError {
        if self.first_start {
            self.state.set_report(LmcacheReport::Off {
                reason: LmcacheOffReason::EngineStartFailed,
                detail: bounded(&crate::launch_failure::log_tail(engine_log)),
                connector_failed: true,
            });
        }
        error
    }

    /// The engine is Ready connected to the server: LMCache is on, and the
    /// launch's identities are the engine group and the server group.
    pub(crate) fn ready(&self, engine: Vec<ProcessIdentity>) -> Vec<ProcessIdentity> {
        self.state.set_report(LmcacheReport::On {
            server: self.server.clone(),
            rpc_port: self.plan.rpc_port,
            http_port: self.plan.http_port,
            pool_bytes: self.plan.pool_bytes(),
        });
        *lock(&self.state.launch) = Some(LmcacheLaunch::Attached {
            server: self.server.clone(),
            plan: self.plan.clone(),
        });
        engine
            .into_iter()
            .chain(self.server.iter().cloned())
            .collect()
    }
}

/// `connector`'s [`Connector::engine_failed`], or `error` unchanged without one.
pub(crate) fn engine_failed(
    connector: Option<&Connector>,
    engine_log: Option<&str>,
    error: RuntimeError,
) -> RuntimeError {
    match connector {
        Some(connector) => connector.engine_failed(engine_log, error),
        None => error,
    }
}

/// ADR 0033 §7: a restart-parked launch's Park: stop the engine's group and
/// its helpers (verified gone), keep the server. The evidence names the
/// server group, which is all the launch holds while parked.
pub(crate) async fn park_engine(
    tools: Arc<dyn OwnedProcessLaunch>,
    command: &RuntimeCommand,
) -> Result<EffectObservation, RuntimeError> {
    let c = &command.context;
    let ExecutionIdentities::Retained(identities) = &c.identities else {
        return Err(RuntimeError::Unsupported);
    };
    let server = lmcache_members(identities);
    let engine: Vec<ProcessIdentity> = identities
        .iter()
        .filter(|identity| !identity.is_lmcache())
        .cloned()
        .collect();
    if leader(&server).is_none() || !engine.iter().any(|identity| identity.role == "api") {
        return Err(RuntimeError::Unsupported);
    }
    if server_alive(&tools, &server).await != Presence::Alive {
        return Err(RuntimeError::Uncertain(
            "the LMCache server of a restart-parked launch is not running".into(),
        ));
    }
    let left = Duration::from_millis(u64::try_from(c.deadline_ms - now_ms()?).unwrap_or(0));
    let grace = ENGINE_STOP_GRACE.min(left / 2);
    let stop_tools = tools.clone();
    tokio::task::spawn_blocking(move || stop_tools.stop_group(&engine, grace))
        .await
        .map_err(|_| RuntimeError::Uncertain("stop task failed".into()))??;
    Ok(EffectObservation {
        token: c.token.clone(),
        binding_id: c.binding_id.clone(),
        incarnation: c.incarnation.clone(),
        identities: server,
        observed_at_ms: now_ms()?,
        receipt: format!(
            "lmcache-restart-park-v1:Park:{}; engine stopped, server kept",
            c.token.step_id
        ),
        facts: vec![Milestone::Quiesced, Milestone::MemoryReleased],
        kernel_builds: Vec::new(),
    })
}

/// A restart-parked launch's Restore context: the server group the launch
/// still holds, nothing of an engine.
pub(crate) fn restore_server(
    command: &RuntimeCommand,
) -> Result<Vec<ProcessIdentity>, RuntimeError> {
    let c = &command.context;
    let ExecutionIdentities::Retained(identities) = &c.identities else {
        return Err(RuntimeError::Unsupported);
    };
    if leader(identities).is_none()
        || identities.iter().any(|identity| !identity.is_lmcache())
        || c.launch_settings.is_some()
        || c.token.step_id.is_empty()
        || c.incarnation.is_empty()
    {
        return Err(RuntimeError::Unsupported);
    }
    Ok(identities.clone())
}

fn now_ms() -> Result<i64, RuntimeError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| RuntimeError::Uncertain("system clock is before the epoch".into()))
        .map(|d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_log_sits_beside_the_engine_log() {
        assert_eq!(
            server_log_path(Path::new("/var/log/capyctl/dep/01ABC.log")),
            PathBuf::from("/var/log/capyctl/dep/01ABC.lmcache.log")
        );
    }

    #[test]
    fn a_detail_is_bounded_at_a_character_boundary() {
        let text = format!("é{}", "x".repeat(MAX_DETAIL_BYTES));
        let cut = bounded(&text);
        assert!(cut.len() <= MAX_DETAIL_BYTES);
        assert!(cut.ends_with('x'));
    }
}

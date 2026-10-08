//! SPEC §§3–4, §14–15: foreground remote roles and enrollment through the product.
use crate::{
    grammar::{Command, InitTarget, Invocation, ListResource, Resource, Role},
    output::StructuredError,
};
use capyctl_agent::{
    enrollment::{self, JoinInvitation, PendingEnrollment},
    identity::HostKey,
    identity_storage::IdentityDirectory,
    journal::HostJournal,
};
use capyctl_config::remote_roles::{HostConfig, ServerConfig};
use capyctl_controller::{
    agent_sessions::AgentSessions, enrollment::EnrollmentAuthority, OwnedCoordinatorState,
};
use capyctl_management::ManagementCredentials;
use capyctl_protocol::pb::{
    self, agent_control_server::AgentControlServer, bootstrap_server::BootstrapServer,
    host_identity_server::HostIdentityServer,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

fn error(message: &str) -> StructuredError {
    StructuredError {
        code: "invalid_config",
        message: message.into(),
    }
}
fn from_newer_version(message: String) -> StructuredError {
    StructuredError {
        code: crate::output::STORE_FROM_NEWER_VERSION,
        message,
    }
}
/// SPEC §4.1, ADR 0016 (owner decision 2026-09-24): the one line a host role
/// logs when the controller answers that its certificate is revoked, before it
/// exits with [`crate::output::ExitCode::HOST_REVOKED`] instead of retrying.
/// Its engines are left running, owned and journaled, for recovery to re-prove.
pub fn host_revoked(host: &str) -> StructuredError {
    StructuredError {
        code: crate::output::HOST_REVOKED,
        message: format!(
            "Host {host} is revoked; its engines keep running. To recover the same identity, run `capyctl invite host {host} --recover --output FILE` on the server for a new recovery invitation, then `capyctl join host --join-file FILE --recover` on this host, and start the host again"
        ),
    }
}
/// SPEC §13.1: identity storage refuses an unsafe or busy directory; the
/// message names the path and the check that failed so the operator can act.
fn identity_refused(
    what: &str,
    path: &Path,
    refusal: &capyctl_agent::identity_storage::StorageError,
    hint: &str,
) -> StructuredError {
    error(&format!(
        "{what} refused: {}{hint}",
        capyctl_agent::identity_storage::describe_refusal(path, refusal)
    ))
}
/// A listener that cannot bind names itself, its address and the reason, so
/// a port another process holds is not reported as a generic failure.
fn listen_failed(
    listener: &str,
    address: std::net::SocketAddr,
    failure: &std::io::Error,
) -> StructuredError {
    let reason = match failure.kind() {
        std::io::ErrorKind::AddrInUse => {
            "the address is already in use (another capyctl role or program listens there)"
                .to_owned()
        }
        std::io::ErrorKind::AddrNotAvailable => {
            "the address does not belong to this machine".to_owned()
        }
        _ => failure.to_string(),
    };
    StructuredError {
        code: "management_unavailable",
        message: format!("Cannot listen on {address} for the {listener} listener: {reason}"),
    }
}
fn unavailable() -> StructuredError {
    StructuredError {
        code: "management_unavailable",
        message: "Remote role operation failed; inspect the saved configuration and identity"
            .into(),
    }
}
fn now() -> i64 {
    capyctl_protocol::now_unix_ms() / 1000
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    version: u32,
    admin_token: String,
    api_key: String,
}
fn token() -> String {
    capyctl_store::secrets::new_engine_key()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn private_dir(path: &Path) -> Result<(), StructuredError> {
    match fs::symlink_metadata(path) {
        Ok(meta)
            if meta.is_dir()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o7777 == 0o700 => {}
        Ok(_) => {
            return Err(error(
                "Existing role state directory is unsafe; no permissions were changed",
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| error("Role state requires an absolute path"))?;
            if !parent.exists() {
                private_dir(parent)?;
            }
            let mut builder = fs::DirBuilder::new();
            match builder.mode(0o700).create(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return private_dir(path)
                }
                Err(_) => return Err(unavailable()),
            }
        }
        Err(_) => return Err(unavailable()),
    }
    if path.canonicalize().map_err(|_| unavailable())? != path {
        return Err(error("Role state path must be canonical"));
    }
    Ok(())
}
/// Owner-protected bounded reads without taking the running service's exclusive lock.
fn private_read(path: &Path) -> Result<Vec<u8>, StructuredError> {
    // A bare relative name (`--join-file host.join`) has an empty parent, which
    // names the working directory, not an unreadable path.
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()
        .map_err(|_| unavailable())?;
    for ancestor in parent.ancestors() {
        let m = fs::symlink_metadata(ancestor).map_err(|_| unavailable())?;
        if !m.is_dir()
            || ![0, unsafe { libc::geteuid() }].contains(&m.uid())
            || m.mode() & 0o022 != 0
        {
            return Err(error(&format!(
                "Unsafe private file directory: {} (holding {}) can be written by other users",
                ancestor.display(),
                path.display()
            )));
        }
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| StructuredError {
            code: "management_unavailable",
            message: format!("Cannot open private file {}: {e}", path.display()),
        })?;
    let m = file.metadata().map_err(|_| unavailable())?;
    let refusal = if !m.is_file() {
        Some("it is not a regular file".to_owned())
    } else if m.uid() != unsafe { libc::geteuid() } {
        Some("it is owned by another user".to_owned())
    } else if m.mode() & 0o7777 != 0o600 {
        Some(format!(
            "it has mode {:04o}; run `chmod 600 {}`",
            m.mode() & 0o7777,
            path.display()
        ))
    } else if m.nlink() != 1 {
        Some("it has more than one hard link".to_owned())
    } else if m.len() == 0 || m.len() > 131072 {
        Some("it is empty or larger than 128 KiB".to_owned())
    } else {
        None
    };
    if let Some(reason) = refusal {
        return Err(error(&format!(
            "Unsafe or incomplete private file {}: {reason}",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(131073)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable())?;
    if bytes.len() > 131072 {
        return Err(error("Private file exceeds its bound"));
    }
    Ok(bytes)
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), StructuredError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| unavailable())?;
    file.write_all(bytes).map_err(|_| unavailable())?;
    file.as_file().sync_all().map_err(|_| unavailable())?;
    file.persist_noclobber(path).map_err(|_| {
        error("Output already exists or cannot be created; nothing was overwritten")
    })?;
    fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|_| unavailable())?;
    Ok(())
}
fn read_config(path: &Path) -> Result<String, StructuredError> {
    let metadata =
        fs::metadata(path).map_err(|_| error("Explicit or saved role configuration is missing"))?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(error("Invalid role configuration file"));
    }
    fs::read_to_string(path).map_err(|_| error("Cannot read role configuration"))
}
/// The process environment as the role reads it (an empty value is unset).
fn role_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}
/// ADR 0018 §2: a host's engines file, by the same rule as `capyctl engine`:
/// beside the named document, else `<config home>/capyctl/engines.yaml`. With
/// no config home at all it stays beside the document the host loads.
fn host_engines(named: Option<&Path>, document: &Path) -> PathBuf {
    crate::engine::role_engines(named, &role_env)
        .unwrap_or_else(|| capyctl_config::registration::engines_beside(document))
}
/// Final review I8-bis (every setting three ways, standalone's precedence): a
/// server's or host's state directory is `--state-dir`, else
/// `CAPYCTL_STATE_DIR`, else the document's `state_dir`. When a named form
/// overrides the document, the winner is used and one notice names the
/// overridden value. `None` when the document's own directory stands.
fn state_dir_override(invocation: &Invocation, document_state_dir: &Path) -> Option<PathBuf> {
    let (winner, source) = match invocation.state_dir.clone() {
        Some(dir) => (dir, "--state-dir"),
        None => (
            std::env::var_os("CAPYCTL_STATE_DIR")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)?,
            "CAPYCTL_STATE_DIR",
        ),
    };
    let winner = crate::engine::absolute(&winner);
    if winner == crate::engine::absolute(document_state_dir) {
        return None;
    }
    capyctl_domain::role_log::notice(
        capyctl_domain::role_log::Level::Notice,
        &format!(
            "state directory {} from {source} overrides the document's state_dir {}",
            winner.display(),
            document_state_dir.display()
        ),
    );
    Some(winner)
}
fn implicit(root: &Path, role: &str) -> PathBuf {
    root.join("config").join(format!("{role}.yaml"))
}
/// Owner rule 2026-09-25 (standalone is a server plus one host, with the
/// same defaults): the document `init host` writes validates as written. The
/// models directory and model downloads are left to the shared defaults
/// (`~/models`, downloads in `<model_store>/sources`, allowed; each settable
/// three ways at start), and the resource policy is derived from this
/// machine's memory and GPU shape exactly as standalone derives its own.
fn host_template(root: &Path) -> Result<String, StructuredError> {
    let mut document: Value =
        serde_json::from_str(&HostConfig::template(root)).map_err(|_| unavailable())?;
    if let Some(fields) = document.as_object_mut() {
        fields.remove("model_store");
    }
    let capacity = capyctl_agent::memory::read_host_memory()
        .map_err(|_| error("Host memory inventory unavailable"))?
        .memory
        .capacity_bytes;
    let sample = capyctl_agent::gpu_memory::sample();
    let shape =
        capyctl_agent::gpu_memory::shape(sample.as_ref()).map_err(|e| error(&e.to_string()))?;
    document["hardware_fingerprint"] = json!("capyctl-host");
    document["environment_fingerprint"] = json!("capyctl-host");
    document["resource_policy"] = crate::standalone_config::resource_policy(
        capacity,
        None,
        &shape,
        capyctl_config::engine_settings::DEFAULT_ENGINE_PORTS,
    );
    serde_json::to_string_pretty(&document).map_err(|_| unavailable())
}

fn initialize(root: &Path, role: InitTarget, output: &Path) -> Result<Value, StructuredError> {
    if fs::symlink_metadata(output).is_ok() {
        return Err(error("Output already exists; nothing was overwritten"));
    }
    let text = match role {
        InitTarget::Server => ServerConfig::template(root),
        InitTarget::Host => host_template(root)?,
    };
    match role {
        InitTarget::Server => {
            ServerConfig::parse(&text).map_err(|_| error("Invalid server initialization path"))?;
        }
        InitTarget::Host => {
            HostConfig::parse(&text).map_err(|_| error("Invalid host initialization path"))?;
        }
    }
    // SPEC §15.3: complete semantic validation before creating any state.
    private_dir(root)?;
    private_dir(&root.join("identity"))?;
    let storage = IdentityDirectory::open(&root.join("identity"))
        .map_err(|e| identity_refused("Role identity", &root.join("identity"), &e, ""))?;
    if role == InitTarget::Server {
        if storage
            .read_bundle("server-credentials.json")
            .map_err(|_| unavailable())?
            .is_some()
        {
            return Err(error(
                "Server identity already exists; use its saved configuration",
            ));
        }
        enrollment::initialize_controller_ca(&storage, now())
            .map_err(|_| error("Server CA already exists or cannot be initialized"))?;
        let credentials = Credentials {
            version: 1,
            admin_token: token(),
            api_key: token(),
        };
        storage
            .create_bundle(
                "server-credentials.json",
                &serde_json::to_vec(&credentials).map_err(|_| unavailable())?,
            )
            .map_err(|_| unavailable())?;
        storage
            .create_bundle("secrets.key", &capyctl_store::secrets::new_engine_key())
            .map_err(|_| unavailable())?;
    }
    if output
        == implicit(
            root,
            if role == InitTarget::Server {
                "server"
            } else {
                "host"
            },
        )
    {
        private_dir(&root.join("config"))?;
    }
    // SPEC §3.3 / ADR 0001: the host template names no runtime_dir, so it
    // runs from the managed copy of the embedded runtime.
    let runtime = (role == InitTarget::Host).then(|| root.join("runtime"));
    if let Some(dir) = &runtime {
        crate::managed_runtime::prepare_for_role(dir)?;
    }
    // A `.yaml` or `.yml` document is written as YAML; any other name keeps
    // the JSON the templates are built as.
    let text = if matches!(
        output.extension().and_then(|e| e.to_str()),
        Some("yaml" | "yml")
    ) {
        let tree: Value = serde_json::from_str(&text).map_err(|_| unavailable())?;
        capyctl_config::yaml_emit::to_block_yaml(&tree)
    } else {
        text
    };
    write_new(output, text.as_bytes())?;
    let mut result = json!({"config":output,"state_dir":root,"initialized":true});
    if let Some(dir) = runtime {
        result["runtime_dir"] = json!(dir);
    }
    Ok(result)
}
fn load_credentials(config: &ServerConfig) -> Result<Credentials, StructuredError> {
    let c: Credentials = serde_json::from_slice(&private_read(
        &config.identity_dir.join("server-credentials.json"),
    )?)
    .map_err(|_| unavailable())?;
    if c.version != 1
        || ManagementCredentials::from_trusted_resolver(&c.admin_token, &c.api_key).is_err()
    {
        return Err(error("Invalid saved server credentials"));
    }
    Ok(c)
}
pub(crate) fn management_context(
    config: &ServerConfig,
) -> Result<(String, String), StructuredError> {
    // Final review I8: `CAPYCTL_MANAGEMENT_ADDR`, else the address the server
    // recorded when it started (a `--management-listen` start), else the
    // document's, as for standalone.
    let address = crate::roles::management_override(None)
        .map_err(|e| error(&e.to_string()))?
        .or_else(|| crate::roles::recorded_management_address(&config.state_dir))
        .unwrap_or(config.management);
    Ok((
        format!("http://{address}/management/v1"),
        load_credentials(config)?.admin_token,
    ))
}
fn management_credentials(c: &Credentials) -> Result<ManagementCredentials, StructuredError> {
    ManagementCredentials::from_trusted_resolver(&c.admin_token, &c.api_key)
        .map_err(|_| unavailable())
}
/// How often the server re-reads the hosts' queue policies (W10 gap b).
const QUEUE_POLICY_REFRESH: Duration = Duration::from_secs(5);

/// Aborts a supervision task when dropped, so it ends with the role.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// SPEC §10 step 1, §16.2 (W10 gap b): the router's waiting-request bounds
/// from a host queue policy. The deadline a waiting request is given is the
/// policy's request deadline.
pub(crate) fn wait_limits(
    queue: &capyctl_config::effective::QueuePolicy,
) -> capyctl_router::queue::WaitLimits {
    capyctl_router::queue::WaitLimits {
        max_pending_per_deployment: queue.max_pending_per_deployment as usize,
        max_pending_total: queue.max_pending_total as usize,
        max_buffered_bytes_total: usize::try_from(queue.max_buffered_bytes_total).unwrap_or(0),
        deadline: Duration::from_millis(u64::try_from(queue.request_deadline_ms).unwrap_or(0)),
        stream_idle: Duration::from_millis(u64::try_from(queue.stream_idle_ms).unwrap_or(0)),
    }
}

/// SPEC §§10, 17 (owner decision 2026-10-08): the management load read: the
/// store's deployments, instances and derived running limits, the router's
/// in-flight and waiting counts and the latest engine samples in `loads`
/// (reported by hosts, or sampled by standalone from its own engines). The
/// store is read under the owner lock; the report is composed after it is
/// released.
pub(crate) fn load_view(
    credentials: ManagementCredentials,
    owner: Arc<Mutex<capyctl_controller::OwnedCoordinatorState>>,
    inflight: Arc<capyctl_router::admission::InFlight>,
    loads: Arc<capyctl_controller::load_table::LoadTable>,
) -> axum::Router {
    use capyctl_management::metrics::LoadRead;
    capyctl_management::metrics::load_router(
        credentials,
        Arc::new(move |deployment: Option<&str>| {
            let read = match owner.lock() {
                Ok(owner) => owner.store().capacity(deployment),
                Err(_) => return LoadRead::Unavailable,
            };
            match read {
                Ok(found) if deployment.is_some() && found.is_empty() => {
                    LoadRead::UnknownDeployment
                }
                Ok(found) => LoadRead::Report(capyctl_router::capacity::capacity_report(
                    &found,
                    &inflight,
                    capyctl_domain::launch::MAX_REQUESTS_PER_DEPLOYMENT as usize,
                    Some(&loads),
                    capyctl_protocol::now_unix_ms(),
                )),
                Err(_) => LoadRead::Unavailable,
            }
        }),
    )
}

async fn serve_server(config: ServerConfig) -> Result<Value, StructuredError> {
    let storage = IdentityDirectory::open(&config.identity_dir).map_err(|e| {
        identity_refused(
            "Server identity",
            &config.identity_dir,
            &e,
            "; run init server first",
        )
    })?;
    let credentials = load_credentials(&config)?;
    let ca = enrollment::load_controller_ca(&storage)
        .map_err(|_| error("Server CA is missing or invalid; explicit recovery is required"))?;
    let key = HostKey::generate().map_err(|_| unavailable())?;
    let certificate = ca
        .issue_server(&config.certificate_name, &key, now())
        .map_err(|_| unavailable())?;
    // Reopening never regenerates the sealing key of an existing controller.
    private_read(&config.identity_dir.join("secrets.key"))?;
    let secrets = capyctl_store::secrets::SecretsKey::load_or_create(
        &config.identity_dir.join("secrets.key"),
    )
    .map_err(|_| unavailable())?;
    let owner = Arc::new(Mutex::new(
        OwnedCoordinatorState::open_with_secrets(&config.state_dir, secrets).map_err(
            |failure| match failure {
                // SPEC §13.2 / T33: say what happened and how to recover.
                capyctl_controller::OwnedStateError::Store(
                    newer @ capyctl_store::StoreError::FromNewerVersion { .. },
                ) => from_newer_version(newer.to_string()),
                _ => error("Controller state is unsafe or already owned"),
            },
        )?,
    ));
    let ca_pem = ca.certificate_pem().to_owned();
    let authority = Arc::new(EnrollmentAuthority::new(owner.clone(), ca));
    // Owner decision 2026-09-23: heartbeats every second on each control
    // session; silence suspends a host's dispatch, then loses its session.
    let sessions = AgentSessions::with_heartbeats(
        authority.clone(),
        capyctl_controller::agent_sessions::HeartbeatPolicy {
            interval: capyctl_config::remote_roles::HEARTBEAT_INTERVAL,
            suspend_after: config.heartbeat.suspend_after,
            lost_after: config.heartbeat.lost_after,
        },
    );
    let bindings = capyctl_controller::remote_execution::RemoteProfileBindings::new(
        owner.clone(),
        sessions.clone(),
        authority.controller_id(),
    );
    let readiness = bindings.readiness();
    let coordinator =
        capyctl_controller::coordinator::OwnedCoordinator::spawn_with_execution_bindings(
            owner.clone(),
            sessions.clone(),
            Arc::new(|| Ok(capyctl_protocol::now_unix_ms())),
            capyctl_controller::coordinator::CoordinatorOptions {
                // ADR 0014 amendment A1: a ceiling only; each Initialize is bounded
                // by its step deadline, the deployment's `timeouts.initialize` or
                // the operator's override, within the host's request deadline.
                initialize_timeout: Duration::from_secs(3600),
                // SPEC §6.5 (W5): the server document's idle policy, off unless named.
                idle: capyctl_store::ordinary_lifecycle::park::IdlePolicy {
                    ready_idle_ms: config.idle.ready_idle.map(|d| d.as_millis() as i64),
                    parked_idle_ms: config.idle.parked_idle.map(|d| d.as_millis() as i64),
                },
                // SPEC §6.3: a Stop drains accepted requests for the same bound a
                // switch does (`switching.drain_timeout`) before it terminates.
                stop_drain_timeout: config.switch_drain_timeout,
                ..Default::default()
            },
            bindings,
        )
        .map_err(|_| unavailable())?;
    // SPEC §§6.1, 13.2 (G2, D8): a Ready remote engine dispatches only while the
    // host session that proved it is current; a new session must re-prove it.
    let supervisor = capyctl_controller::remote_readiness::RemoteReadiness::new(
        owner.clone(),
        sessions.clone(),
        authority.controller_id(),
        readiness,
    );
    // SPEC §4.3: a host's drain notice closes dispatch to its engines before
    // the host is acknowledged and closes its ingress.
    {
        let supervisor = supervisor.clone();
        sessions.on_host_draining(Arc::new(move |host: &str| supervisor.suspend_host(host)));
    }
    // Owner decision 2026-09-23: a host silent past the suspend bound has its
    // dispatch suspended and its readiness re-proven when it is heard again.
    {
        let supervisor = supervisor.clone();
        sessions.on_host_unresponsive(Arc::new(move |host: &str| {
            supervisor.suspend_unresponsive_host(host)
        }));
    }
    // SPEC §13.2 (W13): a host's report that an owned engine exited closes that
    // instance's dispatch and settles it with verified cleanup.
    {
        let exits = capyctl_controller::engine_exit::EngineExits::new(coordinator.commands());
        sessions.on_member_exit(Arc::new(move |host: &str, exit| {
            exits.remote(host, exit);
        }));
    }
    // ADR 0015 invariant 6: the server's supervisors share one cancel signal
    // and are joined at shutdown (`crate::shutdown::Supervision`).
    let mut supervision = crate::shutdown::Supervision::new();
    supervision.supervise(supervisor.spawn_until(supervision.cancel_signal()));
    // ADR 0014 §7 (WE3): pending checkpoint digests are measured by the host
    // that holds each checkpoint and recorded here, one measurement per host.
    supervision.supervise(
        capyctl_controller::checkpoint_digests::CheckpointDigests::new(
            owner.clone(),
            capyctl_controller::checkpoint_digests::RemoteDigests::new(
                owner.clone(),
                sessions.clone(),
                authority.controller_id(),
            ),
        )
        .spawn_until(supervision.cancel_signal()),
    );
    // ADR 0008: declared remote model sources are materialized by the host
    // each resolved on, and their progress recorded here.
    supervision.supervise(
        capyctl_controller::model_sources::SourceMaterializer::new(
            owner.clone(),
            capyctl_controller::model_sources::RemoteSources::new(
                owner.clone(),
                sessions.clone(),
                authority.controller_id(),
            ),
        )
        .spawn_until(supervision.cancel_signal()),
    );
    let configuration = Arc::new(
        capyctl_management::configuration::SharedConfigurationSource::from_registry(
            owner.clone(),
            "owner",
        )
        .map_err(|_| unavailable())?,
    );
    // SPEC §10, ADR 0013 §8 (W10): one switcher for request-driven switching
    // and the operator's `start --evict`, bounded by `switching.drain_timeout`.
    let switcher = Arc::new(capyctl_controller::switching::Switcher::new(
        coordinator.commands(),
        capyctl_controller::switching::SwitchOptions {
            drain_timeout: config.switch_drain_timeout,
            ..Default::default()
        },
    ));
    let actions = Arc::new(
        capyctl_management::actions::OwnedActionSource::new(configuration, coordinator.commands())
            .map_err(|_| unavailable())?
            .with_switcher(switcher.clone()),
    );
    // ADR 0018 §4: retiring a host's runtime profile through the ordinary stop path.
    sessions.with_profile_retirements(Arc::new(
        capyctl_management::engines::StoreRetirements::new(actions.clone()),
    ));
    // ADR 0013 §10 (I3, D9): the router balances across instances on host
    // liveness and the engine load each host agent reports. ADR 0028 §11
    // (decided 2026-10-06): it watches each request to a group's head for its
    // first token within the stall timeout.
    let controller = Arc::new(
        capyctl_controller::CoordinatorLifecycle::new(coordinator.commands())
            .with_routing(capyctl_controller::RoutingSignals::from_sessions(&sessions))
            .with_switcher(switcher.clone())
            .with_group_stall_timeout(config.groups.stall_timeout),
    );
    let inflight = Arc::new(capyctl_router::admission::InFlight::default());
    // SPEC §17 (M80): `observability.timing_header`, off unless set.
    inflight.latency.set_timing_header(config.timing_header);
    // SPEC §17 (M80): the timing header names the engine family the host reports.
    inflight.latency.set_host_latency(sessions.latency_table());
    // SPEC §17 (M80): router, host ingress and engine latency, read together.
    let latency_view = {
        let (recorder, hosts) = (inflight.latency.clone(), sessions.latency_table());
        capyctl_management::metrics::latency_router(
            management_credentials(&credentials)?,
            Arc::new(move |deployment: Option<&str>| {
                capyctl_router::timing::latency_report(
                    &recorder,
                    &hosts.snapshot(deployment),
                    deployment,
                )
            }),
        )
    };
    // SPEC §§10, 17 (owner decision 2026-10-08): live load, read by an
    // operator or an external router.
    let load_view = load_view(
        management_credentials(&credentials)?,
        owner.clone(),
        inflight.clone(),
        sessions.load_table(),
    );
    // SPEC §10 step 1, §16.2 (W10 gap b): waiting requests are bounded by the
    // hosts' published `resource_policy.queue` (the tightest of them), read
    // again as hosts enroll and publish.
    let _queue_limits = {
        let (controller, inflight) = (controller.clone(), inflight.clone());
        AbortOnDrop(tokio::spawn(async move {
            loop {
                if let Ok(Some(queue)) = controller.queue_policy() {
                    inflight.waiting.set_limits(wait_limits(&queue));
                }
                tokio::time::sleep(QUEUE_POLICY_REFRESH).await;
            }
        }))
    };
    let inference = capyctl_router::serve_router(capyctl_router::RouterDeps {
        controller: controller.clone(),
        forwards: Arc::new(capyctl_router::forwarders::LiveForwarders::new(controller)),
        limits: capyctl_router::QueueLimits {
            max_requests_per_deployment: capyctl_domain::launch::MAX_REQUESTS_PER_DEPLOYMENT
                as usize,
            max_buffered_bytes_total: 64 * 1024 * 1024,
        },
        // Design §9: the key check is off only when the operator chose
        // `none` for this run (document, flag or variable).
        api_key: match config.inference_auth {
            crate::exposure::InferenceAuth::ApiKey => Some(credentials.api_key.clone()),
            crate::exposure::InferenceAuth::None => None,
        },
        inflight,
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    });
    // SPEC §4.3: the explicit drain of an enrolled host. Owner decision 4: an
    // offline host's drain is accepted at once with its Stops pending.
    let presence = {
        let sessions = sessions.clone();
        Arc::new(move |host: &str| sessions.current_session(host).is_some())
    };
    let drain = capyctl_management::drain::drain_router_with_presence(
        management_credentials(&credentials)?,
        actions.clone(),
        Vec::new(),
        presence,
    );
    let management =
        capyctl_management::lifecycle_router(management_credentials(&credentials)?, actions)
            .merge(drain)
            .merge(
                capyctl_management::enrollment::enrollment_router(
                    management_credentials(&credentials)?,
                    authority.clone(),
                    config.bootstrap_address.clone(),
                    config.control_address.clone(),
                )
                .map_err(|_| unavailable())?
                .reset_fallback(),
            )
            // SPEC §13.3 / T21: an instance's engine log, read on its host.
            .merge(capyctl_management::engine_log::engine_log_router(
                management_credentials(&credentials)?,
                capyctl_management::engine_log::RemoteEngineLogs::new(
                    owner.clone(),
                    sessions.clone(),
                    authority.controller_id(),
                ),
            ))
            .merge(capyctl_management::hosts::hosts_router(
                management_credentials(&credentials)?,
                owner,
                sessions.clone(),
            ))
            .merge(latency_view)
            .merge(load_view)
            // Design §9: the inference listener's bind and authentication.
            .merge(
                capyctl_management::inference_listener::inference_listener_router(
                    management_credentials(&credentials)?,
                    {
                        let view = Arc::new(
                            capyctl_management::inference_listener::InferenceListenerView::default(
                            ),
                        );
                        view.set(crate::exposure::listener_view(
                            config.inference,
                            config.inference_auth,
                        ));
                        view
                    },
                ),
            );
    let management_listener = tokio::net::TcpListener::bind(config.management)
        .await
        .map_err(|e| listen_failed("management", config.management, &e))?;
    crate::roles::record_management_address(&config.state_dir, config.management);
    // Design §9: said out loud before the listener accepts connections.
    crate::exposure::warn_if_exposed(config.inference, config.inference_auth);
    let inference_listener = tokio::net::TcpListener::bind(config.inference)
        .await
        .map_err(|e| listen_failed("inference", config.inference, &e))?;
    let bootstrap_listener = tokio::net::TcpListener::bind(config.bootstrap)
        .await
        .map_err(|e| listen_failed("bootstrap", config.bootstrap, &e))?;
    let control_listener = tokio::net::TcpListener::bind(config.control)
        .await
        .map_err(|e| listen_failed("control", config.control, &e))?;
    let identity = Identity::from_pem(certificate.pem, key.private_key_pem());
    // SPEC §4.3 (owner decision P3): a signal is a service restart. Inference
    // admission closes first, admitted streams finish within the bound, then
    // every listener stops and the coordinator is joined; remote engines keep
    // running on their hosts and the next start adopts and re-proves them.
    let bound = config.drain_timeout;
    let mut signals = crate::shutdown::Signals::install().map_err(|_| unavailable())?;
    let admission = crate::shutdown::Admission::new();
    let inference = admission.gate(inference);
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let stopping = |mut stopped: tokio::sync::watch::Receiver<bool>| async move {
        let _ = stopped.wait_for(|stopped| *stopped).await;
    };
    let bootstrap = Server::builder()
        .tls_config(ServerTlsConfig::new().identity(identity.clone()))
        .map_err(|_| unavailable())?
        .add_service(
            BootstrapServer::from_arc(authority.clone())
                .max_decoding_message_size(32768)
                .max_encoding_message_size(131072),
        )
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(bootstrap_listener),
            stopping(stopped.clone()),
        );
    let control = Server::builder()
        .tls_config(
            ServerTlsConfig::new()
                .identity(identity)
                .client_ca_root(Certificate::from_pem(ca_pem)),
        )
        .map_err(|_| unavailable())?
        .add_service(
            HostIdentityServer::from_arc(authority)
                .max_decoding_message_size(32768)
                .max_encoding_message_size(131072),
        )
        .add_service(
            AgentControlServer::from_arc(sessions.clone())
                // SPEC §13.3: room for an engine log tail answer.
                .max_decoding_message_size(capyctl_protocol::HOST_MESSAGE_BYTES)
                .max_encoding_message_size(capyctl_protocol::SERVER_MESSAGE_BYTES),
        )
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(control_listener),
            stopping(stopped.clone()),
        );
    // Design §9 ("Where the key is"): the owner-only file holding the API key
    // and admin token, never the key itself.
    print!(
        "{}",
        crate::role_text::banner(&json!({"role":"server","ready":true,
            "version":env!("CARGO_PKG_VERSION"),
            "management":config.management.to_string(),
            "inference":config.inference.to_string(),
            "inference_auth": if matches!(config.inference_auth, crate::exposure::InferenceAuth::None) { "none" } else { "api_key" },
            "bootstrap":config.bootstrap.to_string(),
            "control":config.control.to_string(),
            "state_dir":config.state_dir,
            "credentials":config.identity_dir.join("server-credentials.json")}))
    );
    // SPEC §3: remote and embedded modes share the ordinary lifecycle/router.
    let management_stopped = stopped.clone();
    let mut listeners = tokio::spawn(async move {
        tokio::try_join!(
            async { bootstrap.await.map_err(|_| unavailable()) },
            async { control.await.map_err(|_| unavailable()) },
            async {
                crate::shutdown::serve(management_listener, management, management_stopped)
                    .await
                    .map_err(|_| unavailable())
            },
            async {
                crate::shutdown::serve(inference_listener, inference, stopped)
                    .await
                    .map_err(|_| unavailable())
            }
        )
        .map(|_| ())
    });
    let failed = tokio::select! {
        result = &mut listeners => Some(result),
        _ = signals.recv() => None,
    };
    if let Some(result) = failed {
        listeners.abort();
        supervision
            .join(crate::shutdown::SUPERVISION_JOIN_BOUND)
            .await;
        sessions
            .shutdown(crate::shutdown::SUPERVISION_JOIN_BOUND)
            .await;
        switcher
            .shutdown(crate::shutdown::SUPERVISION_JOIN_BOUND)
            .await;
        let _ = coordinator.shutdown().await;
        result.map_err(|_| unavailable())??;
        return Err(unavailable());
    }
    let started = std::time::Instant::now();
    let drain = admission.drain_unless(bound, signals.forced()).await;
    stop.send_replace(true);
    let _ = crate::shutdown::join_listeners(&mut listeners).await;
    listeners.abort();
    // ADR 0015 invariant 6: nothing the role started outlives it. The
    // supervisors finish their pass and are joined, then every host session
    // ends through its teardown (claims retained, SPEC §13), then the
    // switcher's follow-ups, then the coordinator's worker.
    supervision
        .join(crate::shutdown::SUPERVISION_JOIN_BOUND)
        .await;
    sessions
        .shutdown(crate::shutdown::SUPERVISION_JOIN_BOUND)
        .await;
    switcher
        .shutdown(crate::shutdown::SUPERVISION_JOIN_BOUND)
        .await;
    coordinator.shutdown().await.map_err(|_| unavailable())?;
    drop(storage);
    Ok(
        json!({"role":"server","stopped":true,"engines":"retained","drain":drain.to_json(),
        "drain_bound_secs":bound.as_secs(),"shutdown_ms":crate::shutdown::elapsed_ms(started)}),
    )
}
/// How long a draining host waits for the controller to acknowledge that its
/// dispatch is suspended before it closes ingress anyway.
const DRAIN_NOTICE_BOUND: Duration = Duration::from_secs(5);

/// Discrete GPU design §2: the start-time check of a host's declared device
/// domains against the GPUs it observes (`device_policy_mismatch`, exit 2).
fn check_host_device_policy(
    document: &Value,
    shape: &capyctl_agent::gpu_memory::HostShape,
) -> Result<(), StructuredError> {
    let local = capyctl_config::remote_resources::local_host_document(document).map_err(|e| {
        error(&format!(
            "Invalid host configuration: {}: {}",
            e.path, e.detail
        ))
    })?;
    let policy = capyctl_config::effective::normalize_host_policy(&local).map_err(|e| {
        error(&format!(
            "Invalid host configuration: {}: {}",
            e.path, e.detail
        ))
    })?;
    capyctl_agent::device_domains::check_device_policy(&policy, shape)
        .map_err(|message| error(&message))
}

/// ADR 0018 §2: the host document at `path` merged with `engines`, with its
/// models directory and model-source policy resolved (owner decision
/// 2026-09-25: `flags` > `CAPYCTL_MODELS_ROOT` / `CAPYCTL_MODEL_SOURCES` /
/// `CAPYCTL_MODEL_SOURCES_MAX` > the document > `~/models`, sources allowed with
/// a 500 GiB ceiling in `<model_store>/sources`).
///
/// Owner rule 2026-09-25: the engine settings (`--vllm-bin`, `CAPYCTL_VLLM_BIN`,
/// `local_engine`, `--runtime-dir`, `--engine-ports` and the rest) are
/// resolved by the same rule and applied to the document before publication.
fn load_host(
    path: &Path,
    engines: &Path,
    flags: &capyctl_config::model_settings::ModelOverrides,
    engine_flags: &capyctl_config::engine_settings::EngineOverrides,
    overrides: &capyctl_config::setting_overrides::SettingOverrides,
) -> Result<HostConfig, StructuredError> {
    let invalid = |e: capyctl_config::ConfigError| {
        let e = overrides.annotate(e);
        error(&format!(
            "Invalid host configuration: {}: {}",
            e.path, e.detail
        ))
    };
    let env =
        capyctl_config::model_settings::ModelOverrides::from_process_env().map_err(invalid)?;
    let engine_env =
        capyctl_config::engine_settings::EngineOverrides::from_process_env().map_err(invalid)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    HostConfig::load_with_overrides(path, engines, overrides)
        .and_then(|config| config.with_models(flags, &env, home.as_deref()))
        .and_then(|config| {
            config.with_engines(engine_flags, &engine_env, &crate::roles::engine_version)
        })
        .map_err(invalid)
}

/// `document` is the host.yaml `config` was loaded from, merged with
/// `engines` (the role's engines.yaml); the control handler re-reads both
/// (ADR 0018 §3).
async fn serve_host(
    mut config: HostConfig,
    document: PathBuf,
    engines: PathBuf,
) -> Result<Value, StructuredError> {
    // SPEC §3.3 / ADR 0001: an undeclared runtime_dir is the managed copy of
    // the embedded runtime, refreshed before anything can launch from it.
    if !config.runtime_dir_declared {
        crate::managed_runtime::prepare_for_role(&config.runtime_dir)?;
    }
    // SPEC §3 / T22: fresh physical placement evidence is required on remote
    // hosts too, including unified-memory hosts. A failed collector publishes
    // nothing; native entrypoint denials remain closed.
    let runtime_dir = config.runtime_dir.clone();
    let (boot_gpu, device_inventory) = tokio::task::spawn_blocking(move || {
        let sample = capyctl_agent::gpu_memory::sample();
        let inventory = crate::device_inventory::collect(&runtime_dir, sample.as_ref());
        (sample, inventory)
    })
    .await
    .map_err(|_| unavailable())?;
    crate::device_inventory::publish_host(&mut config.document, device_inventory.as_ref());
    // ADR 0019 (discrete GPU design §2): a host that declares a device domain
    // checks the declaration against the GPUs it observes (a bounded sample,
    // off the async thread) before anything else, and refuses to start on a
    // mismatch. Unified hosts still publish the placement inventory above.
    let discrete = !capyctl_agent::device_domains::device_domains(&config.document).is_empty();
    if discrete {
        let shape = capyctl_agent::gpu_memory::shape(boot_gpu.as_ref())
            .map_err(|e| error(&e.to_string()))?;
        check_host_device_policy(&config.document, &shape)?;
    }
    let storage = IdentityDirectory::open(&config.identity_dir).map_err(|e| {
        identity_refused(
            "Host identity",
            &config.identity_dir,
            &e,
            "; run join host before startup",
        )
    })?;
    let identity = PendingEnrollment::load(&storage).map_err(|_| {
        error(
            "Host is not enrolled or its identity needs recovery; run join host with an invitation",
        )
    })?;
    let host = identity
        .host_id()
        .ok_or_else(|| {
            error("Enrollment is incomplete; repeat join host with the same invitation")
        })?
        .to_owned();
    identity.control_endpoint(now()).map_err(|_| {
        error("Host certificate is invalid or expired; explicit identity recovery is required")
    })?;
    private_dir(&config.state_dir.join("journal"))?;
    let journal = HostJournal::open(
        &config.state_dir.join("journal"),
        &identity.controller_id(),
        &host,
    )
    .map_err(|failure| match failure {
        // SPEC §13.2 / T33: say what happened and how to recover.
        newer @ capyctl_agent::journal::JournalError::FromNewerVersion { .. } => {
            from_newer_version(newer.to_string())
        }
        _ => error("Host journal is unsafe or owned by another process"),
    })?;
    let reading = capyctl_agent::memory::read_host_memory()
        .map_err(|_| error("Host memory inventory unavailable"))?;
    // SPEC §7.2 / ADR 0019: the device domains are published from a sample
    // taken beside the memory reading, so both are fresh at publication.
    let gpu = if discrete {
        tokio::task::spawn_blocking(capyctl_agent::gpu_memory::sample)
            .await
            .map_err(|_| unavailable())?
    } else {
        None
    };
    let domains = capyctl_agent::device_domains::startup_domains(
        &config.document,
        &reading.memory,
        &reading.source,
        gpu.as_ref(),
    );
    // ADR 0008 (owner decision 2026-09-23): registration measures each
    // installation (engine package version and a digest over its files);
    // later drift is flagged against this. Unmeasurable is never a refusal.
    let profiles = tokio::task::spawn_blocking({
        let config = config.clone();
        move || capyctl_agent::profiles::profile_statuses(&config)
    })
    .await
    .map_err(|_| unavailable())?;
    let inventory = pb::ReportInventory {
        group: None,
        domains,
        profiles,
        envelope: Some(pb::Envelope {
            host_id: host.clone(),
            protocol_version: capyctl_protocol::PROTOCOL_VERSION.into(),
            ..Default::default()
        }),
        approved_host_config_json: config.document.to_string(),
        host_boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_err(|_| error("Host boot identity unavailable"))?
            .trim()
            .to_owned(),
        policy_fingerprint: capyctl_config::remote_resources::policy_fingerprint(&config.document),
        // SPEC §§3.1, 7.3: the native executor below advertises per-launch
        // claims in the inventory it publishes; this startup snapshot does not.
        launch_claims: String::new(),
        // ADR 0028 §12: saver maps are sampled by the executor's refreshes.
        member_savers: Vec::new(),
    };
    let ingress = capyctl_agent::ingress::Ingress::new().map_err(|_| unavailable())?;
    let (execution, ingress_listener) = if let Some(settings) = &config.ingress {
        private_dir(&config.state_dir.join("ingress-identity"))?;
        private_dir(&config.state_dir.join("logs"))?;
        // SPEC §9.2: memory-saver SGLang launches enroll their saver
        // observation here (0700, this service user).
        private_dir(&config.state_dir.join("observation"))?;
        // SPEC §8.2 / T21: per-launch SGLang file rendezvous directories,
        // removed on gone evidence (0700, this service user).
        private_dir(&config.state_dir.join("rendezvous"))?;
        private_dir(&config.state_dir.join("engines"))?;
        let private =
            IdentityDirectory::open(&config.state_dir.join("ingress-identity")).map_err(|e| {
                identity_refused(
                    "Ingress identity",
                    &config.state_dir.join("ingress-identity"),
                    &e,
                    "",
                )
            })?;
        let identities = capyctl_agent::ingress_identity::IngressIdentities::new(private);
        let execution = capyctl_agent::native_execution::NativeHostExecution::new(
            journal.clone(),
            ingress.clone(),
            identities,
            config.clone(),
            host.clone(),
            identity.controller_id(),
            config.runtime_dir.clone(),
            config.state_dir.join("logs"),
            inventory.clone(),
        )
        // ADR 0014 §7, Q9: the per-host checkpoint stat cache is private state.
        .with_checkpoint_cache(config.state_dir.join("checkpoints"))
        // SPEC §8.2 / T21 (found live 2026-09-23): a signalled SGLang stop
        // left its rendezvous directory in /tmp; the host now owns and removes it.
        .with_rendezvous_root(config.state_dir.join("rendezvous"))
        .with_engine_cache_root(config.state_dir.join("engines"))
        // SPEC §9.2 (W4): the production saver observation source; without it
        // SGLang Park is refused unchanged.
        .with_saver_residency(Arc::new(
            capyctl_agent::native_execution::EnrolledSaver::new(
                config.state_dir.join("observation"),
            ),
        ))
        // ADR 0007 (found live 2026-09-23, matrix M33): each GPU process's
        // memory rides the availability reports, so the server credits
        // resident engines instead of charging them twice.
        .with_process_residency(capyctl_agent::process_residency::ResidencySampler::nvidia());
        let listener = tokio::net::TcpListener::bind(settings.bind)
            .await
            .map_err(|e| listen_failed("ingress", settings.bind, &e))?;
        (Some(execution), Some(listener))
    } else {
        (None, None)
    };
    // SPEC §4.3 (owner decision P3): a signal is a service restart. Ingress
    // admission and every ingress gate close, admitted streams finish within the
    // bound, and the control session closes (its fence journals the disconnect).
    // No engine is signalled: each stays running and owned, and the next session
    // re-proves it with a fresh probe before its gate reopens.
    let bound = config.drain_timeout;
    let mut signals = crate::shutdown::Signals::install().map_err(|_| unavailable())?;
    let admission = crate::shutdown::Admission::new();
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let drain_signal = capyctl_agent::session::DrainSignal::new();
    // ADR 0018 §3: the profile sets the native executor authorizes against,
    // shared with the session and the local control handler.
    let host_profiles = match &execution {
        Some(native) => native.profiles(),
        None => capyctl_agent::profiles::HostProfiles::new(
            capyctl_agent::profiles::ProfileSet::new(config.clone(), inventory.clone()),
        ),
    };
    let updates = capyctl_agent::session::ProfileUpdates::new(host_profiles);
    // ADR 0018 §3: the local control channel, bound only inside the role's
    // 0700 state directory. A socket that cannot be bound (unsafe directory,
    // path too long, another role) is reported and the role runs without it.
    let (control_stop, control_shutdown) = tokio::sync::watch::channel(false);
    let control_socket = config
        .state_dir
        .join(capyctl_agent::control_socket::SOCKET_NAME);
    let control_server = match capyctl_agent::control_socket::ControlServer::bind(&control_socket) {
        Ok(server) => {
            let handler = capyctl_agent::host_control::HostControl::new(
                document.clone(),
                engines.clone(),
                config.clone(),
                updates.clone(),
                journal.clone(),
            );
            // SAFETY: geteuid has no preconditions and cannot fail.
            let uid = unsafe { libc::geteuid() };
            Some(tokio::spawn(server.serve(handler, uid, control_shutdown)))
        }
        Err(failure) => {
            capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Warning,
                &format!("host control socket unavailable: {failure}"),
            );
            None
        }
    };
    let execution =
        execution.map(|native| native as Arc<dyn capyctl_agent::session::SessionExecution>);
    let session = capyctl_agent::session::run_session_with_updates(
        &identity,
        journal,
        inventory,
        receiver,
        execution,
        Some(drain_signal.clone()),
        Some(updates.clone()),
    );
    let gates = ingress.clone();
    let router = admission.gate(ingress.router());
    // Design §9 ("Where the key is"): ready, said as `start standalone` says
    // it, naming the owner-only identity file and never its contents.
    print!(
        "{}",
        crate::role_text::banner(&host_banner(
            &host,
            &config.state_dir,
            config
                .ingress
                .as_ref()
                .map(|settings| settings.bind.to_string()),
            &config
                .identity_dir
                .join(capyctl_agent::enrollment::HOST_FILE),
            &config.document,
        ))
    );
    let mut ingress_server = tokio::spawn(async move {
        match ingress_listener {
            Some(listener) => crate::shutdown::serve(listener, router, stopped)
                .await
                .map_err(|_| unavailable()),
            None => std::future::pending::<Result<(), StructuredError>>().await,
        }
    });
    tokio::pin!(session);
    tokio::select! {
        result = &mut ingress_server => {
            result.map_err(|_| unavailable())??;
            return Err(unavailable());
        }
        result = &mut session => {
            // SPEC §4.1, ADR 0016: a revoked host exits without draining or
            // signalling anything; engines stay running for `join --recover`.
            result.map_err(|_| host_revoked(&host))?;
            return Err(unavailable());
        }
        _ = signals.recv() => {}
    }
    let started = std::time::Instant::now();
    // SPEC §4.3 (Phase B follow-up): the controller suspends dispatch to this
    // host first, so the router answers a retryable 503 instead of forwarding
    // into an ingress that is closing. Bounded: an unreachable controller does
    // not hold the drain.
    // The session is polled only here, while the notice goes out and its
    // acknowledgement comes back; as before, it runs no further effects while
    // the ingress drains.
    let mut session_ended = None;
    let announced = {
        let announce = drain_signal.announce(DRAIN_NOTICE_BOUND);
        tokio::pin!(announce);
        tokio::select! {
            announced = &mut announce => announced,
            ended = &mut session => {
                session_ended = Some(ended);
                capyctl_agent::session::DrainAnnouncement::NotConnected
            }
        }
    };
    admission.close();
    // SPEC §13: closing forwarding claims no native quiescence and releases
    // nothing; it only stops new work reaching the engines.
    let _ = gates.close_all();
    let drain = admission.drain_unless(bound, signals.forced()).await;
    stop.send_replace(true);
    let _ = crate::shutdown::join_listeners(&mut ingress_server).await;
    ingress_server.abort();
    let _ = shutdown.send(true);
    // ADR 0018 §3: the control socket closes with the role and removes its file.
    let _ = control_stop.send(true);
    if let Some(server) = control_server {
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    }
    match session_ended {
        Some(ended) => ended,
        None => session.await,
    }
    .map_err(|_| host_revoked(&host))?;
    Ok(
        json!({"role":"host","host_id":host,"stopped":true,"engines":"retained","drain":drain.to_json(),
        "dispatch_suspension":announced.as_str(),
        "drain_bound_secs":bound.as_secs(),"shutdown_ms":crate::shutdown::elapsed_ms(started)}),
    )
}
/// ADR 0016: how long a recovery invitation stays redeemable.
const RECOVERY_INVITATION_SECONDS: i64 = 900;
pub fn supports(command: &Command) -> bool {
    matches!(
        command,
        Command::Start(Role::Server | Role::Host)
            | Command::Init(_)
            | Command::Invite { .. }
            | Command::Join { .. }
            | Command::List {
                resource: ListResource::Hosts
            }
            | Command::List {
                resource: ListResource::Engines
            }
            | Command::Inspect {
                resource: Resource::Host,
                ..
            }
    )
}
pub async fn execute(invocation: &Invocation, root: &Path) -> Result<Value, StructuredError> {
    match &invocation.command {
        Command::Init(role) => {
            let output = invocation
                .output
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    implicit(
                        root,
                        if *role == InitTarget::Server {
                            "server"
                        } else {
                            "host"
                        },
                    )
                });
            initialize(root, *role, &output)
        }
        Command::Start(role @ (Role::Server | Role::Host)) => {
            let label = if *role == Role::Server {
                "server"
            } else {
                "host"
            };
            // ADR 0018 §2 (review decision 2026-09-25): a role's document is
            // `--config`, else `$CAPYCTL_CONFIG`, as for `capyctl engine`; owner
            // rule 2026-09-25: the server's too, so the setting has its two
            // run-time forms for every role.
            // Owner decision 2026-09-25: this run's generic overrides
            // (`--set` > `CAPYCTL_SET__…` > the document); a named flag or
            // variable of the same setting must agree with them. Checked
            // before anything is created.
            let kind = if *role == Role::Server {
                capyctl_config::ConfigKind::Server
            } else {
                capyctl_config::ConfigKind::Host
            };
            let overrides = crate::settings::role_overrides(
                kind,
                &invocation.sets,
                &crate::settings::flag_layer(invocation),
            )
            .map_err(|e| {
                error(&format!(
                    "Invalid {label} configuration: {}",
                    crate::settings::describe(&e)
                ))
            })?;
            let named = crate::engine::named_role_document(invocation.config.as_deref(), &role_env);
            let path = named.clone().unwrap_or_else(|| implicit(root, label));
            if named.is_none()
                && fs::symlink_metadata(&path)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                initialize(
                    root,
                    if *role == Role::Server {
                        InitTarget::Server
                    } else {
                        InitTarget::Host
                    },
                    &path,
                )?;
                if *role == Role::Host {
                    return Err(error("Created an offline host template; run join host with an invitation before starting"));
                }
            }
            let source = read_config(&path)?;
            if *role == Role::Server {
                // Design §9: a bad `--listen` or CAPYCTL_INFERENCE_ADDR refuses
                // before the document is touched.
                crate::roles::inference_override(invocation.listen)?;
                crate::exposure::effective_inference_auth(
                    crate::exposure::InferenceAuth::ApiKey,
                    invocation.no_inference_auth,
                )?;
                if let Some(warning) =
                    crate::roles::deprecated_inference_env_warning(invocation.listen)
                {
                    crate::roles::role_warning(&warning);
                }
                let invalid = |e: capyctl_config::ConfigError| {
                    error(&format!(
                        "Invalid server configuration: {}",
                        crate::settings::describe(&overrides.annotate(e))
                    ))
                };
                let parse = |source: &str| {
                    let document = capyctl_config::parse_document(source)
                        .and_then(|document| overrides.apply_and_validate(document))
                        .map_err(invalid)?;
                    ServerConfig::parse(&document.to_string()).map_err(invalid)
                };
                let mut config = parse(&source)?;
                // ADR 0028 §11 (decided 2026-10-06): `--group-stall-timeout` >
                // CAPYCTL_GROUP_STALL_TIMEOUT > `groups.stall_timeout` > 120 s;
                // a zero or malformed value refuses the start before anything
                // is created.
                config = config
                    .with_group_stall_timeout(invocation.group_stall_timeout, &|name| {
                        std::env::var(name).ok()
                    })
                    .map_err(invalid)?;
                // Final review I8-bis: `--state-dir` > CAPYCTL_STATE_DIR > the
                // document, before anything reads the state directory.
                let state_dir = state_dir_override(invocation, &config.state_dir);
                if let Some(dir) = &state_dir {
                    config = config.with_state_dir(dir.clone());
                }
                let document_bind = config.inference;
                // Design §9: `--listen` > CAPYCTL_INFERENCE_ADDR > the document,
                // by the rule the standalone role uses, and never onto another
                // server listener.
                let address =
                    crate::roles::effective_inference_address(document_bind, invocation.listen)?;
                let mut config = config.with_inference(address).map_err(|_| {
                    error(&format!(
                        "inference address {address} collides with another server listener \
                         or is not a unicast address with a non-zero port"
                    ))
                })?;
                // Design §9: `--no-inference-auth` > CAPYCTL_INFERENCE_AUTH >
                // the document, by the rule the standalone role uses.
                config.inference_auth = crate::exposure::effective_inference_auth(
                    config.inference_auth,
                    invocation.no_inference_auth,
                )?;
                // Final review I8: `--management-listen` >
                // CAPYCTL_MANAGEMENT_ADDR > the document, as for standalone.
                if let Some(address) =
                    crate::roles::management_override(invocation.management_listen)
                        .map_err(|e| error(&e.to_string()))?
                {
                    config = config.with_management(address).map_err(|_| {
                        error(&format!(
                            "management address {address} collides with another server listener \
                             or is not a loopback address with a non-zero port"
                        ))
                    })?;
                }
                // Owner decision 2026-09-26: client commands run later on
                // this machine without --config find this server.
                crate::local_role::record_document(root, "server", named.as_deref());
                serve_server(config).await
            } else {
                // ADR 0018 §2: the host document merged with its `engines.yaml`,
                // resolved by the same rule as `capyctl engine`; the document
                // itself was already read above for the size and existence
                // checks.
                let engines = host_engines(named.as_deref(), &path);
                // Owner rule 2026-09-25: a deprecated variable name is
                // warned about once.
                for warning in capyctl_config::engine_settings::deprecation_warnings(&|key| {
                    std::env::var(key).ok()
                }) {
                    crate::roles::role_warning(&warning);
                }
                // ADR 0018 amendment A3: a profile for an engine kind this
                // release does not know is skipped (the merge leaves it out),
                // and named here. An unreadable file is refused by the load.
                if let Ok(file) = capyctl_config::registration::EnginesFile::load(&engines) {
                    for unknown in file.runnable().1 {
                        crate::roles::role_warning(&unknown.warning(&engines));
                    }
                }
                let mut host = load_host(
                    &path,
                    &engines,
                    &invocation.model_overrides,
                    &invocation.engine_overrides,
                    &overrides,
                )?;
                // Final review I8-bis: `--state-dir` > CAPYCTL_STATE_DIR > the
                // document.
                if let Some(dir) = state_dir_override(invocation, &host.state_dir) {
                    host = host.with_state_dir(dir);
                }
                // Owner decision 2026-09-26: a command run later on this
                // machine without --config (`capyctl engine`, `config show`)
                // finds this host's document.
                crate::local_role::record_document(root, "host", named.as_deref());
                serve_host(host, path.clone(), engines).await
            }
        }
        Command::Join { join_file, recover } => {
            let named = crate::engine::named_role_document(invocation.config.as_deref(), &role_env);
            let path = named.clone().unwrap_or_else(|| implicit(root, "host"));
            // ADR 0018 §2: `read_config` keeps the existing size/existence
            // checks; the host document is loaded merged with `engines.yaml`.
            read_config(&path)?;
            let engines = host_engines(named.as_deref(), &path);
            // Owner decision 2026-09-25: the generic overrides apply here as
            // at `start host` (final review I8: `join host --set` too), so
            // both find the same identity.
            let overrides = capyctl_config::setting_overrides::SettingOverrides::from_process(
                capyctl_config::ConfigKind::Host,
                &invocation.sets,
            )
            .map_err(|e| {
                error(&format!(
                    "Invalid host configuration: {}",
                    crate::settings::describe(&e)
                ))
            })?;
            let mut config = load_host(
                &path,
                &engines,
                &Default::default(),
                &Default::default(),
                &overrides,
            )?;
            // Final review I8-bis: the identity `start host` will use.
            if let Some(dir) = state_dir_override(invocation, &config.state_dir) {
                config = config.with_state_dir(dir);
            }
            let invitation: JoinInvitation = serde_json::from_slice(&private_read(join_file)?)
                .map_err(|_| error("Invalid join invitation"))?;
            // ADR 0016: recovery is explicit on both sides. A recovery
            // invitation is never redeemed as an ordinary enrollment, and
            // `--recover` never enrolls a new host.
            match (*recover, invitation.recover_host_id.is_some()) {
                (false, true) => {
                    return Err(error(
                        "This is a recovery invitation; run join host --recover to re-enroll the revoked host",
                    ))
                }
                (true, false) => {
                    return Err(error(
                        "join host --recover needs a recovery invitation (invite host <name|id> --recover)",
                    ))
                }
                _ => {}
            }
            let storage = IdentityDirectory::open(&config.identity_dir).map_err(|e| {
                identity_refused(
                    "Host identity",
                    &config.identity_dir,
                    &e,
                    "; stop the host role first",
                )
            })?;
            let mut pending = if *recover {
                PendingEnrollment::prepare_recovery(&storage, &invitation).map_err(|_| {
                    error("Recovery invitation conflicts with the retained host identity")
                })?
            } else {
                PendingEnrollment::prepare(&storage, &invitation)
                    .map_err(|_| error("Invitation conflicts with the retained host identity"))?
            };
            let host_id = pending.enroll(&storage, &invitation, now()).await.map_err(|_| error("Enrollment failed; retain identity and retry the same invitation transaction"))?;
            crate::local_role::record_document(root, "host", named.as_deref());
            if *recover {
                // ADR 0016: the same host id, a new certificate; engines the
                // server recorded reopen only after a fresh probe.
                Ok(json!({"host_id":host_id,"enrolled":true,"recovered":true}))
            } else {
                Ok(json!({"host_id":host_id,"enrolled":true}))
            }
        }
        Command::Invite { name, recover } => {
            let output = invocation.output.as_ref().ok_or_else(|| {
                error("invite host requires --output FILE; invitations are never printed")
            })?;
            let path = Path::new(output);
            if fs::symlink_metadata(path).is_ok() {
                return Err(error(
                    "Invitation output already exists; nothing was overwritten",
                ));
            }
            let target = crate::local_role::resolve(root, invocation.config.as_deref())?;
            // ADR 0016: a recovery invitation is shorter-lived than an
            // ordinary one, and only a recovery request carries `recover`, so
            // an ordinary request keeps its exact earlier shape.
            let body = if *recover {
                json!({"host_name":name,"lifetime_seconds":RECOVERY_INVITATION_SECONDS,"recover":true})
            } else {
                json!({"host_name":name,"lifetime_seconds":3600})
            };
            let result = management_call(
                &target.endpoint,
                &target.token,
                reqwest::Method::POST,
                "/host-invitations",
                Some(body),
            )
            .await?;
            write_new(
                path,
                &serde_json::to_vec(&result).map_err(|_| unavailable())?,
            )?;
            if *recover {
                Ok(
                    json!({"invitation_file":path,"host_name":result["host_name"],
                    "recover_host_id":result["recover_host_id"],"expires_unix":result["expires_unix"]}),
                )
            } else {
                Ok(json!({"invitation_file":path,"host_name":name}))
            }
        }
        Command::List {
            resource: ListResource::Hosts,
        }
        | Command::Inspect {
            resource: Resource::Host,
            ..
        } => {
            let target = crate::local_role::resolve(root, invocation.config.as_deref())?;
            let result = management_call(
                &target.endpoint,
                &target.token,
                reqwest::Method::GET,
                "/hosts",
                None,
            )
            .await?;
            if let Command::Inspect { id: Some(id), .. } = &invocation.command {
                result["hosts"]
                    .as_array()
                    .and_then(|hosts| {
                        hosts
                            .iter()
                            .find(|h| h["host_id"] == *id || h["name"] == *id)
                    })
                    .cloned()
                    .ok_or_else(|| error("Host not found"))
            } else {
                Ok(result)
            }
        }
        Command::List {
            resource: ListResource::Engines,
        } => {
            // ADR 0018: every host's published profiles, from the server.
            let target = crate::local_role::resolve(root, invocation.config.as_deref())?;
            management_call(
                &target.endpoint,
                &target.token,
                reqwest::Method::GET,
                "/engines",
                None,
            )
            .await
        }
        _ => Err(error("Unsupported remote role command")),
    }
}
pub fn server_context(
    explicit: Option<&Path>,
    root: &Path,
) -> Result<ServerConfig, StructuredError> {
    ServerConfig::parse(&read_config(
        &explicit
            .map(PathBuf::from)
            .unwrap_or_else(|| implicit(root, "server")),
    )?)
    .map_err(|_| error("Invalid server context"))
}
pub async fn management_request(
    config: &ServerConfig,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, StructuredError> {
    let (endpoint, token) = management_context(config)?;
    management_call(&endpoint, &token, method, path, body).await
}

/// Owner decision 2026-09-26: the admin token in a server's identity
/// directory (`server-credentials.json`, owner-only).
pub(crate) fn server_admin_token(identity_dir: &Path) -> Result<String, StructuredError> {
    let c: Credentials = serde_json::from_slice(&private_read(
        &identity_dir.join("server-credentials.json"),
    )?)
    .map_err(|_| unavailable())?;
    Ok(c.admin_token)
}

/// One management request to `endpoint` with the admin `token`, whichever
/// role named them.
pub async fn management_call(
    endpoint: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, StructuredError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| unavailable())?;
    let mut request = client
        .request(method, format!("{endpoint}{path}"))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let mut response = request.send().await.map_err(|_| unavailable())?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
        if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
            return Err(unavailable());
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        // SPEC §14: keep the server's error class when it sent one.
        return Err(match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) if value["error"]["code"].is_string() => {
                crate::client::refusal(status, &value)
            }
            _ => error("Management command rejected"),
        });
    }
    serde_json::from_slice(&bytes).map_err(|_| unavailable())
}

/// The host role's ready banner; ADR 0018 A2: it names its engines, or the
/// command that adds one.
fn host_banner(
    host: &str,
    state_dir: &std::path::Path,
    ingress: Option<String>,
    credentials: &std::path::Path,
    document: &serde_json::Value,
) -> serde_json::Value {
    let mut profiles: Vec<&str> = document["runtime_profiles"]
        .as_object()
        .map(|profiles| profiles.keys().map(String::as_str).collect())
        .unwrap_or_default();
    profiles.sort_unstable();
    json!({"role": "host", "ready": true,
        "version": env!("CARGO_PKG_VERSION"),
        "host_id": host,
        "state_dir": state_dir,
        "ingress": ingress,
        "credentials": credentials,
        "profiles": profiles})
}

#[cfg(test)]
mod banner_tests {
    use super::*;

    // T02: a host with no engine names the command that adds one; one with
    // engines names them.
    #[test]
    fn a_host_banner_names_its_engines_or_engine_add() {
        let banner = |document: serde_json::Value| {
            crate::role_text::banner_text(&host_banner(
                "h1",
                std::path::Path::new("/s"),
                None,
                std::path::Path::new("/s/identity/host"),
                &document,
            ))
        };
        let empty = banner(json!({"runtime_profiles": {}}));
        assert!(
            empty.contains("Engines") && empty.contains("capyctl engine add"),
            "{empty}"
        );
        let named = banner(json!({"runtime_profiles": {"sglang": {}, "vllm": {}}}));
        assert!(named.contains("sglang, vllm"), "{named}");
        assert!(!named.contains("engine add"), "{named}");
    }
}

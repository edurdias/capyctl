//! U9 / ADR 0040 §§10–11: HTTP administration through the public server
//! adapter and a real mTLS host session. Fake engine installations provide
//! CPU integration evidence, not native-engine qualification.

use axum::{
    body::{to_bytes, Body},
    http::Request,
    Extension, Router,
};
use capyctl_agent::{
    control_socket::{ControlHandler, ControlRequest},
    engines::ScanRoots,
    enrollment::{JoinInvitation, PendingEnrollment},
    host_admin::{RoleKind, Target},
    host_admin_remote::{HostAdminHandler, HostAdminService, InProcessRole},
    host_control::HostControl,
    identity::{CertificateAuthority, HostKey},
    identity_storage::IdentityDirectory,
    profiles::{HostProfiles, ProfileSet},
    session::{
        run_session_with_updates, ExecutionFuture, LoadFuture, ProfileUpdates, SessionExecution,
    },
};
use capyctl_controller::{
    agent_sessions::AgentSessions,
    enrollment::EnrollmentAuthority,
    load_table::InstanceKey,
    ownership::SharedCoordinatorState,
    profile_retirement::{ProfileRetirements, RetirementStep},
    OwnedCoordinatorState,
};
use capyctl_management::{
    host_admin, host_admin_remote::RemoteHostAdmin, hosts::hosts_router, ui_sessions::Principal,
};
use capyctl_protocol::{
    capabilities,
    pb::{
        self, agent_control_client::AgentControlClient, agent_control_server::AgentControlServer,
        agent_to_server, bootstrap_server::Bootstrap, server_to_agent,
    },
    version::BINARY_VERSION,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tower::ServiceExt;

fn now() -> i64 {
    capyctl_protocol::now_unix_ms() / 1000
}

fn directory() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn prepared_document() -> Value {
    let fixture = include_str!("../../capyctl-config/tests/fixtures/effective-sglang-golden.json");
    let mut host = serde_json::from_str::<Value>(fixture).unwrap()["input"]["host"].clone();
    host["state_dir"] = json!("/home/operator/.local/state/capyctl");
    host["identity_dir"] = json!("/home/operator/.local/state/capyctl/identity");
    // Match the controller fixture: fit the approved policy to real runner memory.
    let bytes = |v: &Value| capyctl_config::effective::parse_bytes(v.as_str().unwrap()).unwrap();
    let capacity = capyctl_agent::memory::read_host_memory()
        .unwrap()
        .memory
        .capacity_bytes as f64;
    let domain = &mut host["resource_policy"]["domains"]["unified"];
    let needed = (bytes(&domain["managed_limit"]) + bytes(&domain["free_reserve"])) as f64;
    let scale = (capacity / 2.0 / needed).min(1.0);
    for field in [
        "managed_limit",
        "free_reserve",
        "parked_limit",
        "host_kv_limit",
    ] {
        let mib = (bytes(&domain[field]) as f64 * scale) as i64 >> 20;
        domain[field] = json!(format!("{mib}MiB"));
    }
    host
}

fn inventory(host: &str) -> pb::ReportInventory {
    inventory_document(host, &prepared_document())
}

fn inventory_document(host: &str, document: &Value) -> pb::ReportInventory {
    let config = capyctl_config::remote_roles::HostConfig::parse(&document.to_string()).unwrap();
    let at = capyctl_protocol::now_unix_ms();
    pb::ReportInventory {
        domains: vec![pb::DomainObservation {
            domain_id: "unified".into(),
            kind: "system".into(),
            observed_bytes: 100 << 30,
            observed_at_unix: at / 1000,
            capacity_bytes: 128 << 30,
            available_bytes: 100 << 30,
            observed_at_unix_ms: at,
            ..Default::default()
        }],
        profiles: config
            .profiles
            .iter()
            .map(|(name, profile)| pb::RuntimeProfileStatus {
                name: name.clone(),
                build_fingerprint: profile["build_fingerprint"]
                    .as_str()
                    .unwrap_or("unknown")
                    .into(),
                eligibility: "unknown".into(),
                ..Default::default()
            })
            .collect(),
        approved_host_config_json: config.document.to_string(),
        host_boot_id: "boot".into(),
        policy_fingerprint: capyctl_config::remote_resources::policy_fingerprint(&config.document),
        envelope: Some(pb::Envelope {
            host_id: host.into(),
            protocol_version: capyctl_protocol::PROTOCOL_VERSION.into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

struct Harness {
    state: SharedCoordinatorState,
    sessions: Arc<AgentSessions>,
    identity: PendingEnrollment,
    host: String,
    server: tokio::task::JoinHandle<()>,
    _state_dir: tempfile::TempDir,
    identity_dir: tempfile::TempDir,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Harness {
    async fn enrolled() -> Self {
        let state_dir = directory();
        let state = Arc::new(Mutex::new(
            OwnedCoordinatorState::open(state_dir.path()).unwrap(),
        ));
        let ca = CertificateAuthority::generate(now()).unwrap();
        let ca_pem = ca.certificate_pem().to_owned();
        let key = HostKey::generate().unwrap();
        let certificate = ca.issue_server("localhost", &key, now()).unwrap();
        let authority = Arc::new(EnrollmentAuthority::new(state.clone(), ca));
        let sessions = AgentSessions::new(authority.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(certificate.pem, key.private_key_pem()))
            .client_ca_root(Certificate::from_pem(&ca_pem));
        let service = AgentControlServer::from_arc(sessions.clone());
        let server = tokio::spawn(async move {
            Server::builder()
                .tls_config(tls)
                .unwrap()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let identity_dir = directory();
        let storage = IdentityDirectory::open(identity_dir.path()).unwrap();
        let invite = authority.invite("spark", 300, now()).unwrap();
        let invitation = JoinInvitation {
            version: 1,
            server_address: address.clone(),
            control_address: address,
            server_ca: ca_pem,
            invitation_id: invite.id,
            invitation_secret: invite.secret,
            host_name: invite.host_name,
            expires_unix: invite.expires_unix,
            recover_host_id: None,
        };
        let mut identity = PendingEnrollment::prepare(&storage, &invitation).unwrap();
        let certificate = Bootstrap::enroll(
            authority.as_ref(),
            tonic::Request::new(identity.request(&invitation).unwrap()),
        )
        .await
        .unwrap()
        .into_inner();
        let host = identity
            .accept_certificate(&storage, certificate, now())
            .unwrap();
        Self {
            state,
            sessions,
            identity,
            host,
            server,
            _state_dir: state_dir,
            identity_dir,
        }
    }

    fn router(&self) -> Router {
        host_admin::router(RemoteHostAdmin::new(
            self.state.clone(),
            self.sessions.clone(),
        ))
        .merge(hosts_router(self.state.clone(), self.sessions.clone()))
        .layer(Extension(Principal::Admin))
    }

    async fn scripted(
        &self,
        capabilities: Vec<String>,
    ) -> (
        tokio::sync::mpsc::Sender<pb::AgentToServer>,
        tonic::Streaming<pb::ServerToAgent>,
    ) {
        let channel = self
            .identity
            .control_endpoint(now())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let (send, receive) = tokio::sync::mpsc::channel(64);
        for msg in [
            agent_to_server::Msg::Connect(pb::Connect {
                host_id: self.host.clone(),
                protocol_version: capyctl_protocol::PROTOCOL_VERSION.into(),
                binary_version: BINARY_VERSION.into(),
                capabilities,
                ..Default::default()
            }),
            agent_to_server::Msg::ReportInventory(inventory(&self.host)),
            agent_to_server::Msg::ReconcileHistory(pb::ReconcileHistory {
                records: vec![],
                complete: true,
            }),
        ] {
            send.send(pb::AgentToServer { msg: Some(msg) })
                .await
                .unwrap();
        }
        let mut stream = AgentControlClient::new(channel)
            .session(ReceiverStream::new(receive))
            .await
            .unwrap()
            .into_inner();
        assert!(matches!(
            next(&mut stream, Duration::from_secs(10)).await,
            Some(server_to_agent::Msg::SessionReady(_))
        ));
        (send, stream)
    }

    async fn agent(&self, execution: Option<Arc<dyn SessionExecution>>) -> Agent {
        let dir = directory();
        std::fs::create_dir(dir.path().join("etc")).unwrap();
        let document = dir.path().join("etc/host.yaml");
        let mut host_document = prepared_document();
        host_document["state_dir"] = json!(dir.path().join("state"));
        host_document["identity_dir"] = json!(dir.path().join("state/identity"));
        host_document["model_store"]["path"] = json!(dir.path().join("models"));
        let config =
            capyctl_config::remote_roles::HostConfig::parse(&host_document.to_string()).unwrap();
        std::fs::write(&document, host_document.to_string()).unwrap();
        let inv = inventory_document(&self.host, &host_document);
        let updates = ProfileUpdates::new(HostProfiles::new(ProfileSet::new(
            config.clone(),
            inv.clone(),
        )));
        let journal = capyctl_agent::journal::HostJournal::open(
            dir.path(),
            &self.identity.controller_id(),
            &self.host,
        )
        .unwrap();
        let control = HostControl::new(
            document.clone(),
            capyctl_config::test_support::host_engines_path(&document),
            config,
            updates.clone(),
            journal.clone(),
        );
        updates.serve_host_admin(host_service(dir.path(), control.clone(), self.host.clone()));
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let identity =
            PendingEnrollment::load(&IdentityDirectory::open(self.identity_dir.path()).unwrap())
                .unwrap();
        let session = tokio::spawn(async move {
            run_session_with_updates(
                &identity,
                journal,
                inv,
                shutdown,
                execution,
                None,
                Some(updates),
            )
            .await
            .unwrap();
        });
        eventually(|| {
            self.sessions
                .inspect(&self.host)
                .is_some_and(|v| v.reconciled)
        })
        .await;
        Agent {
            control,
            document,
            stop,
            session,
            dir,
        }
    }
}

struct Agent {
    control: Arc<HostControl>,
    document: PathBuf,
    stop: tokio::sync::watch::Sender<bool>,
    session: tokio::task::JoinHandle<()>,
    dir: tempfile::TempDir,
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.session.abort();
    }
}

impl Agent {
    async fn stop(&mut self) {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), &mut self.session)
            .await
            .unwrap()
            .unwrap();
    }
}

async fn eventually(mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

async fn next(
    stream: &mut tonic::Streaming<pb::ServerToAgent>,
    wait: Duration,
) -> Option<server_to_agent::Msg> {
    let until = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(until, stream.message()).await {
            Err(_) => return None,
            Ok(message) => match message.unwrap()?.msg {
                Some(server_to_agent::Msg::Heartbeat(_)) => continue,
                other => return other,
            },
        }
    }
}

async fn http(router: &Router, method: &str, uri: &str, body: Option<Value>) -> (u16, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        router.clone().oneshot(request.body(body).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn host_uri(host: &str, suffix: &str) -> String {
    format!("/management/v1/hosts/{host}{suffix}")
}

fn script(path: &Path, body: &str) {
    capyctl_config::test_support::write_executable(path, format!("#!/bin/sh\n{body}\n"), 0o755)
        .unwrap();
}

fn capability_report() -> Value {
    json!({"schema": "capyctl/engine-capabilities/v1", "engine": "vllm",
        "capabilities": {"core": [], "deep_park": [], "metrics": []}})
}

fn vllm_env(root: &Path) -> PathBuf {
    let site = root.join("lib/python3.12/site-packages");
    std::fs::create_dir_all(site.join("vllm")).unwrap();
    std::fs::create_dir_all(site.join("vllm-0.29.0.dist-info")).unwrap();
    std::fs::write(
        site.join("vllm-0.29.0.dist-info/METADATA"),
        "Name: vllm\nVersion: 0.29.0\n",
    )
    .unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    script(&root.join("bin/vllm"), "echo 0.29.0");
    script(
        &root.join("bin/python3"),
        &format!("echo '{}'", capability_report()),
    );
    root.to_path_buf()
}

fn host_service(
    dir: &Path,
    control: Arc<HostControl>,
    host_id: String,
) -> Arc<dyn HostAdminHandler> {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let document = dir.join("etc/host.yaml");
    Arc::new(HostAdminService {
        host_id,
        target: Target {
            engines: capyctl_config::test_support::host_engines_path(&document),
            role_document: document,
            kind: RoleKind::Host,
            socket: state.join(capyctl_agent::control_socket::SOCKET_NAME),
            state_dir: state,
        },
        roots: ScanRoots {
            path_dirs: vec![],
            home: Some(home.clone()),
            xdg_data: Some(home.join(".local/share")),
            pipx_home: None,
            conda_roots: vec![],
            opt: None,
            system_bins: vec![],
            extra: vec![],
        },
        toolchain: capyctl_config::toolchain::ToolchainSearch {
            system: dir.join("no-bin").display().to_string(),
            default_cuda_home: dir.join("no-cuda"),
            system_root: dir.join("no-root"),
        },
        cuda_home_env: None,
        measurer: Arc::new(capyctl_agent::installation::InstallationMeasurer::new()),
        role: InProcessRole(control),
        model_env: Default::default(),
        pinner: Arc::new(capyctl_agent::host_admin::CachedSourcePinner::new(
            Default::default(),
            dir.to_path_buf(),
            Arc::new(|_| None),
        )),
    })
}

fn engine<'a>(list: &'a Value, host: &str, profile: &str) -> Option<&'a Value> {
    list["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["host_id"] == host && row["profile"] == profile)
}

// ADR 0040 §10 / KTD11 / T32: discovery paths do not authorize execution.
// Preview only measures files, so this also works before a blocked probe.
async fn confirmed_vllm_body(router: &Router, host: &str, mut body: Value) -> Value {
    body["preview"] = json!(true);
    let (status, preview) = http(
        router,
        "POST",
        &host_uri(host, "/engines"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, 200, "{preview}");
    body["preview"] = json!(false);
    body["digest"] = preview["digest"].clone();
    body["confirm_name"] = json!("vllm");
    body
}

async fn add_vllm(router: &Router, host: &str, dir: &Path) -> Value {
    let env = vllm_env(&dir.join("elsewhere/vllm-env"));
    let body = confirmed_vllm_body(
        router,
        host,
        json!({"path": env, "paths": [dir.join("elsewhere")]}),
    )
    .await;
    let (status, result) = http(router, "POST", &host_uri(host, "/engines"), Some(body)).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["published"], "published", "{result}");
    result
}

// T32 / U9: both host name and immutable id selectors reach the chosen host.
#[tokio::test]
async fn detected_builtin_add_over_http_publishes_in_the_server_engines_list() {
    let h = Harness::enrolled().await;
    let mut a = h.agent(None).await;
    let router = h.router();
    let env = vllm_env(&a.dir.path().join("elsewhere/vllm-env"));
    let before = std::fs::read(&a.document).unwrap();
    let query = format!(
        "{}?path={}",
        host_uri("spark", "/engines/detected"),
        a.dir.path().join("elsewhere").display()
    );
    let (status, detected) = http(&router, "GET", &query, None).await;
    assert_eq!(status, 200, "{detected}");
    assert!(detected["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|candidate| candidate["env"] == json!(env) && candidate["engine"] == "vllm"));
    // ADR 0040 §10 / KTD11 / T32: finding an installation through caller
    // paths is not permission to execute it without preview confirmation.
    let body = json!({"path": env, "paths": [a.dir.path().join("elsewhere")]});
    let (status, refused) = http(
        &router,
        "POST",
        &host_uri(&h.host, "/engines"),
        Some(body.clone()),
    )
    .await;
    assert_ne!(status, 200, "{refused}");
    assert_eq!(refused["error"]["code"], "confirmation_required");
    let body = confirmed_vllm_body(&router, &h.host, body).await;
    let (status, added) = http(&router, "POST", &host_uri(&h.host, "/engines"), Some(body)).await;
    assert_eq!(status, 200, "{added}");
    assert_eq!(added["profile"], "vllm", "{added}");
    assert_eq!(added["published"], "published", "{added}");
    let (status, list) = http(&router, "GET", "/management/v1/engines", None).await;
    assert_eq!(status, 200, "{list}");
    let row = engine(&list, &h.host, "vllm").unwrap();
    assert_eq!(row["engine"], "vllm");
    assert_eq!(row["published"], "published");
    assert_eq!(row["retiring"], false);
    let host_list = a.control.handle(ControlRequest::List).await;
    assert_eq!(
        host_list["accepted"]["vllm"]["engine"], "vllm",
        "{host_list}"
    );
    assert_eq!(
        std::fs::read(&a.document).unwrap(),
        before,
        "host.yaml is not rewritten"
    );
    a.stop().await;
}

// T32 / U9 / ADR 0017: capability refusal sends no host-admin frame.
#[tokio::test]
async fn missing_host_admin_capability_returns_422_and_sends_no_request() {
    let h = Harness::enrolled().await;
    let without = capabilities::agent_capabilities()
        .into_iter()
        .filter(|c| c != capabilities::HOST_ADMIN)
        .collect();
    let (_send, mut stream) = h.scripted(without).await;
    let (status, result) = http(
        &h.router(),
        "GET",
        &host_uri("spark", "/engines/detected"),
        None,
    )
    .await;
    assert_eq!(status, 422, "{result}");
    assert_eq!(
        result["error"]["code"],
        "host_capability_missing:host_admin"
    );
    assert_eq!(result["error"]["retryable"], false);
    assert_eq!(next(&mut stream, Duration::from_millis(500)).await, None);
}

// T32 / U9: offline is known now, not after the remote request's deadline.
#[tokio::test]
async fn disconnected_host_returns_503_observation_stale_in_under_one_second() {
    let h = Harness::enrolled().await;
    drop(h.scripted(capabilities::agent_capabilities()).await);
    eventually(|| h.sessions.inspect(&h.host).is_some_and(|v| !v.online)).await;
    let started = tokio::time::Instant::now();
    let (status, result) = http(
        &h.router(),
        "GET",
        &host_uri(&h.host, "/engines/detected"),
        None,
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(status, 503, "{result}");
    assert_eq!(result["error"]["code"], "observation_stale");
}

// T32 / U9: dispatch happened, but the connection died before the answer.
#[tokio::test]
async fn session_lost_mid_request_returns_409_outcome_unknown() {
    let h = Harness::enrolled().await;
    let (send, mut stream) = h.scripted(capabilities::agent_capabilities()).await;
    let router = h.router();
    let uri = host_uri("spark", "/engines/detected");
    let asked = tokio::spawn(async move { http(&router, "GET", &uri, None).await });
    let Some(server_to_agent::Msg::HostAdminRequest(sent)) =
        next(&mut stream, Duration::from_secs(10)).await
    else {
        panic!("expected host administration request");
    };
    assert!(!sent.request_id.is_empty());
    assert!(sent.deadline_unix_ms > capyctl_protocol::now_unix_ms());
    drop((send, stream));
    let (status, result) = tokio::time::timeout(Duration::from_secs(10), asked)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, 409, "{result}");
    assert_eq!(result["error"]["code"], "outcome_unknown");
    assert_eq!(result["error"]["retryable"], false);
}

/// The fixture has no runtime holders. Write the real durable retirement, but
/// pause its confirmation so HTTP inventory can observe the intermediate state.
/// No host-admin operation result or publication is supplied by this fixture.
struct PausedRetirements {
    state: SharedCoordinatorState,
    release: AtomicBool,
}

impl ProfileRetirements for PausedRetirements {
    fn begin(&self, host: &str, profile: &str, key: &str, drain: bool) -> RetirementStep {
        assert!(drain);
        let owned = self.state.lock().unwrap();
        let at = capyctl_protocol::now_unix_ms();
        assert_eq!(
            owned
                .store()
                .begin_profile_retirement(host, profile, key, at, at + 30_000, drain,)
                .unwrap(),
            capyctl_store::profile_retirement::RetirementStart::Clear
        );
        RetirementStep::Draining(vec![])
    }

    fn poll(&self, _: &str, _: &str, _: &str) -> Option<RetirementStep> {
        self.release
            .load(Ordering::Acquire)
            .then_some(RetirementStep::Confirmed)
    }
}

// T32 / U9: retiring stays visible until the real host removes and republishes.
#[tokio::test]
async fn remove_returns_202_and_server_list_shows_retiring_until_host_removal() {
    let h = Harness::enrolled().await;
    let retirements = Arc::new(PausedRetirements {
        state: h.state.clone(),
        release: AtomicBool::new(false),
    });
    h.sessions.with_profile_retirements(retirements.clone());
    let mut a = h.agent(None).await;
    let router = h.router();
    add_vllm(&router, &h.host, a.dir.path()).await;
    let uri = host_uri(&h.host, "/engines/vllm/remove");
    let requester = router.clone();
    let asked =
        tokio::spawn(
            async move { http(&requester, "POST", &uri, Some(json!({"drain": true}))).await },
        );
    eventually(|| {
        h.state
            .lock()
            .unwrap()
            .store()
            .profile_retirement(&h.host, "vllm")
            .unwrap()
            .is_some()
    })
    .await;
    for _ in 0..2 {
        let (status, list) = http(&router, "GET", "/management/v1/engines", None).await;
        assert_eq!(status, 200, "{list}");
        assert_eq!(engine(&list, &h.host, "vllm").unwrap()["retiring"], true);
        let file = capyctl_config::test_support::host_engines_path(&a.document);
        assert!(
            capyctl_config::registration::EnginesFile::load(&file)
                .unwrap()
                .profiles
                .contains_key("vllm"),
            "host has not removed the profile yet"
        );
        assert!(!asked.is_finished());
    }
    retirements.release.store(true, Ordering::Release);
    let (status, result) = tokio::time::timeout(Duration::from_secs(10), asked)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, 202, "{result}");
    assert_eq!(result["removed"], "vllm");
    let (status, list) = http(&router, "GET", "/management/v1/engines", None).await;
    assert_eq!(status, 200, "{list}");
    assert!(engine(&list, &h.host, "vllm").is_none(), "{list}");
    let host_list = a.control.handle(ControlRequest::List).await;
    assert!(host_list["accepted"].get("vllm").is_none(), "{host_list}");
    assert!(h
        .state
        .lock()
        .unwrap()
        .store()
        .profile_retirement(&h.host, "vllm")
        .unwrap()
        .is_none());
    a.stop().await;
}

struct Loaded(String);

impl SessionExecution for Loaded {
    fn execute(&self, _: u64, _: capyctl_protocol::execution::MemberCommand) -> ExecutionFuture {
        Box::pin(async { Err(capyctl_agent::session::SessionError) })
    }

    fn load_reports(&self) -> Option<LoadFuture> {
        let report = capyctl_protocol::reports::LoadReport {
            host_id: self.0.clone(),
            samples: vec![capyctl_protocol::reports::LoadSample {
                deployment_id: "deployment".into(),
                generation: 1,
                owned_handle: "launch-1".into(),
                sampled_at_ms: capyctl_protocol::now_unix_ms(),
                ingress_in_flight: 0,
                engine: None,
                latency: None,
                max_running: None,
            }],
            lmcache: vec![],
        }
        .to_wire();
        Some(Box::pin(async move { vec![report] }))
    }

    fn load_interval(&self) -> Duration {
        capyctl_agent::load::MIN_LOAD_INTERVAL
    }
}

// T32 / U9: block the real add's capability-probe subprocess, not a fake
// operation reply, and observe two fresh reports during that exact add.
#[tokio::test]
async fn long_real_add_keeps_host_load_reports_flowing() {
    let h = Harness::enrolled().await;
    let mut a = h.agent(Some(Arc::new(Loaded(h.host.clone())))).await;
    let env = vllm_env(&a.dir.path().join("elsewhere/vllm-env"));
    let entered = a.dir.path().join("probe-entered");
    let release = a.dir.path().join("probe-release");
    script(&env.join("bin/python3"), &format!(
        "touch '{entered}'\ni=0\nwhile [ ! -f '{release}' ]; do\n  i=$((i + 1))\n  [ \"$i\" -lt 400 ] || exit 1\n  sleep 0.05\ndone\necho '{report}'",
        entered = entered.display(), release = release.display(), report = capability_report(),
    ));
    let router = h.router();
    let uri = host_uri(&h.host, "/engines");
    let body = confirmed_vllm_body(
        &router,
        &h.host,
        json!({"path": env, "paths": [a.dir.path().join("elsewhere")]}),
    )
    .await;
    assert!(
        !entered.exists(),
        "preview must not execute the blocked probe"
    );
    let asked = tokio::spawn(async move { http(&router, "POST", &uri, Some(body)).await });
    eventually(|| entered.exists()).await;
    let table = h.sessions.load_table();
    let key = InstanceKey::new("deployment", 1);
    let sampled = || {
        table
            .fresh(&key, &h.host)
            .map(|sample| sample.sampled_at_ms)
    };
    eventually(|| sampled().is_some()).await;
    let first = sampled().unwrap();
    eventually(|| sampled().is_some_and(|at| at > first)).await;
    let second = sampled().unwrap();
    eventually(|| sampled().is_some_and(|at| at > second)).await;
    assert!(!asked.is_finished(), "the actual add is still probing");
    assert!(!capyctl_config::test_support::host_engines_path(&a.document).exists());
    std::fs::write(&release, b"release").unwrap();
    let (status, result) = tokio::time::timeout(Duration::from_secs(10), asked)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["published"], "published", "{result}");
    let (status, list) = http(&h.router(), "GET", "/management/v1/engines", None).await;
    assert_eq!(status, 200, "{list}");
    assert!(engine(&list, &h.host, "vllm").is_some(), "{list}");
    a.stop().await;
}

// T32 / U9 / AE6: stale files invalidate confirmation before any write;
// a fresh HTTP confirmation pins the measured installation on the real host.
#[tokio::test]
async fn custom_preview_confirm_pins_on_host_and_refuses_confirmation_stale() {
    let h = Harness::enrolled().await;
    let mut a = h.agent(None).await;
    let router = h.router();
    let root = a.dir.path().join("serve-venv");
    let site = root.join("lib/python3.12/site-packages/serve_dep");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    let dependency = site.join("__init__.py");
    std::fs::write(&dependency, "VERSION = 1\n").unwrap();
    let executed = a.dir.path().join("executed");
    let executable = root.join("bin/serve");
    script(&executable, &format!("touch '{}'", executed.display()));
    let spec = json!({
        "operation": "add", "name": "serve", "executable": executable,
        "manifest": "kind: custom_engine\nschema_version: 1\napi: decisions\nversion: \"2.2.0\"\nargs: [--host, \"{host}\", --port, \"{port}\", --model, \"{model_dir}\"]\nhealth: /health\n"
    });
    let preview_uri = host_uri("spark", "/custom-engines/preview");
    let confirm_uri = host_uri(&h.host, "/custom-engines");
    let (status, preview) = http(&router, "POST", &preview_uri, Some(spec.clone())).await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["command"][0], json!(executable));
    assert!(preview["summary"]
        .as_str()
        .is_some_and(|text| !text.is_empty()));
    let file = capyctl_config::registration::custom_engines_beside(
        &capyctl_config::test_support::host_engines_path(&a.document),
    );
    assert!(!file.exists(), "preview writes nothing");
    std::fs::write(&dependency, "VERSION = 2\n").unwrap();
    let mut confirmation = spec.clone();
    confirmation["digest"] = preview["digest"].clone();
    confirmation["confirm_name"] = json!("serve");
    let (status, stale) = http(&router, "POST", &confirm_uri, Some(confirmation.clone())).await;
    assert_eq!(status, 409, "{stale}");
    assert_eq!(stale["error"]["code"], "confirmation_stale");
    assert!(!file.exists(), "stale confirmation writes no profile");
    let (_, list) = http(&router, "GET", "/management/v1/engines", None).await;
    assert!(engine(&list, &h.host, "serve").is_none());
    let (status, fresh) = http(&router, "POST", &preview_uri, Some(spec)).await;
    assert_eq!(status, 200, "{fresh}");
    assert_ne!(preview["digest"], fresh["digest"]);
    confirmation["digest"] = fresh["digest"].clone();
    let (status, confirmed) = http(&router, "POST", &confirm_uri, Some(confirmation)).await;
    assert_eq!(status, 200, "{confirmed}");
    let profiles = capyctl_config::registration::EnginesFile::load(&file)
        .unwrap()
        .profiles;
    assert_eq!(
        profiles["serve"]["custom"]["installation_digest"],
        confirmed["fingerprint"]["digest"]
    );
    assert!(confirmed["fingerprint"]["digest"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
    let (status, list) = http(&router, "GET", "/management/v1/engines", None).await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(engine(&list, &h.host, "serve").unwrap()["engine"], "custom");
    assert!(
        !executed.exists(),
        "custom registration never executes the program"
    );
    a.stop().await;
}

// T32 / U9: configuration is the server's approved publication, even without
// host_admin and after disconnection; neither read sends anything to the host.
#[tokio::test]
async fn approved_configuration_is_readable_without_capability_and_while_offline() {
    let h = Harness::enrolled().await;
    let without = capabilities::agent_capabilities()
        .into_iter()
        .filter(|c| c != capabilities::HOST_ADMIN)
        .collect();
    let (send, mut stream) = h.scripted(without).await;
    let router = h.router();
    let (status, online) = http(&router, "GET", &host_uri("spark", "/config"), None).await;
    assert_eq!(status, 200, "{online}");
    assert_eq!(online["role"], "host");
    let settings = online["settings"].as_array().unwrap();
    assert!(
        settings.iter().any(|setting| setting["path"] == "name"
            && setting["value"] == "lab"
            && setting["source"] == "yaml"),
        "{online}"
    );
    assert!(
        settings
            .iter()
            .any(|setting| setting["path"] == "model_store.path"
                && setting["value"] == "/srv/models"
                && setting["source"] == "yaml"),
        "{online}"
    );
    assert_eq!(next(&mut stream, Duration::from_millis(500)).await, None);
    drop((send, stream));
    eventually(|| h.sessions.inspect(&h.host).is_some_and(|v| !v.online)).await;
    let (status, offline) = http(&router, "GET", &host_uri(&h.host, "/config"), None).await;
    assert_eq!(status, 200, "{offline}");
    assert_eq!(
        offline, online,
        "the same approved document is readable offline"
    );
}

fn verified_copy(store: &Path, key: &str, bytes: usize) {
    std::fs::create_dir_all(store.join(key)).unwrap();
    std::fs::write(store.join(key).join("weights.bin"), vec![0_u8; bytes]).unwrap();
    let state = store.join("sources/.capyctl");
    std::fs::create_dir_all(&state).unwrap();
    let id = hex::encode(&Sha256::digest(key.as_bytes())[..12]);
    std::fs::write(
        state.join(format!("{id}.verified")),
        json!({"key": key, "state": "verified", "bytes": bytes, "files": 1}).to_string(),
    )
    .unwrap();
}

/// Introduce a stopped deployment through the same approved remote-host
/// configuration source the server uses, without importing an embedded policy.
fn reference_checkpoint(h: &Harness) {
    use capyctl_management::configuration::{
        ConfigurationCommand, ConfigurationSource, SharedConfigurationSource,
    };
    let source = SharedConfigurationSource::from_registry(h.state.clone(), "owner").unwrap();
    source
        .accept(
            "reference-created-after-listing",
            ConfigurationCommand::Create {
                config_json: json!({
                    "name":"reference", "engine":"sglang", "host":h.host,
                    "model":{"source":{"huggingface":{
                        "repo":"Qwen/Qwen3-4B",
                        "revision":"0123456789abcdef0123456789abcdef01234567"
                    }}},
                    "engine_config":{"memory":{"kv_cache":"16MiB"}}
                })
                .to_string(),
            },
        )
        .unwrap();
}

// T32 / U9 / SPEC §6.3: list and dry-run are not authorization to delete.
// Creating a deployment between preview and apply must protect its copy.
#[tokio::test]
async fn model_store_prune_recomputes_references_for_each_http_request() {
    let h = Harness::enrolled().await;
    let mut a = h.agent(None).await;
    let router = h.router();
    let models = a.dir.path().join("models");
    let kept = "sources/huggingface/Qwen--Qwen3-4B@0123456789abcdef0123456789abcdef01234567";
    let unused = "sources/http/unused";
    verified_copy(&models, kept, 10);
    verified_copy(&models, unused, 20);
    let list_uri = host_uri("spark", "/model-store");
    let prune_uri = host_uri(&h.host, "/model-store/prune");
    let (status, before) = http(&router, "GET", &list_uri, None).await;
    assert_eq!(status, 200, "{before}");
    assert_eq!(before["copies"].as_array().unwrap().len(), 2);
    assert!(before["copies"]
        .as_array()
        .unwrap()
        .iter()
        .all(|copy| copy["referenced"] == false));
    let (status, preview) = http(&router, "POST", &prune_uri, Some(json!({"apply": false}))).await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["removed_bytes"], 30);
    assert!(models.join(kept).is_dir());
    assert!(models.join(unused).is_dir());
    reference_checkpoint(&h);
    let (status, applied) = http(&router, "POST", &prune_uri, Some(json!({"apply": true}))).await;
    assert_eq!(status, 200, "{applied}");
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["removed_bytes"], 20);
    assert_eq!(applied["removed"][0]["key"], unused);
    assert_eq!(applied["kept"][0]["key"], kept);
    assert!(
        models.join(kept).is_dir(),
        "the newly referenced bytes stay on the host"
    );
    assert!(
        !models.join(unused).exists(),
        "the unused bytes are really removed"
    );
    let (status, after) = http(&router, "GET", &list_uri, None).await;
    assert_eq!(status, 200, "{after}");
    assert_eq!(
        after["copies"],
        json!([{"key": kept, "bytes": 10, "referenced": true}])
    );
    a.stop().await;
}

// T32 / U9 / KTD11: inspect the actual correlated wire envelope. Scripted
// hosts deliberately disconnect without inventing an operation's output.
#[tokio::test]
async fn read_and_add_http_requests_carry_bounded_correlated_deadlines() {
    for (method, suffix, body, deadline_ms) in [
        ("GET", "/engines/detected", None, 30_000),
        (
            "POST",
            "/engines",
            Some(json!({"path": "/unused/vllm"})),
            180_000,
        ),
    ] {
        let h = Harness::enrolled().await;
        let (send, mut stream) = h.scripted(capabilities::agent_capabilities()).await;
        let router = h.router();
        let uri = host_uri(&h.host, suffix);
        let before = capyctl_protocol::now_unix_ms();
        let asked = tokio::spawn(async move { http(&router, method, &uri, body).await });
        let Some(server_to_agent::Msg::HostAdminRequest(sent)) =
            next(&mut stream, Duration::from_secs(10)).await
        else {
            panic!("expected correlated request for {suffix}");
        };
        let after = capyctl_protocol::now_unix_ms();
        assert!(!sent.request_id.is_empty());
        assert!(sent.deadline_unix_ms >= before + deadline_ms);
        assert!(sent.deadline_unix_ms <= after + deadline_ms);
        match sent.op {
            Some(pb::host_admin_request::Op::Detect(_)) => assert_eq!(method, "GET"),
            Some(pb::host_admin_request::Op::AddBuiltin(_)) => assert_eq!(method, "POST"),
            other => panic!("unexpected operation: {other:?}"),
        }
        assert_eq!(
            next(&mut stream, Duration::from_millis(100)).await,
            None,
            "one HTTP operation dispatches only once"
        );
        drop((send, stream));
        let (status, result) = tokio::time::timeout(Duration::from_secs(10), asked)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, 409, "{result}");
        assert_eq!(result["error"]["code"], "outcome_unknown");
        assert_eq!(result["error"]["retryable"], false);
    }
}

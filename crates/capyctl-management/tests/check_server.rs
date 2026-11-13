//! U7 / U9b, R21–R22: the HTTP deployment check reaches a real host over
//! mTLS and uses that host's source policy and real cached pinner. The hub
//! fixture supplies metadata only: pinning, sizing and fit are production code.

use axum::{
    body::{to_bytes, Body},
    extract::Path as HttpPath,
    http::{HeaderMap, Request},
    routing::get,
    Extension, Json, Router,
};
use capyctl_agent::{
    engines::ScanRoots,
    enrollment::{JoinInvitation, PendingEnrollment},
    host_admin::{CachedSourcePinner, RoleKind, Target},
    host_admin_remote::{HostAdminService, InProcessRole},
    host_control::HostControl,
    identity::{CertificateAuthority, HostKey},
    identity_storage::IdentityDirectory,
    profiles::{HostProfiles, ProfileSet},
    session::{run_session_with_updates, ProfileUpdates},
};
use capyctl_config::model_source::ModelSourcePolicy;
use capyctl_controller::{
    agent_sessions::AgentSessions, enrollment::EnrollmentAuthority,
    ownership::SharedCoordinatorState, OwnedCoordinatorState,
};
use capyctl_management::{
    check, configuration::SharedConfigurationSource, host_admin_remote::RemoteHostAdmin,
    ui_sessions::Principal,
};
use capyctl_protocol::pb::{
    self, agent_control_server::AgentControlServer, bootstrap_server::Bootstrap,
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tower::ServiceExt;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const HOST_TOKEN: &str = "fixture-host-private-token";

fn now() -> i64 {
    capyctl_protocol::now_unix_ms() / 1000
}

fn directory() -> tempfile::TempDir {
    // Role ownership refuses /tmp and other world-writable ancestors.
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn private_directory(path: &Path) {
    std::fs::DirBuilder::new().mode(0o700).create(path).unwrap();
}

fn prepared_document(root: &Path, mirror: &Mirror) -> Value {
    let golden: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut host = golden["input"]["host"].clone();
    host["name"] = json!("spark");
    host["state_dir"] = json!(root.join("state"));
    host["identity_dir"] = json!(root.join("state/identity"));
    host["model_store"]["path"] = json!(root.join("models"));
    let token = root.join("hf-token");
    std::fs::write(&token, format!("{HOST_TOKEN}\n")).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    host["model_sources"] = json!({
        "huggingface_endpoint": mirror.endpoint(),
        "huggingface_token_file": token,
    });
    // The real session publishes the runner's physical memory. As in the
    // controller's host_admin fixture, keep the approved policy below it.
    let domain = &mut host["resource_policy"]["domains"]["unified"];
    let bytes = |v: &Value| capyctl_config::effective::parse_bytes(v.as_str().unwrap()).unwrap();
    let capacity = capyctl_agent::memory::read_host_memory()
        .unwrap()
        .memory
        .capacity_bytes as f64;
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

fn inventory(host: &str, document: &Value) -> pb::ReportInventory {
    let config = capyctl_config::remote_roles::HostConfig::parse(&document.to_string()).unwrap();
    let now = capyctl_protocol::now_unix_ms();
    pb::ReportInventory {
        domains: vec![pb::DomainObservation {
            domain_id: "unified".into(),
            kind: "system".into(),
            observed_bytes: 100 << 30,
            observed_at_unix: now / 1000,
            capacity_bytes: 128 << 30,
            available_bytes: 100 << 30,
            observed_at_unix_ms: now,
            ..Default::default()
        }],
        profiles: config
            .profiles
            .iter()
            .map(|(name, profile)| pb::RuntimeProfileStatus {
                name: name.clone(),
                build_fingerprint: profile["build_fingerprint"].as_str().unwrap().into(),
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
    dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn enrolled() -> Harness {
    let state_dir = directory();
    let state = Arc::new(Mutex::new(
        OwnedCoordinatorState::open(state_dir.path()).unwrap(),
    ));
    let ca = CertificateAuthority::generate(now()).unwrap();
    let ca_pem = ca.certificate_pem().to_owned();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_server("localhost", &key, now()).unwrap();
    let authority = Arc::new(EnrollmentAuthority::new(state.clone(), ca));
    let sessions = AgentSessions::new(authority.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(cert.pem, key.private_key_pem()))
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
    let storage_dir = directory();
    let storage = IdentityDirectory::open(storage_dir.path()).unwrap();
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
    Harness {
        state,
        sessions,
        identity,
        host,
        server,
        dirs: (state_dir, storage_dir),
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

struct Agent {
    stop: tokio::sync::watch::Sender<bool>,
    session: Option<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Agent {
    async fn stop(mut self) {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.session.take().unwrap())
            .await
            .unwrap()
            .unwrap();
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if let Some(session) = &self.session {
            session.abort();
        }
    }
}

async fn agent(h: &Harness, mirror: &Mirror, serve_admin: bool) -> Agent {
    let dir = directory();
    for name in ["etc", "state", "models", "home"] {
        private_directory(&dir.path().join(name));
    }
    let document = prepared_document(dir.path(), mirror);
    let path = dir.path().join("etc/host.yaml");
    std::fs::write(&path, document.to_string()).unwrap();
    let config = capyctl_config::remote_roles::HostConfig::parse(&document.to_string()).unwrap();
    let inv = inventory(&h.host, &document);
    let profiles = HostProfiles::new(ProfileSet::new(config.clone(), inv.clone()));
    let updates = ProfileUpdates::new(profiles);
    let journal =
        capyctl_agent::journal::HostJournal::open(dir.path(), &h.identity.controller_id(), &h.host)
            .unwrap();
    let control = HostControl::new(
        path.clone(),
        capyctl_config::registration::engines_beside(&path),
        config,
        updates.clone(),
        journal.clone(),
    );
    if serve_admin {
        let mut policy = ModelSourcePolicy::from_raw(Some(
            serde_json::from_value(document["model_sources"].clone()).unwrap(),
        ))
        .unwrap();
        // Same loopback HTTP transport seam as tests/check.rs. The approved
        // document remains HTTPS; only the host fixture's client transport is
        // changed. No source policy, credentials or pinner exists on the server.
        policy.huggingface_endpoint = policy
            .huggingface_endpoint
            .map(|endpoint| endpoint.replacen("https://127.0.0.1:", "http://127.0.0.1:", 1));
        let home = dir.path().join("home");
        updates.serve_host_admin(Arc::new(HostAdminService {
            host_id: h.host.clone(),
            target: Target {
                engines: capyctl_config::registration::engines_beside(&path),
                role_document: path,
                kind: RoleKind::Host,
                socket: dir
                    .path()
                    .join("state")
                    .join(capyctl_agent::control_socket::SOCKET_NAME),
                state_dir: dir.path().join("state"),
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
                system: dir.path().join("no-bin").display().to_string(),
                default_cuda_home: dir.path().join("no-cuda"),
                system_root: dir.path().join("no-root"),
            },
            cuda_home_env: None,
            measurer: Arc::new(capyctl_agent::installation::InstallationMeasurer::new()),
            role: InProcessRole(control),
            model_env: Default::default(),
            pinner: Arc::new(CachedSourcePinner::new(
                policy,
                dir.path().join("models"),
                // Never inherit credentials from the test runner's shell.
                Arc::new(|_| None),
            )),
        }));
    }
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let identity =
        PendingEnrollment::load(&IdentityDirectory::open(h.dirs.1.path()).unwrap()).unwrap();
    let session = tokio::spawn(async move {
        run_session_with_updates(&identity, journal, inv, shutdown, None, None, Some(updates))
            .await
            .unwrap();
    });
    eventually(|| h.sessions.inspect(&h.host).is_some_and(|v| v.reconciled)).await;
    Agent {
        stop,
        session: Some(session),
        _dir: dir,
    }
}

struct Mirror {
    origin: String,
    references: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Mirror {
    fn endpoint(&self) -> String {
        self.origin.replacen("http:", "https:", 1)
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mirror() -> Mirror {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let origin = format!("http://{address}");
    let references = Arc::new(Mutex::new(Vec::new()));
    let seen = references.clone();
    let app = Router::new().route(
        "/api/models/org/model/revision/{reference}",
        get(
            move |HttpPath(reference): HttpPath<String>, headers: HeaderMap| {
                let seen = seen.clone();
                let address = address.clone();
                async move {
                    // Both the reference and immutable-commit requests must hit
                    // this host's mirror and use its protected token file.
                    assert_eq!(headers.get("host").unwrap(), address.as_str());
                    assert_eq!(
                        headers.get("authorization").unwrap(),
                        format!("Bearer {HOST_TOKEN}").as_str()
                    );
                    seen.lock().unwrap().push(reference);
                    Json(json!({"sha":SHA,"siblings":[
                        {"rfilename":"model.safetensors","size":1 << 30},
                        // Ancillary metadata is not checkpoint weight memory.
                        {"rfilename":"config.json","size":128_u64 << 30}
                    ]}))
                }
            },
        ),
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mirror {
        origin,
        references,
        task,
    }
}

fn app(h: &Harness) -> Router {
    let configuration =
        Arc::new(SharedConfigurationSource::from_registry(h.state.clone(), "owner").unwrap());
    check::router(
        configuration,
        RemoteHostAdmin::new(h.state.clone(), h.sessions.clone()),
    )
    .layer(Extension(Principal::Admin))
}

async fn request(app: &Router, host: &str) -> (u16, Value) {
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        app.clone().oneshot(
            Request::post("/management/v1/deployments/check")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"config":{
                        "name":"preview", "engine":"vllm", "host":host,
                        "model":{"hf":"org/model"},
                        // Above this host's <=32GiB managed limit on every
                        // runner; test the fit verdict without assuming RAM.
                        "engine_config":{"memory":{"kv_cache":"64GiB"}}
                    }})
                    .to_string(),
                ))
                .unwrap(),
        ),
    )
    .await
    .expect("a local preview or refusal must not wait for the remote deadline")
    .unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

// U7 R21–R22 / ADR 0040 §9: real mTLS host dispatch, host-only endpoint and
// token, weight sizing, fit, normalized source and debounced pin caching.
#[tokio::test]
async fn server_check_uses_the_chosen_hosts_mirror_token_and_cached_pinner() {
    let hub = mirror().await;
    let h = enrolled().await;
    let a = agent(&h, &hub, true).await;
    let app = app(&h);
    let (status, value) = request(&app, "spark").await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "does_not_fit", "{value}");
    assert!(value["limit"]
        .as_str()
        .unwrap()
        .starts_with("insufficient_capacity:"));
    assert_eq!(value["estimated"], true);
    assert_eq!(value["errors"], json!([]));
    assert_eq!(value["config"]["model"]["source"]["repo"], "org/model");
    assert_eq!(value["config"]["model"]["source"]["revision"], SHA);
    assert!(value["config"]["model"].get("hf").is_none());
    assert_eq!(value["hosts"].as_array().unwrap().len(), 1);
    assert_eq!(value["hosts"][0]["host_id"], h.host);
    assert_eq!(value["hosts"][0]["verdict"], "does_not_fit");
    assert_eq!(value["hosts"][0]["config"], value["config"]);
    assert!(value["yaml"].as_str().unwrap().contains(SHA));
    assert_eq!(*hub.references.lock().unwrap(), vec!["main", SHA]);
    assert_eq!(request(&app, "spark").await, (200, value));
    assert_eq!(*hub.references.lock().unwrap(), vec!["main", SHA]);
    assert_eq!(
        h.state.lock().unwrap().store().deployment_count().unwrap(),
        0
    );
    a.stop().await;
}

// U7 / U9b, ADR 0040 §§9–10: a cached pin must not let a server substitute
// its own source access when the chosen host disconnects.
#[tokio::test]
async fn server_check_refuses_an_offline_host_even_after_a_successful_pin() {
    let hub = mirror().await;
    let h = enrolled().await;
    let a = agent(&h, &hub, true).await;
    let app = app(&h);
    let (status, value) = request(&app, &h.host).await;
    assert_eq!(status, 200, "{value}");
    a.stop().await;
    eventually(|| h.sessions.inspect(&h.host).is_some_and(|v| !v.online)).await;
    let (status, value) = request(&app, &h.host).await;
    assert_eq!(status, 503, "{value}");
    assert_eq!(value["error"]["code"], "observation_stale", "{value}");
    assert_eq!(value["error"]["retryable"], false);
    assert_eq!(*hub.references.lock().unwrap(), vec!["main", SHA]);
}

// U7 / U9b, ADR 0017 / ADR 0040 §10: without the capability, the source
// operation is refused instead of attempting the server's default hub.
#[tokio::test]
async fn server_check_refuses_a_host_without_host_admin() {
    let hub = mirror().await;
    let h = enrolled().await;
    let a = agent(&h, &hub, false).await;
    let (status, value) = request(&app(&h), "spark").await;
    assert_eq!(status, 422, "{value}");
    assert_eq!(
        value["error"]["code"], "host_capability_missing:host_admin",
        "{value}"
    );
    assert_eq!(value["error"]["retryable"], false);
    assert!(hub.references.lock().unwrap().is_empty());
    a.stop().await;
}

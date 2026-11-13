//! ADR 0040 §10, ADR 0017: host administration carried over a real mTLS
//! control session. The server's helper refuses without sending to a host
//! that cannot take it, answers an offline host at once, and reports a lost
//! session or a missed deadline; a real host session runs requests off its
//! loop. Fake installations only: CPU evidence, never qualification.

use capyctl_agent::{
    enrollment::{JoinInvitation, PendingEnrollment},
    identity::{CertificateAuthority, HostKey},
    identity_storage::IdentityDirectory,
};
use capyctl_controller::{
    agent_sessions::AgentSessions,
    enrollment::EnrollmentAuthority,
    host_admin_remote::{request, Refusal},
    load_table::InstanceKey,
    ownership::SharedCoordinatorState,
    OwnedCoordinatorState,
};
use capyctl_protocol::{
    capabilities,
    pb::{
        self, agent_control_client::AgentControlClient, agent_control_server::AgentControlServer,
        agent_to_server, bootstrap_server::Bootstrap, host_admin_request::Op, server_to_agent,
    },
    version::BINARY_VERSION,
};
use prost::Message;
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

fn now() -> i64 {
    capyctl_protocol::now_unix_ms() / 1000
}

fn directory() -> tempfile::TempDir {
    let d = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

/// An approved host preparation with one SGLang runtime profile.
fn prepared_document() -> Value {
    let text = include_str!("../../capyctl-config/tests/fixtures/effective-sglang-golden.json");
    let mut host = serde_json::from_str::<Value>(text).unwrap()["input"]["host"].clone();
    host["state_dir"] = json!("/home/operator/.local/state/capyctl");
    host["identity_dir"] = json!("/home/operator/.local/state/capyctl/identity");
    fit_to_host(&mut host["resource_policy"]["domains"]["unified"]);
    host
}

/// The agent reports real host memory; shrink the policy to fit a small runner.
fn fit_to_host(domain: &mut Value) {
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
}

/// The inventory a prepared host reports.
fn inventory(host: &str) -> pb::ReportInventory {
    let document = prepared_document();
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
    dirs: (tempfile::TempDir, tempfile::TempDir),
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
    let service = AgentControlServer::from_arc(sessions.clone())
        .max_encoding_message_size(capyctl_protocol::SERVER_MESSAGE_BYTES)
        .max_decoding_message_size(capyctl_protocol::HOST_MESSAGE_BYTES);
    let server = tokio::spawn(async move {
        let _ = Server::builder()
            .tls_config(tls)
            .unwrap()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await;
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
    let request = identity.request(&invitation).unwrap();
    let certificate = Bootstrap::enroll(authority.as_ref(), tonic::Request::new(request))
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

type Opened = (
    tokio::sync::mpsc::Sender<pb::AgentToServer>,
    tonic::Streaming<pb::ServerToAgent>,
);

impl Harness {
    /// A reconciled control session of a scripted host declaring `version`
    /// and `capabilities`.
    async fn reconciled(&self, version: &str, capabilities: Vec<String>) -> Opened {
        let channel = self
            .identity
            .control_endpoint(now())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let (send, recv) = tokio::sync::mpsc::channel(64);
        for msg in [
            agent_to_server::Msg::Connect(pb::Connect {
                host_id: self.host.clone(),
                protocol_version: capyctl_protocol::PROTOCOL_VERSION.into(),
                binary_version: version.into(),
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
            .max_decoding_message_size(capyctl_protocol::SERVER_MESSAGE_BYTES)
            .max_encoding_message_size(capyctl_protocol::HOST_MESSAGE_BYTES)
            .session(ReceiverStream::new(recv))
            .await
            .unwrap()
            .into_inner();
        match stream.message().await.unwrap().unwrap().msg {
            Some(server_to_agent::Msg::SessionReady(_)) => {}
            other => panic!("expected SessionReady, got {other:?}"),
        }
        (send, stream)
    }
}

/// The next message other than a heartbeat, or `None` within `wait`.
async fn next(
    stream: &mut tonic::Streaming<pb::ServerToAgent>,
    wait: Duration,
) -> Option<server_to_agent::Msg> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, stream.message()).await {
            Err(_) => return None,
            Ok(message) => match message.ok()??.msg {
                Some(server_to_agent::Msg::Heartbeat(_)) => continue,
                other => return other,
            },
        }
    }
}

fn detect() -> Op {
    Op::Detect(pb::DetectEngines { extra_dirs: vec![] })
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

// T34 / ADR 0040 §10 / KTD11: a host that did not declare `host_admin` is
// refused typed and sent nothing.
#[tokio::test]
async fn a_host_without_the_capability_is_refused_and_sent_nothing() {
    let h = enrolled().await;
    let without: Vec<String> = capabilities::agent_capabilities()
        .into_iter()
        .filter(|c| c != capabilities::HOST_ADMIN)
        .collect();
    let (_send, mut stream) = h.reconciled(BINARY_VERSION, without).await;
    let refused = request(&h.sessions, &h.host, detect(), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "host_capability_missing:host_admin");
    assert_eq!(next(&mut stream, Duration::from_millis(500)).await, None);
    h.server.abort();
}

// T34 / ADR 0040 §10 / KTD11: version refusal takes precedence over a
// missing capability, including hosts predating capability declarations.
#[tokio::test]
async fn a_drain_only_host_is_refused_upgrade_required() {
    for declared in [vec![], vec![capabilities::HOST_ADMIN.to_owned()]] {
        let h = enrolled().await;
        let (_send, mut stream) = h.reconciled("", declared).await;
        let refused = request(&h.sessions, &h.host, detect(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(refused, Refusal::HostUpgradeRequired);
        assert_eq!(refused.code(), "host_upgrade_required");
        assert_eq!(next(&mut stream, Duration::from_millis(500)).await, None);
        h.server.abort();
    }
}

// T34 / ADR 0040 §10 / KTD11: a host with no live session is answered at
// once, even without a previously recorded capability declaration.
#[tokio::test]
async fn an_offline_host_is_refused_at_once() {
    let h = enrolled().await;
    let refused = request(&h.sessions, &h.host, detect(), Duration::from_secs(30))
        .await
        .unwrap_err();
    assert_eq!(refused, Refusal::ObservationStale);
    let opened = h.reconciled(BINARY_VERSION, vec![]).await;
    drop(opened);
    eventually(|| h.sessions.inspect(&h.host).is_some_and(|v| !v.online)).await;
    let started = tokio::time::Instant::now();
    let refused = request(&h.sessions, &h.host, detect(), Duration::from_secs(30))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "observation_stale");
    assert!(started.elapsed() < Duration::from_secs(1));
    h.server.abort();
}

// ADR 0040 §10: the request reaches the host with an id and a deadline;
// the host's reply naming that id is the answer.
#[tokio::test]
async fn a_reply_naming_the_request_answers_it() {
    let h = enrolled().await;
    let (send, mut stream) = h
        .reconciled(BINARY_VERSION, capabilities::agent_capabilities())
        .await;
    let sessions = h.sessions.clone();
    let host = h.host.clone();
    let asked =
        tokio::spawn(
            async move { request(&sessions, &host, detect(), Duration::from_secs(5)).await },
        );
    let Some(server_to_agent::Msg::HostAdminRequest(sent)) =
        next(&mut stream, Duration::from_secs(5)).await
    else {
        panic!("expected a HostAdminRequest");
    };
    assert!(!sent.request_id.is_empty());
    assert!(sent.deadline_unix_ms > capyctl_protocol::now_unix_ms());
    assert!(matches!(sent.op, Some(Op::Detect(_))));
    // A reply naming another request settles nothing.
    for request_id in ["other".to_owned(), sent.request_id.clone()] {
        send.send(pb::AgentToServer {
            msg: Some(agent_to_server::Msg::HostAdminReply(pb::HostAdminReply {
                result_json: json!({"answered": request_id}).to_string(),
                request_id,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
    }
    let reply = asked.await.unwrap().unwrap();
    assert_eq!(reply.request_id, sent.request_id);
    assert_eq!(
        serde_json::from_str::<Value>(&reply.result_json).unwrap()["answered"],
        json!(sent.request_id)
    );
    h.server.abort();
}

// ADR 0040 §10: a session lost with the request outstanding answers
// `outcome_unknown` without waiting out the deadline.
#[tokio::test]
async fn a_lost_session_answers_outcome_unknown() {
    let h = enrolled().await;
    let (send, mut stream) = h
        .reconciled(BINARY_VERSION, capabilities::agent_capabilities())
        .await;
    let sessions = h.sessions.clone();
    let host = h.host.clone();
    let asked =
        tokio::spawn(
            async move { request(&sessions, &host, detect(), Duration::from_secs(30)).await },
        );
    assert!(matches!(
        next(&mut stream, Duration::from_secs(5)).await,
        Some(server_to_agent::Msg::HostAdminRequest(_))
    ));
    drop((send, stream));
    let refused = tokio::time::timeout(Duration::from_secs(10), asked)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(refused.code(), "outcome_unknown");
    h.server.abort();
}

// ADR 0040 §10: no answer by the deadline answers `deadline_exceeded`.
#[tokio::test]
async fn an_unanswered_request_answers_deadline_exceeded() {
    let h = enrolled().await;
    let (_send, mut stream) = h
        .reconciled(BINARY_VERSION, capabilities::agent_capabilities())
        .await;
    let refused = request(&h.sessions, &h.host, detect(), Duration::from_millis(300))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "deadline_exceeded");
    assert!(matches!(
        next(&mut stream, Duration::from_secs(1)).await,
        Some(server_to_agent::Msg::HostAdminRequest(_))
    ));
    h.server.abort();
}

// T34 / ADR 0040 §10 / KTD11: a replacement session's declaration alone
// authorizes dispatch; an old session's outstanding operation is not replayed.
#[tokio::test]
async fn capability_and_dispatch_follow_the_current_session() {
    let h = enrolled().await;
    let (_old_send, mut old_stream) = h
        .reconciled(BINARY_VERSION, capabilities::agent_capabilities())
        .await;
    let sessions = h.sessions.clone();
    let host = h.host.clone();
    let asked =
        tokio::spawn(
            async move { request(&sessions, &host, detect(), Duration::from_secs(30)).await },
        );
    assert!(matches!(
        next(&mut old_stream, Duration::from_secs(5)).await,
        Some(server_to_agent::Msg::HostAdminRequest(_))
    ));
    let without: Vec<String> = capabilities::agent_capabilities()
        .into_iter()
        .filter(|c| c != capabilities::HOST_ADMIN)
        .collect();
    let (_send, mut stream) = h.reconciled(BINARY_VERSION, without).await;
    let lost = tokio::time::timeout(Duration::from_secs(5), asked)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(lost, Refusal::OutcomeUnknown);
    let refused = request(&h.sessions, &h.host, detect(), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "host_capability_missing:host_admin");
    assert_eq!(next(&mut stream, Duration::from_millis(500)).await, None);

    let (send, mut stream) = h
        .reconciled(BINARY_VERSION, capabilities::agent_capabilities())
        .await;
    let sessions = h.sessions.clone();
    let host = h.host.clone();
    let asked =
        tokio::spawn(
            async move { request(&sessions, &host, detect(), Duration::from_secs(5)).await },
        );
    let Some(server_to_agent::Msg::HostAdminRequest(sent)) =
        next(&mut stream, Duration::from_secs(5)).await
    else {
        panic!("expected a HostAdminRequest on the replacement session");
    };
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::HostAdminReply(pb::HostAdminReply {
            request_id: sent.request_id.clone(),
            result_json: "{}".into(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    assert_eq!(asked.await.unwrap().unwrap().request_id, sent.request_id);
    h.server.abort();
}

/// A detection operation whose complete ServerToAgent frame has `bytes`
/// encoded bytes, including the request id, deadline and nested wrappers.
fn detect_sized(bytes: usize) -> Op {
    let mut wire = pb::ServerToAgent {
        msg: Some(server_to_agent::Msg::HostAdminRequest(
            pb::HostAdminRequest {
                request_id: "0".repeat(26),
                deadline_unix_ms: capyctl_protocol::now_unix_ms() + 30_000,
                op: Some(Op::Detect(pb::DetectEngines {
                    extra_dirs: vec!["x".repeat(bytes)],
                })),
            },
        )),
    };
    let overhead = wire.encoded_len() - bytes;
    let Some(server_to_agent::Msg::HostAdminRequest(request)) = wire.msg.as_mut() else {
        unreachable!();
    };
    let Some(Op::Detect(detect)) = request.op.as_mut() else {
        unreachable!();
    };
    detect.extra_dirs[0].truncate(bytes - overhead);
    assert_eq!(wire.encoded_len(), bytes);
    let Some(server_to_agent::Msg::HostAdminRequest(request)) = wire.msg else {
        unreachable!();
    };
    request.op.unwrap()
}

// T19 T37 / ADR 0040 §§10–11 / KTD11–12: all host-admin operations obey
// the complete 64 KiB wire bound before enqueueing; refusal keeps the stream
// usable and never truncates the references protecting a model-store prune.
#[tokio::test]
async fn oversized_requests_are_refused_without_ending_the_session() {
    let h = enrolled().await;
    let (send, mut stream) = h
        .reconciled(BINARY_VERSION, capabilities::agent_capabilities())
        .await;
    let session_id = h.sessions.inspect(&h.host).unwrap().session_id;
    let limit = capyctl_protocol::SERVER_MESSAGE_BYTES;
    let manifest = "x".repeat(48 * 1024);
    let custom = pb::CustomEngineSpec {
        manifest,
        env: [("CONFIG".into(), "x".repeat(20 * 1024))]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let oversized = [
        detect_sized(limit + 1),
        Op::PreviewBuiltin(pb::BuiltinEngine {
            args: vec!["x".repeat(limit)],
            ..Default::default()
        }),
        Op::AddBuiltin(pb::AddBuiltinEngine {
            extra_dirs: vec!["x".repeat(limit)],
            ..Default::default()
        }),
        Op::PreviewCustom(custom.clone()),
        Op::ConfirmCustom(pb::ConfirmCustomEngine {
            engine: Some(custom),
            ..Default::default()
        }),
        Op::ModelStoreList(pb::ModelStoreList {
            referenced: vec!["x".repeat(256); 256],
        }),
        Op::ModelStorePrune(pb::ModelStorePrune {
            referenced: vec!["x".repeat(256); 256],
            apply: true,
        }),
        Op::PinAndSize(pb::PinAndSize {
            sources: vec!["x".repeat(limit)],
        }),
    ];
    for op in oversized {
        let refused = request(&h.sessions, &h.host, op, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(refused, Refusal::RequestTooLarge);
        assert_eq!(refused.code(), "request_too_large");
    }
    assert_eq!(next(&mut stream, Duration::from_millis(500)).await, None);

    // The limit itself is accepted, not reserved for protobuf overhead.
    let sessions = h.sessions.clone();
    let host = h.host.clone();
    let asked = tokio::spawn(async move {
        request(
            &sessions,
            &host,
            detect_sized(limit),
            Duration::from_secs(30),
        )
        .await
    });
    let Some(server_to_agent::Msg::HostAdminRequest(sent)) =
        next(&mut stream, Duration::from_secs(5)).await
    else {
        panic!("expected an exactly 64 KiB HostAdminRequest");
    };
    let wire = pb::ServerToAgent {
        msg: Some(server_to_agent::Msg::HostAdminRequest(sent.clone())),
    };
    assert_eq!(wire.encoded_len(), limit);
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::HostAdminReply(pb::HostAdminReply {
            request_id: sent.request_id,
            result_json: "{}".into(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    assert_eq!(asked.await.unwrap().unwrap().result_json, "{}");
    let current = h.sessions.inspect(&h.host).unwrap();
    assert!(current.online && current.reconciled);
    assert_eq!(current.session_id, session_id);
    h.server.abort();
}

use capyctl_agent::control_socket::ControlHandler;
use capyctl_agent::engines::ScanRoots;
use capyctl_agent::host_admin::{RoleKind, Target};
use capyctl_agent::host_admin_remote::{
    HostAdminHandler, HostAdminService, InProcessRole, ReplyFuture,
};
use capyctl_agent::host_control::HostControl;
use capyctl_agent::profiles::{HostProfiles, ProfileSet};
use capyctl_agent::session::{
    run_session_with_updates, ExecutionFuture, LoadFuture, ProfileUpdates, SessionExecution,
};

/// A real host session serving host administration with `handler`.
struct Agent {
    control: Arc<HostControl>,
    document: PathBuf,
    stop: tokio::sync::watch::Sender<bool>,
    session: tokio::task::JoinHandle<()>,
    dir: tempfile::TempDir,
}

impl Agent {
    async fn stop(self) {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.session)
            .await
            .unwrap()
            .unwrap();
    }
}

/// Reports one fresh load sample for `deployment` generation 1 every tick.
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
            lmcache: Vec::new(),
        }
        .to_wire();
        Some(Box::pin(async move { vec![report] }))
    }
    fn load_interval(&self) -> Duration {
        capyctl_agent::load::MIN_LOAD_INTERVAL
    }
}

async fn agent(
    h: &Harness,
    handler: impl FnOnce(&Path, Arc<HostControl>) -> Arc<dyn HostAdminHandler>,
    execution: Option<Arc<dyn SessionExecution>>,
) -> Agent {
    let dir = directory();
    let etc = dir.path().join("etc");
    std::fs::create_dir(&etc).unwrap();
    let document = etc.join("host.yaml");
    std::fs::write(&document, prepared_document().to_string()).unwrap();
    let config =
        capyctl_config::remote_roles::HostConfig::parse(&prepared_document().to_string()).unwrap();
    let profiles = HostProfiles::new(ProfileSet::new(config.clone(), inventory(&h.host)));
    let updates = ProfileUpdates::new(profiles);
    let journal =
        capyctl_agent::journal::HostJournal::open(dir.path(), &h.identity.controller_id(), &h.host)
            .unwrap();
    let control = HostControl::new(
        document.clone(),
        capyctl_config::registration::engines_beside(&document),
        config,
        updates.clone(),
        journal.clone(),
    );
    updates.serve_host_admin(handler(dir.path(), control.clone()));
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let identity =
        PendingEnrollment::load(&IdentityDirectory::open(h.dirs.1.path()).unwrap()).unwrap();
    let (inv, u) = (inventory(&h.host), updates.clone());
    let session = tokio::spawn(async move {
        let _ =
            run_session_with_updates(&identity, journal, inv, shutdown, execution, None, Some(u))
                .await;
    });
    eventually(|| h.sessions.inspect(&h.host).is_some_and(|v| v.reconciled)).await;
    Agent {
        control,
        document,
        stop,
        session,
        dir,
    }
}

/// Holds every operation until released, answering with its request id.
struct Held(tokio::sync::Semaphore);

impl HostAdminHandler for Held {
    fn handle(self: Arc<Self>, request: pb::HostAdminRequest) -> ReplyFuture {
        Box::pin(async move {
            let _ = self.0.acquire().await.unwrap();
            pb::HostAdminReply {
                request_id: request.request_id,
                result_json: "{}".into(),
                ..Default::default()
            }
        })
    }
}

// ADR 0040 §10: a long operation runs off the host's session loop: the host
// keeps sending load reports, and the server keeps taking them, until the
// operation answers.
#[tokio::test]
async fn load_reports_continue_while_an_operation_runs() {
    let h = enrolled().await;
    let held = Arc::new(Held(tokio::sync::Semaphore::new(0)));
    let handler = held.clone();
    let a = agent(
        &h,
        move |_, _| handler,
        Some(Arc::new(Loaded(h.host.clone()))),
    )
    .await;
    let sessions = h.sessions.clone();
    let host = h.host.clone();
    let asked =
        tokio::spawn(
            async move { request(&sessions, &host, detect(), Duration::from_secs(30)).await },
        );
    let table = h.sessions.load_table();
    let key = InstanceKey::new("deployment", 1);
    let sampled = || table.fresh(&key, &h.host).map(|v| v.sampled_at_ms);
    eventually(|| sampled().is_some()).await;
    let first = sampled().unwrap();
    eventually(|| sampled().is_some_and(|at| at > first)).await;
    let second = sampled().unwrap();
    eventually(|| sampled().is_some_and(|at| at > second)).await;
    assert!(!asked.is_finished(), "the operation is still running");
    held.0.add_permits(1);
    let reply = tokio::time::timeout(Duration::from_secs(10), asked)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply.result_json, "{}");
    a.stop().await;
    h.server.abort();
}

/// Hold a real host-local operation after dispatch, without tying its
/// lifetime to the controller's caller.
struct DelayedAdmin {
    service: Arc<HostAdminService>,
    started: Arc<tokio::sync::Notify>,
    released: Arc<tokio::sync::Semaphore>,
}

impl HostAdminHandler for DelayedAdmin {
    fn handle(self: Arc<Self>, request: pb::HostAdminRequest) -> ReplyFuture {
        Box::pin(async move {
            let _permit = if matches!(request.op.as_ref(), Some(Op::AddBuiltin(_))) {
                self.started.notify_one();
                Some(self.released.acquire().await.unwrap())
            } else {
                None
            };
            self.service.clone().handle(request).await
        })
    }
}

// T08 T37 / ADR 0040 §§10–11 / KTD7, KTD11–12: cancellation ends only the
// caller's wait. An already dispatched, confirmed engine add still writes and
// publishes its profile; its late reply does not end the control session.
#[tokio::test]
async fn cancelling_the_caller_preserves_a_dispatched_engine_add() {
    let h = enrolled().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let released = Arc::new(tokio::sync::Semaphore::new(0));
    let (signal, gate) = (started.clone(), released.clone());
    let host_id = h.host.clone();
    let a = agent(
        &h,
        move |dir, control| {
            Arc::new(DelayedAdmin {
                service: host_service(dir, control, host_id),
                started: signal,
                released: gate,
            })
        },
        None,
    )
    .await;
    let session_id = h.sessions.inspect(&h.host).unwrap().session_id;
    let elsewhere = a.dir.path().join("elsewhere");
    let env = vllm_env(&elsewhere.join("vllm-env"));
    let engine = pb::BuiltinEngine {
        path: env.display().to_string(),
        name: "vllm".into(),
        ..Default::default()
    };
    let preview = request(
        &h.sessions,
        &h.host,
        Op::PreviewBuiltin(engine.clone()),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert!(preview.error_code.is_empty(), "{}", preview.error_message);
    let preview: Value = serde_json::from_str(&preview.result_json).unwrap();
    let digest = preview["digest"].as_str().unwrap().to_owned();
    let sessions = h.sessions.clone();
    let host = h.host.clone();
    let asked = tokio::spawn(async move {
        request(
            &sessions,
            &host,
            Op::AddBuiltin(pb::AddBuiltinEngine {
                engine: Some(engine),
                extra_dirs: vec![elsewhere.display().to_string()],
                digest,
                typed_name: "vllm".into(),
            }),
            Duration::from_secs(30),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    asked.abort();
    assert!(asked.await.unwrap_err().is_cancelled());
    let engines = capyctl_config::registration::engines_beside(&a.document);
    assert!(!engines.exists(), "the dispatched add is still held");
    released.add_permits(1);
    eventually(|| {
        h.state
            .lock()
            .unwrap()
            .store()
            .host_publication(&h.host)
            .unwrap()
            .is_some_and(|publication| publication.config_json.contains("\"vllm\""))
    })
    .await;
    assert!(capyctl_config::registration::EnginesFile::load(&engines)
        .unwrap()
        .profiles
        .contains_key("vllm"));
    let listed = a
        .control
        .handle(capyctl_agent::control_socket::ControlRequest::List)
        .await;
    assert_eq!(listed["accepted"]["vllm"]["engine"], "vllm");
    let reply = request(&h.sessions, &h.host, detect(), Duration::from_secs(30))
        .await
        .unwrap();
    assert!(reply.error_code.is_empty(), "{}", reply.error_message);
    let current = h.sessions.inspect(&h.host).unwrap();
    assert!(current.online && current.reconciled);
    assert_eq!(current.session_id, session_id);
    a.stop().await;
    h.server.abort();
}

fn script(path: &Path, body: &str) {
    capyctl_config::test_support::write_executable(path, format!("#!/bin/sh\n{body}\n"), 0o755)
        .unwrap();
}

/// A vLLM venv whose `vllm --version` prints the version and whose
/// interpreter answers the capability probe.
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
    let report = json!({"schema": "capyctl/engine-capabilities/v1", "engine": "vllm",
        "capabilities": {"core": [], "deep_park": [], "metrics": []}});
    script(&root.join("bin/python3"), &format!("echo '{report}'"));
    root.to_path_buf()
}

fn host_service(dir: &Path, control: Arc<HostControl>, host_id: String) -> Arc<HostAdminService> {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let document = dir.join("etc/host.yaml");
    Arc::new(HostAdminService {
        host_id,
        target: Target {
            engines: capyctl_config::registration::engines_beside(&document),
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

/// The harness has no running deployments: retire through the real store,
/// while the production service additionally drains any runtime holders.
struct UnusedProfileRetirements(SharedCoordinatorState);
impl capyctl_controller::profile_retirement::ProfileRetirements for UnusedProfileRetirements {
    fn begin(
        &self,
        host: &str,
        profile: &str,
        key: &str,
        drain: bool,
    ) -> capyctl_controller::profile_retirement::RetirementStep {
        assert!(!drain);
        let owned = self.0.lock().unwrap();
        let now = capyctl_protocol::now_unix_ms();
        let outcome = owned
            .store()
            .begin_profile_retirement(host, profile, key, now, now + 30_000, false)
            .unwrap();
        assert_eq!(
            outcome,
            capyctl_store::profile_retirement::RetirementStart::Clear
        );
        assert!(
            owned
                .store()
                .profile_retirement(host, profile)
                .unwrap()
                .is_some(),
            "retiring stays published until the host removes the profile"
        );
        capyctl_controller::profile_retirement::RetirementStep::Confirmed
    }
    fn poll(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Option<capyctl_controller::profile_retirement::RetirementStep> {
        panic!("an unused profile retires synchronously")
    }
}

// T37 / ADR 0040 §§10–11 / KTD11–12: the server adds a built-in engine
// detected under an extra search directory only after preview confirmation;
// the host writes and publishes its profile, and the server approves it.
#[tokio::test]
async fn a_detected_builtin_engine_is_added_through_the_server() {
    let h = enrolled().await;
    h.sessions
        .with_profile_retirements(Arc::new(UnusedProfileRetirements(h.state.clone())));
    let host_id = h.host.clone();
    let a = agent(
        &h,
        move |dir, control| host_service(dir, control, host_id),
        None,
    )
    .await;
    let elsewhere = a.dir.path().join("elsewhere");
    let env = vllm_env(&elsewhere.join("vllm-env"));
    let host_before = std::fs::read(&a.document).unwrap();

    let detected = request(
        &h.sessions,
        &h.host,
        Op::Detect(pb::DetectEngines {
            extra_dirs: vec![elsewhere.display().to_string()],
        }),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(detected.error_code, "", "{}", detected.error_message);
    let detected: Value = serde_json::from_str(&detected.result_json).unwrap();
    assert!(
        detected["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["env"] == json!(env) && c["engine"] == "vllm"),
        "{detected}"
    );

    let engine = pb::BuiltinEngine {
        path: env.display().to_string(),
        name: "vllm".into(),
        ..Default::default()
    };
    let preview = request(
        &h.sessions,
        &h.host,
        Op::PreviewBuiltin(engine.clone()),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert!(preview.error_code.is_empty(), "{}", preview.error_message);
    let preview: Value = serde_json::from_str(&preview.result_json).unwrap();

    let added = request(
        &h.sessions,
        &h.host,
        Op::AddBuiltin(pb::AddBuiltinEngine {
            engine: Some(engine),
            extra_dirs: vec![elsewhere.display().to_string()],
            digest: preview["digest"].as_str().unwrap().into(),
            typed_name: "vllm".into(),
        }),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(added.error_code, "", "{}", added.error_message);
    let added: Value = serde_json::from_str(&added.result_json).unwrap();
    assert_eq!(added["profile"], "vllm", "{added}");
    assert_eq!(added["published"], "published", "{added}");
    let approved = h
        .state
        .lock()
        .unwrap()
        .store()
        .host_publication(&h.host)
        .unwrap()
        .unwrap();
    assert!(approved.config_json.contains("\"vllm\""));
    assert_eq!(std::fs::read(&a.document).unwrap(), host_before);
    let list = a
        .control
        .handle(capyctl_agent::control_socket::ControlRequest::List)
        .await;
    assert_eq!(list["accepted"]["vllm"]["engine"], "vllm", "{list}");
    let removed = request(
        &h.sessions,
        &h.host,
        Op::Remove(pb::RemoveEngine {
            name: "vllm".into(),
            drain: false,
        }),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(removed.error_code, "", "{}", removed.error_message);
    let list = a
        .control
        .handle(capyctl_agent::control_socket::ControlRequest::List)
        .await;
    assert!(list["accepted"].get("vllm").is_none(), "{list}");
    assert!(h
        .state
        .lock()
        .unwrap()
        .store()
        .profile_retirement(&h.host, "vllm")
        .unwrap()
        .is_none());
    a.stop().await;
    h.server.abort();
}

// U9 / ADR 0040 §§9–10: real host dispatch sizes a local checkpoint, with
// its source identity and request id carried intact through the protocol.
#[tokio::test]
async fn pin_and_size_runs_on_the_host_and_returns_weight_sizes() {
    let h = enrolled().await;
    let host_id = h.host.clone();
    let a = agent(
        &h,
        move |dir, control| host_service(dir, control, host_id),
        None,
    )
    .await;
    let model = a.dir.path().join("checkpoint");
    std::fs::create_dir(&model).unwrap();
    std::fs::write(model.join("model.safetensors"), vec![0; 4096]).unwrap();
    std::fs::write(model.join("config.json"), b"{}").unwrap();
    let source = model.display().to_string();
    let reply = request(
        &h.sessions,
        &h.host,
        Op::PinAndSize(pb::PinAndSize {
            sources: vec![source.clone()],
        }),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert!(!reply.request_id.is_empty());
    assert!(reply.error_code.is_empty(), "{}", reply.error_message);
    assert!(reply.result_json.is_empty());
    let answers = reply.pin_and_size.unwrap().sources;
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].source, source);
    assert_eq!(answers[0].pinned, source);
    assert_eq!(answers[0].weight_bytes, Some(4096));
    assert!(answers[0].estimated);
    assert!(answers[0].error.is_empty());
    a.stop().await;
    h.server.abort();
}

// U9 / AE6: custom confirmations cross the same protocol. Changed files
// invalidate the digest before anything is written; a fresh confirmation
// pins the actual installation on the host and publishes it to the server.
#[tokio::test]
async fn custom_confirmation_is_pinned_on_the_host_and_stale_digest_is_refused() {
    let h = enrolled().await;
    let host_id = h.host.clone();
    let a = agent(
        &h,
        move |dir, control| host_service(dir, control, host_id),
        None,
    )
    .await;
    let root = a.dir.path().join("serve-venv");
    let site = root.join("lib/python3.12/site-packages/serve_dep");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    let dependency = site.join("__init__.py");
    std::fs::write(&dependency, "VERSION = 1\n").unwrap();
    let executed = a.dir.path().join("executed");
    let executable = root.join("bin/serve");
    script(&executable, &format!("touch {}", executed.display()));
    let spec = pb::CustomEngineSpec {
        operation: "add".into(),
        name: "serve".into(),
        executable: executable.display().to_string(),
        manifest: r#"kind: custom_engine
schema_version: 1
api: decisions
version: "2.2.0"
args: [--host, "{host}", --port, "{port}", --model, "{model_dir}"]
health: /health
"#
        .into(),
        ..Default::default()
    };
    let preview = request(
        &h.sessions,
        &h.host,
        Op::PreviewCustom(spec.clone()),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert!(preview.error_code.is_empty(), "{}", preview.error_message);
    let preview: Value = serde_json::from_str(&preview.result_json).unwrap();
    let file = capyctl_config::registration::custom_engines_beside(
        &capyctl_config::registration::engines_beside(&a.document),
    );
    assert!(!file.exists(), "preview writes nothing");
    std::fs::write(&dependency, "VERSION = 2\n").unwrap();
    let stale = request(
        &h.sessions,
        &h.host,
        Op::ConfirmCustom(pb::ConfirmCustomEngine {
            engine: Some(spec.clone()),
            digest: preview["digest"].as_str().unwrap().into(),
            typed_name: "serve".into(),
        }),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(stale.error_code, "confirmation_stale");
    assert!(!file.exists());
    let fresh = request(
        &h.sessions,
        &h.host,
        Op::PreviewCustom(spec.clone()),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert!(fresh.error_code.is_empty(), "{}", fresh.error_message);
    let fresh: Value = serde_json::from_str(&fresh.result_json).unwrap();
    assert_ne!(preview["digest"], fresh["digest"]);
    let confirmed = request(
        &h.sessions,
        &h.host,
        Op::ConfirmCustom(pb::ConfirmCustomEngine {
            engine: Some(spec),
            digest: fresh["digest"].as_str().unwrap().into(),
            typed_name: "serve".into(),
        }),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert!(
        confirmed.error_code.is_empty(),
        "{}",
        confirmed.error_message
    );
    let confirmed: Value = serde_json::from_str(&confirmed.result_json).unwrap();
    let profiles = capyctl_config::registration::EnginesFile::load(&file)
        .unwrap()
        .profiles;
    assert_eq!(
        profiles["serve"]["custom"]["installation_digest"],
        confirmed["fingerprint"]["digest"]
    );
    let approved = h
        .state
        .lock()
        .unwrap()
        .store()
        .host_publication(&h.host)
        .unwrap()
        .unwrap();
    assert!(approved.config_json.contains("\"serve\""));
    assert!(
        !executed.exists(),
        "custom registration never executes the program"
    );
    a.stop().await;
    h.server.abort();
}

//! ADR 0017 (owner decision 2026-09-24): the host/server version skew policy
//! and the post-baseline capability gate, over real mTLS control sessions.
//!
//! A host on the server's minor line (or one behind) is supported; an older,
//! other-major or unversioned host is connected drain-only; a newer host is
//! refused with "upgrade the server first". A command needing a feature the
//! host did not declare is refused, typed, and never sent.
//!
//! CPU-only transport tests: nothing here qualifies a native engine.

use mllm_agent::{
    enrollment::{JoinInvitation, PendingEnrollment},
    identity::{CertificateAuthority, HostKey},
    identity_storage::IdentityDirectory,
};
use mllm_adapters::traits::{RuntimeAction, RuntimeCommand, RuntimeError};
use mllm_controller::{
    agent_sessions::{gate_refusal, AgentSessions},
    remote_execution::{self, ReadinessLedger, RemoteLaunchBinding},
    coordinator::ServiceObservation,
    enrollment::EnrollmentAuthority,
    ownership::SharedCoordinatorState,
    OwnedCoordinatorState,
};
use mllm_domain::{
    completion::{ExecutionIdentities, ProcessIdentity, StepExecutionContext, TransitionToken},
    group::{CommandIdentity, MemberKey},
};
use mllm_protocol::{
    capabilities,
    execution::{MemberAction, MemberCommand, SingleLaunchPlan},
    pb::{
        self, agent_control_client::AgentControlClient, agent_control_server::AgentControlServer,
        agent_to_server, bootstrap_server::Bootstrap, server_to_agent,
    },
    version::{Version, BINARY_VERSION},
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

fn now() -> i64 {
    mllm_protocol::now_unix_ms() / 1000
}

fn directory() -> tempfile::TempDir {
    let d = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

/// An approved host preparation with one SGLang runtime profile.
fn prepared_document() -> Value {
    let text = include_str!("../../mllm-config/tests/fixtures/effective-sglang-golden.json");
    let mut host = serde_json::from_str::<Value>(text).unwrap()["input"]["host"].clone();
    host["state_dir"] = json!("/home/operator/.local/state/mllm");
    host["identity_dir"] = json!("/home/operator/.local/state/mllm/identity");
    host
}

/// The inventory a prepared host reports.
fn inventory(host: &str) -> pb::ReportInventory {
    let document = prepared_document();
    let config = mllm_config::remote_roles::HostConfig::parse(&document.to_string()).unwrap();
    let now = mllm_protocol::now_unix_ms();
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
                build_fingerprint: profile["build_fingerprint"].as_str().unwrap_or("unknown").into(),
                eligibility: "unknown".into(),
                ..Default::default()
            })
            .collect(),
        approved_host_config_json: config.document.to_string(),
        host_boot_id: "boot".into(),
        policy_fingerprint: mllm_config::remote_resources::policy_fingerprint(&config.document),
        envelope: Some(pb::Envelope {
            host_id: host.into(),
            protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

struct Harness {
    state: SharedCoordinatorState,
    authority: Arc<EnrollmentAuthority>,
    sessions: Arc<AgentSessions>,
    identity: PendingEnrollment,
    host: String,
    server: tokio::task::JoinHandle<()>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn enrolled() -> Harness {
    let state_dir = directory();
    let state = Arc::new(Mutex::new(OwnedCoordinatorState::open(state_dir.path()).unwrap()));
    let ca = CertificateAuthority::generate(now()).unwrap();
    let ca_pem = ca.certificate_pem().to_owned();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_server("localhost", &key, now()).unwrap();
    let authority = Arc::new(EnrollmentAuthority::new(state.clone(), ca));
    let sessions = AgentSessions::new(authority.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("https://localhost:{}", listener.local_addr().unwrap().port());
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(cert.pem, key.private_key_pem()))
        .client_ca_root(Certificate::from_pem(&ca_pem));
    let service = AgentControlServer::from_arc(sessions.clone());
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
    let host = identity.accept_certificate(&storage, certificate, now()).unwrap();
    Harness { state, authority, sessions, identity, host, server, _dirs: (state_dir, storage_dir) }
}

type Opened = (tokio::sync::mpsc::Sender<pb::AgentToServer>, tonic::Streaming<pb::ServerToAgent>);

impl Harness {
    /// Open a control session as a host declaring `version` and
    /// `capabilities`. `Err` is the controller's refusal of the session.
    async fn open(&self, version: &str, capabilities: Vec<String>) -> Result<Opened, Box<tonic::Status>> {
        let channel = self.identity.control_endpoint(now()).unwrap().connect().await.unwrap();
        let (send, recv) = tokio::sync::mpsc::channel(64);
        send.send(pb::AgentToServer {
            msg: Some(agent_to_server::Msg::Connect(pb::Connect {
                host_id: self.host.clone(),
                protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
                binary_version: version.into(),
                capabilities,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let stream = AgentControlClient::new(channel)
            .session(ReceiverStream::new(recv))
            .await
            .map_err(Box::new)?
            .into_inner();
        Ok((send, stream))
    }

    /// As `open`, then publish a prepared inventory and reconcile.
    async fn reconciled(&self, version: &str, capabilities: Vec<String>) -> Opened {
        let (send, mut stream) = self.open(version, capabilities).await.expect("session accepted");
        send.send(pb::AgentToServer {
            msg: Some(agent_to_server::Msg::ReportInventory(inventory(&self.host))),
        })
        .await
        .unwrap();
        send.send(pb::AgentToServer {
            msg: Some(agent_to_server::Msg::ReconcileHistory(pb::ReconcileHistory {
                records: vec![],
                complete: true,
            })),
        })
        .await
        .unwrap();
        match stream.message().await.unwrap().unwrap().msg {
            Some(server_to_agent::Msg::SessionReady(_)) => {}
            other => panic!("expected SessionReady, got {other:?}"),
        }
        (send, stream)
    }

    fn command(&self, id: &str, action: MemberAction) -> MemberCommand {
        let mut command = MemberCommand {
            identity: CommandIdentity {
                controller_id: self.authority.controller_id(),
                member: MemberKey { host_id: self.host.clone(), member_id: "head".into() },
                deployment_id: "deployment".into(),
                operation_id: format!("operation-{id}"),
                command_id: id.into(),
                step_id: id.into(),
                generation: 1,
                revision: 1,
                deadline_ms: mllm_protocol::now_unix_ms() + 20_000,
                payload_digest: [0; 32],
                expected_state: "reserved".into(),
                profile_fingerprint: "sglang-0.5.20".into(),
                instance_index: 0,
            },
            action,
        };
        command.identity.payload_digest = command.canonical_digest();
        command
    }
}

/// A launch carrying the WE3 digest and a startup reservation, as this server
/// sends every launch.
fn launch() -> MemberAction {
    let fixture: Value =
        serde_json::from_str(include_str!("../../mllm-config/tests/fixtures/f2-deployment.json")).unwrap();
    MemberAction::LaunchSingle(SingleLaunchPlan {
        deployment_config: fixture["deployment"].to_string(),
        profile_name: "sglang".into(),
        checkpoint_fingerprint: "sha256:model".into(),
        host_policy_fingerprint: "a".repeat(64),
        binding_id: "01K00000000000000000000001".into(),
        incarnation: "01K00000000000000000000002".into(),
        grant_id: "01K00000000000000000000003".into(),
        service_port: 8100,
        issued_at_ms: 1,
        coordinator_session_id: "01K00000000000000000000004".into(),
        checkpoint_digest: format!("sha256:{}", "1".repeat(64)),
        checkpoint_weights_bytes: Some(1 << 30),
        startup_bytes: Some(2 << 30),
    })
}

fn recorded() -> ProcessIdentity {
    ProcessIdentity { role: "api".into(), pid: 4242, boot_id: "boot".into(), start_ticks: 7 }
}

/// The typed refusal, answered at once (never left to the deadline).
async fn refused(sessions: &AgentSessions, command: MemberCommand) -> String {
    let status = tokio::time::timeout(Duration::from_secs(5), sessions.execute(command))
        .await
        .expect("a gate refusal is answered at once")
        .expect_err("the command is refused");
    gate_refusal(&status).expect("a typed gate refusal").to_owned()
}

/// The next ExecuteMember the host is sent within `within`, if any.
async fn next_command(
    stream: &mut tonic::Streaming<pb::ServerToAgent>,
    within: Duration,
) -> Option<pb::ExecuteMember> {
    tokio::time::timeout(within, async {
        loop {
            match stream.message().await {
                Ok(Some(pb::ServerToAgent { msg: Some(server_to_agent::Msg::ExecuteMember(c)) })) => return Some(c),
                Ok(Some(_)) => continue,
                _ => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

fn all() -> Vec<String> {
    capabilities::agent_capabilities()
}

// T06 T33 T34: a host that predates the policy (no version, no capability
// list) connects drain-only: status says `upgrade_required` with the reason,
// it is no placement candidate, a start or a park is refused typed and never
// sent, while a stop (Terminate) and a probe are still delivered. The stop
// carries no recorded identities the host could not decode.
#[tokio::test]
async fn a_drain_only_host_refuses_start_but_allows_stop() {
    let h = enrolled().await;
    let (_send, mut stream) = h.reconciled("", vec![]).await;
    let view = h.sessions.inspect(&h.host).unwrap();
    assert!(view.online && view.reconciled);
    assert_eq!(view.compatibility, "upgrade_required");
    assert!(view.compatibility_reason.contains("drain-only"), "{}", view.compatibility_reason);
    assert!(!view.eligible, "a drain-only host takes no new placement");
    assert!(!h.sessions.eligible_hosts().unwrap().contains(&h.host));
    // Owner decision 2026-09-25: a start refused for it names the host and
    // why, with both versions, rather than reporting capacity.
    let why = h.sessions.ineligible_hosts();
    let why = why.get(&h.host).expect("the drain-only host has a reason");
    assert!(
        why.starts_with(&format!("host {} is drain-only (upgrade_required)", h.host)),
        "{why}"
    );
    assert!(why.contains("host version unreported"), "{why}");
    assert!(
        why.contains(&format!("server version {BINARY_VERSION}")),
        "{why}"
    );
    assert!(
        h.sessions.online_hosts().unwrap().contains(&h.host),
        "it can still be stopped"
    );
    // Status evidence survives in the store.
    let record = h.state.lock().unwrap().store().host_version(&h.host).unwrap().unwrap();
    assert_eq!(record.compatibility, "upgrade_required");
    assert_eq!(record.binary_version, "");

    // Start, park and wake are refused typed, before anything is sent.
    assert_eq!(refused(&h.sessions, h.command("launch", launch())).await, "host_upgrade_required");
    let handle = || "01K00000000000000000000009".to_owned();
    assert_eq!(
        refused(&h.sessions, h.command("park", MemberAction::Park { owned_handle: handle() })).await,
        "host_upgrade_required"
    );
    assert_eq!(h.sessions.preflight(&h.host, &[], true).unwrap_err(), "host_upgrade_required");
    assert!(h.sessions.preflight(&h.host, &[], false).is_ok());
    assert!(next_command(&mut stream, Duration::from_millis(300)).await.is_none(), "nothing was sent");

    // A stop is delivered. The server only includes recorded identities for a
    // host that declared them (ADR 0016 field); this one did not.
    assert!(!h.sessions.supports(&h.host, capabilities::TERMINATE_RECORDED_PROCESSES));
    let with_identities = h.command(
        "stop-recorded",
        MemberAction::Terminate { owned_handle: handle(), recorded: vec![recorded()] },
    );
    assert_eq!(
        refused(&h.sessions, with_identities).await,
        "host_capability_missing:terminate_recorded_processes"
    );
    let stop = h.command("stop", MemberAction::Terminate { owned_handle: handle(), recorded: vec![] });
    let sessions = h.sessions.clone();
    let pending = tokio::spawn(async move { sessions.execute(stop).await });
    let sent = next_command(&mut stream, Duration::from_secs(5)).await.expect("the stop is sent");
    assert!(matches!(sent.action, Some(pb::execute_member::Action::TerminateOwnedHandle(_))));
    assert!(sent.terminate_recorded_processes.is_empty());
    assert!(sent.restore_checkpoint_digest.is_empty());
    pending.abort();

    // A probe is delivered too.
    let probe = h.command("probe", MemberAction::Probe { owned_handle: handle() });
    let sessions = h.sessions.clone();
    let pending = tokio::spawn(async move { sessions.execute(probe).await });
    let sent = next_command(&mut stream, Duration::from_secs(5)).await.expect("the probe is sent");
    assert!(matches!(sent.action, Some(pb::execute_member::Action::ProbeOwnedHandle(_))));
    pending.abort();
    h.server.abort();
}

// T16 T34: found by the rc.2 live validation (2026-09-24). A park of a Ready
// engine on a drain-only host is refused before anything is sent, and the
// coordinator settles it leaving the remote launch's dispatch closed until a
// fresh probe reopens it. The refusal must therefore forget the launch's
// readiness proof, so the readiness supervisor sends that probe (a drain-only
// host still takes probes). With the proof kept, the engine kept running but
// stayed closed to dispatch until the host reconnected.
#[tokio::test]
async fn a_park_refused_before_sending_forgets_readiness_so_a_probe_reopens_dispatch() {
    let h = enrolled().await;
    let (_send, mut stream) = h.reconciled("", vec![]).await;
    let MemberAction::LaunchSingle(plan) = launch() else { unreachable!() };
    let binding_id = plan.binding_id.clone();
    let incarnation = plan.incarnation.clone();
    let readiness: ReadinessLedger = Default::default();
    readiness.lock().unwrap().insert(binding_id.clone(), "proving-session".into());
    let engine = remote_execution::engine(
        h.sessions.clone(),
        h.state.clone(),
        RemoteLaunchBinding {
            controller_id: h.authority.controller_id(),
            host_id: h.host.clone(),
            member_id: "head".into(),
            profile_fingerprint: "sglang-0.5.20".into(),
            launch_command_id: "01K00000000000000000000005".into(),
            plan,
            ingress_gate_key: [7; 32],
            instance_index: 0,
        },
        readiness.clone(),
    );
    let park = RuntimeCommand {
        action: RuntimeAction::Park,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: "deployment".into(),
                revision: 1,
                generation: 1,
                operation_id: "01K00000000000000000000006".into(),
                step_id: "01K00000000000000000000007".into(),
            },
            binding_id: binding_id.clone(),
            incarnation,
            issued_at_ms: 1,
            deadline_ms: mllm_protocol::now_unix_ms() + 20_000,
            identities: ExecutionIdentities::Retained(vec![recorded()]),
            completion_target: None,
            grant_id: None,
            launch_settings: None,
        },
    };
    assert_eq!(
        engine.execute_persisted(&park).await.unwrap_err(),
        RuntimeError::Refused("host_upgrade_required".into())
    );
    assert!(
        !readiness.lock().unwrap().contains_key(&binding_id),
        "the refused park must leave the launch to a fresh readiness probe"
    );
    assert!(next_command(&mut stream, Duration::from_millis(300)).await.is_none(), "nothing was sent");
    h.server.abort();
}

// T06 T34: a host newer than the server is refused with the policy sentence,
// "upgrade the server first"; nothing is registered for it but the record.
#[tokio::test]
async fn a_newer_host_is_refused_with_upgrade_the_server_first() {
    let h = enrolled().await;
    let own = Version::parse(BINARY_VERSION).unwrap();
    for newer in [format!("{}.{}.0", own.major, own.minor + 1), format!("{}.0.0", own.major + 1)] {
        let status = h.open(&newer, all()).await.expect_err("a newer host is refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().starts_with(mllm_protocol::version::NEWER_HOST_REFUSAL), "{}", status.message());
        assert!(status.message().contains("upgrade the server first"), "{}", status.message());
        assert!(h.sessions.inspect(&h.host).is_none(), "no session was registered");
        let record = h.state.lock().unwrap().store().host_version(&h.host).unwrap().unwrap();
        assert_eq!(record.compatibility, "refused");
        assert_eq!(record.binary_version, newer);
    }
    // The same line with a newer patch, a pre-release or build metadata is
    // supported.
    let same_line = format!("{}.{}.{}+build.9", own.major, own.minor, own.patch + 3);
    let (_send, _stream) = h.reconciled(&same_line, all()).await;
    let view = h.sessions.inspect(&h.host).unwrap();
    assert_eq!(view.compatibility, "supported");
    assert_eq!(view.binary_version, same_line);
    assert!(view.eligible);
    h.server.abort();
}

// T06 T34: a supported host that lacks one post-baseline feature is refused,
// typed, exactly the operations that need it; placement leaves it out when
// the feature is a placement requirement; status names what it lacks.
#[tokio::test]
async fn a_missing_capability_is_refused_typed_and_never_sent() {
    let h = enrolled().await;
    let mut declared = all();
    declared.retain(|c| c != capabilities::STARTUP_BYTES);
    let (_send, mut stream) = h.reconciled(BINARY_VERSION, declared).await;
    let view = h.sessions.inspect(&h.host).unwrap();
    assert_eq!(view.compatibility, "supported");
    assert_eq!(view.capabilities_missing, vec![capabilities::STARTUP_BYTES.to_owned()]);
    assert!(!view.eligible, "a placement requirement is missing");
    assert!(!h.sessions.eligible_hosts().unwrap().contains(&h.host));
    assert_eq!(
        refused(&h.sessions, h.command("launch", launch())).await,
        "host_capability_missing:startup_bytes"
    );
    assert_eq!(
        h.sessions.preflight(&h.host, &[capabilities::STARTUP_BYTES], true).unwrap_err(),
        "host_capability_missing:startup_bytes"
    );
    // What it declared still reaches it: a wake with the recorded digest.
    assert!(h.sessions.supports(&h.host, capabilities::RESTORE_CHECKPOINT_DIGEST));
    let wake = h.command(
        "wake",
        MemberAction::Restore {
            owned_handle: "01K00000000000000000000009".into(),
            checkpoint_digest: format!("sha256:{}", "1".repeat(64)),
        },
    );
    let sessions = h.sessions.clone();
    let pending = tokio::spawn(async move { sessions.execute(wake).await });
    let sent = next_command(&mut stream, Duration::from_secs(5)).await.expect("the wake is sent");
    assert!(!sent.restore_checkpoint_digest.is_empty());
    pending.abort();
    h.server.abort();
}

// T06 T33: one minor release behind is supported with an upgrade recommended:
// it is eligible and takes launches; status shows its version and the advice.
#[tokio::test]
async fn an_n_minus_one_host_is_supported_with_an_upgrade_recommended() {
    let own = Version::parse(BINARY_VERSION).unwrap();
    let h = enrolled().await;
    let previous = if own.minor > 0 {
        format!("{}.{}.4", own.major, own.minor - 1)
    } else {
        // Not expressible on this line; the policy table covers it.
        return;
    };
    let (_send, mut stream) = h.reconciled(&previous, all()).await;
    let view = h.sessions.inspect(&h.host).unwrap();
    assert_eq!(view.compatibility, "upgrade_recommended");
    assert!(view.compatibility_reason.contains("upgrade the host"));
    assert_eq!(view.binary_version, previous);
    assert!(view.eligible);
    let sessions = h.sessions.clone();
    let start = h.command("launch", launch());
    let pending = tokio::spawn(async move { sessions.execute(start).await });
    assert!(next_command(&mut stream, Duration::from_secs(5)).await.is_some(), "the launch is sent");
    pending.abort();
    h.server.abort();
}

// T06: the host listing's evidence: status shows the host's version and
// verdict, and the store keeps them after the session ends.
#[tokio::test]
async fn status_shows_the_version_after_the_session_ends() {
    let h = enrolled().await;
    let (send, stream) = h.reconciled(BINARY_VERSION, all()).await;
    let view = h.sessions.inspect(&h.host).unwrap();
    assert_eq!(view.binary_version, BINARY_VERSION);
    assert_eq!(view.compatibility, "supported");
    assert!(view.capabilities_missing.is_empty());
    let serialized = serde_json::to_value(&view).unwrap();
    assert_eq!(serialized["binary_version"], BINARY_VERSION);
    assert_eq!(serialized["compatibility"], "supported");
    drop((send, stream));
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.sessions.inspect(&h.host).is_some_and(|v| v.online) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let record = h.state.lock().unwrap().store().host_version(&h.host).unwrap().unwrap();
    assert_eq!(record.binary_version, BINARY_VERSION);
    assert_eq!(record.compatibility, "supported");
    assert_eq!(record.capabilities.len(), capabilities::CATALOGUE.len());
    // Offline, the recorded declaration still answers what the host supports.
    assert!(h.sessions.supports(&h.host, capabilities::TERMINATE_RECORDED_PROCESSES));
    h.server.abort();
}

// T06: a malformed capability declaration is refused like any malformed
// Connect.
#[tokio::test]
async fn a_malformed_declaration_is_refused() {
    let h = enrolled().await;
    let status = h.open(BINARY_VERSION, vec!["Not A Name".into()]).await.expect_err("refused");
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    h.server.abort();
}

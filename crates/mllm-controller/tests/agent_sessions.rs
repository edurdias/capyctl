use mllm_agent::{
    enrollment::{JoinInvitation, PendingEnrollment},
    identity::{CertificateAuthority, HostKey},
    identity_storage::IdentityDirectory,
    journal::HostJournal,
};
use mllm_controller::{
    agent_sessions::AgentSessions, enrollment::EnrollmentAuthority, OwnedCoordinatorState,
};
use mllm_protocol::pb::{
    self, agent_control_client::AgentControlClient, agent_control_server::AgentControlServer,
    agent_to_server, bootstrap_server::Bootstrap,
};
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
async fn eventually(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
struct JournalExecutor {
    journal: Arc<HostJournal>,
    fresh: Arc<std::sync::atomic::AtomicUsize>,
    lose_first: std::sync::atomic::AtomicBool,
}
struct InspectPolicy;
impl mllm_agent::journal::LocalExecutionPolicy for InspectPolicy {
    fn authorize(&self, command: &mllm_protocol::execution::MemberCommand) -> Result<(), mllm_agent::journal::JournalError> {
        if matches!(command.action, mllm_protocol::execution::MemberAction::Inspect) { Ok(()) }
        else { Err(mllm_agent::journal::JournalError::Unauthorized) }
    }
    fn render_launch(&self, _: &mllm_protocol::execution::MemberCommand) -> Result<mllm_agent::journal::ApprovedLaunch, mllm_agent::journal::JournalError> {
        Err(mllm_agent::journal::JournalError::Unauthorized)
    }
}
impl mllm_agent::session::SessionExecution for JournalExecutor {
    fn execute(&self, session: u64, command: mllm_protocol::execution::MemberCommand) -> mllm_agent::session::ExecutionFuture {
        let journal = self.journal.clone();
        let fresh = self.fresh.clone();
        let lose = self.lose_first.swap(false, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let now = mllm_protocol::now_unix_ms();
                match journal.accept(session, &command, now, &InspectPolicy).map_err(|_| mllm_agent::session::SessionError)? {
                    mllm_agent::journal::Acceptance::Fresh(ticket) => {
                        fresh.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        journal.execute(ticket, now, &InspectPolicy).map_err(|_| mllm_agent::session::SessionError)?;
                    }
                    mllm_agent::journal::Acceptance::Replay(_) => {}
                }
                if lose { return Err(mllm_agent::session::SessionError); }
                journal.execution_result(&command.identity.command_id, mllm_protocol::now_unix_ms()).map_err(|_| mllm_agent::session::SessionError)
            }).await.map_err(|_| mllm_agent::session::SessionError)?
        })
    }
}
// T05 T06 T33 T34: actual mTLS, persistent identity, fencing, active revocation.
#[tokio::test]
async fn outbound_reconnect_fences_old_stream_and_revocation_closes_current() {
    let state_dir = directory();
    let state = Arc::new(Mutex::new(
        OwnedCoordinatorState::open(state_dir.path()).unwrap(),
    ));
    let ca = CertificateAuthority::generate(now()).unwrap();
    let ca_pem = ca.certificate_pem().to_owned();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_server("localhost", &key, now()).unwrap();
    let authority = Arc::new(EnrollmentAuthority::new(state, ca));
    let sessions = AgentSessions::new(authority.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let server = tokio::spawn(
        Server::builder()
            .tls_config(
                ServerTlsConfig::new()
                    .identity(Identity::from_pem(cert.pem, key.private_key_pem()))
                    .client_ca_root(Certificate::from_pem(&ca_pem)),
            )
            .unwrap()
            .add_service(
                AgentControlServer::from_arc(sessions.clone()).max_decoding_message_size(65536),
            )
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
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
    let channel = identity
        .control_endpoint(now())
        .unwrap()
        .connect()
        .await
        .unwrap();
    // SPEC §13.1, T34: a wrong host, an unknown version and a version 1 peer
    // (which cannot read version 2 actions and reports) are all refused.
    for (claimed_host, version) in [
        ("wrong-host", mllm_protocol::PROTOCOL_VERSION),
        (host.as_str(), "unsupported"),
        (host.as_str(), "1"),
    ] {
        let (send, recv) = tokio::sync::mpsc::channel(16);
        send.send(pb::AgentToServer {
            msg: Some(agent_to_server::Msg::Connect(pb::Connect {
                host_id: claimed_host.into(),
                protocol_version: version.into(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        assert!(AgentControlClient::new(channel.clone())
            .session(ReceiverStream::new(recv))
            .await
            .is_err());
    }
    let (send, recv) = tokio::sync::mpsc::channel(16);
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::Connect(pb::Connect {
            host_id: host.clone(),
            protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let mut old = AgentControlClient::new(channel)
        .session(ReceiverStream::new(recv))
        .await
        .unwrap()
        .into_inner();
    eventually(|| sessions.inspect(&host).is_some()).await;
    let old_id = sessions.inspect(&host).unwrap().session_id;
    assert!(!sessions.inspect(&host).unwrap().reconciled);
    let journal_dir = directory();
    let journal = HostJournal::open(journal_dir.path(), &identity.controller_id(), &host).unwrap();
    let fresh = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executor = Arc::new(JournalExecutor {
        journal: journal.clone(), fresh: fresh.clone(),
        lose_first: std::sync::atomic::AtomicBool::new(true),
    });
    let controller_id = identity.controller_id();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        mllm_agent::session::run_session_with_execution(
            &identity,
            journal,
            pb::ReportInventory {
                domains: vec![pb::DomainObservation {
                    domain_id: "system-memory".into(),
                    kind: "system".into(),
                    observed_bytes: 1024,
                    observed_at_unix: now(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            shutdown,
            Some(executor),
        )
        .await
    });
    eventually(|| {
        sessions
            .inspect(&host)
            .is_some_and(|s| s.reconciled && s.session_id != old_id)
    })
    .await;
    assert!(!sessions.inspect(&host).unwrap().eligible);
    assert_eq!(
        sessions.inspect(&host).unwrap().domains[0].observed_bytes,
        1024
    );
    assert!(tokio::time::timeout(Duration::from_secs(2), old.message())
        .await
        .unwrap()
        .is_err());
    // T09 / T33: first execution commits then drops its ACK and TLS session.
    // Reconnect delivers the exact command and returns its durable result once.
    let mut command = mllm_protocol::execution::MemberCommand {
        identity: mllm_domain::group::CommandIdentity {
            controller_id,
            member: mllm_domain::group::MemberKey { host_id: host.clone(), member_id: "head".into() },
            deployment_id: "deployment".into(), operation_id: "operation".into(),
            command_id: "inspect-once".into(), step_id: "inspect-step".into(),
            generation: 1, revision: 1, deadline_ms: mllm_protocol::now_unix_ms() + 10_000,
            payload_digest: [0; 32], expected_state: "stopped".into(), profile_fingerprint: "local-profile".into(),
            instance_index: 0,
        },
        action: mllm_protocol::execution::MemberAction::Inspect,
    };
    command.identity.payload_digest = command.canonical_digest();
    let result = tokio::time::timeout(Duration::from_secs(12), sessions.execute(command.clone())).await.unwrap().unwrap();
    assert_eq!(result.state, "completed");
    assert!(!result.model_usable);
    assert_eq!(fresh.load(std::sync::atomic::Ordering::SeqCst), 1);
    mllm_protocol::execution::validate_result(&command, &result).unwrap();
    authority.revoke(&host).unwrap();
    eventually(|| !sessions.inspect(&host).unwrap().online).await;
    // ADR 0016 (owner decision 2026-09-24): told its certificate is revoked,
    // the agent stops by itself instead of reconnecting.
    let ended = tokio::time::timeout(Duration::from_secs(15), task).await.unwrap().unwrap();
    assert!(matches!(ended, Err(mllm_agent::session::HostRevoked)), "{ended:?}");
    drop(stop);
    server.abort();
}

// T06 T33 T34: expiry closes an existing TLS stream; bounded queue refuses overflow.
#[tokio::test]
async fn certificate_expiry_closes_existing_stream_and_backpressure_is_bounded() {
    let state_dir = directory();
    let state = Arc::new(Mutex::new(
        OwnedCoordinatorState::open(state_dir.path()).unwrap(),
    ));
    let ca = CertificateAuthority::generate(now() - 365 * 24 * 60 * 60 + 5).unwrap();
    let ca_pem = ca.certificate_pem().to_owned();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_server("localhost", &key, now()).unwrap();
    let authority = Arc::new(EnrollmentAuthority::new(state, ca));
    let sessions = AgentSessions::new(authority.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let server = tokio::spawn(
        Server::builder()
            .tls_config(
                ServerTlsConfig::new()
                    .identity(Identity::from_pem(cert.pem, key.private_key_pem()))
                    .client_ca_root(Certificate::from_pem(&ca_pem)),
            )
            .unwrap()
            .add_service(
                AgentControlServer::from_arc(sessions.clone()).max_decoding_message_size(65536),
            )
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
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
    let channel = identity
        .control_endpoint(now())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let (send, recv) = tokio::sync::mpsc::channel(16);
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::Connect(pb::Connect {
            host_id: host.clone(),
            protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let mut stream = AgentControlClient::new(channel)
        .session(ReceiverStream::new(recv))
        .await
        .unwrap()
        .into_inner();
    let mut command = mllm_protocol::execution::MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(pb::ExecuteMember {
            identity: Some(pb::CommandIdentity {
                controller_id: authority.controller_id(),
                host_id: host.clone(),
                member_id: "rank-0".into(),
                deployment_id: "model".into(),
                operation_id: "operation".into(),
                command_id: "inspect".into(),
                step_id: "inspect".into(),
                generation: 1,
                revision: 1,
                deadline_unix_ms: mllm_protocol::now_unix_ms() + 30000,
                payload_digest: vec![0; 32],
                expected_state: "reserved".into(),
                profile_fingerprint: "pinned".into(),
                protocol_version: "1".into(),
                instance_index: 0,
            }),
            action: Some(pb::execute_member::Action::Inspect(true)),
            restore_checkpoint_digest: String::new(),
            terminate_recorded_processes: Vec::new(),
        })),
    })
    .unwrap();
    command.identity.payload_digest = command.canonical_digest();
    assert!(sessions.dispatch(&host, command.to_wire()).is_err());
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::ReportInventory(pb::ReportInventory {
            envelope: Some(pb::Envelope {
                host_id: host.clone(),
                protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
                ..Default::default()
            }),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::ReconcileHistory(
            pb::ReconcileHistory {
                records: vec![],
                complete: true,
            },
        )),
    })
    .await
    .unwrap();
    assert!(matches!(
        stream.message().await.unwrap().unwrap().msg,
        Some(pb::server_to_agent::Msg::SessionReady(_))
    ));
    let mut expired = command.clone();
    expired.identity.deadline_ms = mllm_protocol::now_unix_ms() - 1;
    expired.identity.payload_digest = expired.canonical_digest();
    assert!(sessions.dispatch(&host, expired.to_wire()).is_err());
    let mut queued = 0;
    while sessions.dispatch(&host, command.to_wire()).is_ok() {
        queued += 1;
        assert!(queued <= 16);
    }
    // Commands leave the session's reply reserve (4 of 16 slots) free.
    assert_eq!(queued, 12);
    // The response consumer can drain; certificate expiry still closes the long-lived stream.
    let expiry = tokio::time::timeout(Duration::from_secs(7), async {
        loop {
            match stream.message().await {
                Ok(Some(_)) => {}
                result => break result,
            }
        }
    })
    .await
    .unwrap();
    assert!(expiry.is_err());
    assert!(!sessions.inspect(&host).unwrap().online);
    server.abort();
}

struct Enrolled {
    sessions: Arc<AgentSessions>,
    identity: PendingEnrollment,
    host: String,
    server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}
async fn enrolled_host() -> Enrolled {
    let state_dir = directory();
    let state = Arc::new(Mutex::new(
        OwnedCoordinatorState::open(state_dir.path()).unwrap(),
    ));
    let ca = CertificateAuthority::generate(now()).unwrap();
    let ca_pem = ca.certificate_pem().to_owned();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_server("localhost", &key, now()).unwrap();
    let authority = Arc::new(EnrollmentAuthority::new(state, ca));
    let sessions = AgentSessions::new(authority.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("https://localhost:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(
        Server::builder()
            .tls_config(
                ServerTlsConfig::new()
                    .identity(Identity::from_pem(cert.pem, key.private_key_pem()))
                    .client_ca_root(Certificate::from_pem(&ca_pem)),
            )
            .unwrap()
            .add_service(
                AgentControlServer::from_arc(sessions.clone()).max_decoding_message_size(65536),
            )
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
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
    Enrolled { sessions, identity, host, server, _dirs: (state_dir, storage_dir) }
}
fn domain(observed_bytes: i64) -> pb::DomainObservation {
    pb::DomainObservation {
        domain_id: "system-memory".into(),
        kind: "system".into(),
        observed_bytes,
        observed_at_unix: now(),
        capacity_bytes: 4096,
        available_bytes: observed_bytes,
        observed_at_unix_ms: mllm_protocol::now_unix_ms(),
        residents: vec![],
    }
}
/// An executor whose single launch-like effect outlives several controller
/// redeliveries, and whose inventory is a fresh measurement.
struct SlowExecutor {
    journal: Arc<HostJournal>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    hold: Duration,
}
impl mllm_agent::session::SessionExecution for SlowExecutor {
    fn execute(&self, session: u64, command: mllm_protocol::execution::MemberCommand) -> mllm_agent::session::ExecutionFuture {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let journal = self.journal.clone();
        let hold = self.hold;
        Box::pin(async move {
            tokio::time::sleep(hold).await;
            tokio::task::spawn_blocking(move || {
                let now = mllm_protocol::now_unix_ms();
                if let mllm_agent::journal::Acceptance::Fresh(ticket) = journal.accept(session, &command, now, &InspectPolicy).map_err(|_| mllm_agent::session::SessionError)? {
                    journal.execute(ticket, now, &InspectPolicy).map_err(|_| mllm_agent::session::SessionError)?;
                }
                journal.execution_result(&command.identity.command_id, mllm_protocol::now_unix_ms()).map_err(|_| mllm_agent::session::SessionError)
            }).await.map_err(|_| mllm_agent::session::SessionError)?
        })
    }
    fn inventory(&self) -> Option<pb::ReportInventory> {
        Some(pb::ReportInventory { domains: vec![domain(2048)], ..Default::default() })
    }
}

// T09 / T33 / T38: at-least-once redelivery of a command whose effect is still
// running (a native launch waiting for readiness) is the same request. It must
// neither start a parallel effect nor exhaust the effect bound and tear the
// session down, which aborted a live SGLang launch mid-readiness (U5, host-a).
// Every connect also reports a fresh measurement, never the startup snapshot:
// publication refuses stale observations, so a stale first report made every
// reconnect after the observation TTL fail (U5, host-a).
#[tokio::test]
async fn redelivery_during_a_running_effect_keeps_the_session_and_reports_fresh_inventory() {
    let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
    let journal_dir = directory();
    let journal = HostJournal::open(journal_dir.path(), &identity.controller_id(), &host).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executor = Arc::new(SlowExecutor {
        journal: journal.clone(),
        calls: calls.clone(),
        // Longer than eight 500 ms redeliveries.
        hold: Duration::from_millis(5_500),
    });
    let controller_id = identity.controller_id();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        mllm_agent::session::run_session_with_execution(
            &identity,
            journal,
            // The startup snapshot, shaped so the controller refuses it the
            // way it refuses a stale observation: a connect that sent it
            // would never reconcile.
            pb::ReportInventory {
                domains: vec![domain(1024)],
                profiles: vec![pb::RuntimeProfileStatus {
                    name: "local".into(),
                    build_fingerprint: "0.5.20".into(),
                    eligibility: "refused-by-controller".into(),
                    reason: String::new(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            shutdown,
            Some(executor),
        )
        .await
    });
    eventually(|| sessions.inspect(&host).is_some_and(|s| s.reconciled)).await;
    let first = sessions.inspect(&host).unwrap();
    assert_eq!(first.domains[0].observed_bytes, 2048);
    let mut command = mllm_protocol::execution::MemberCommand {
        identity: mllm_domain::group::CommandIdentity {
            controller_id,
            member: mllm_domain::group::MemberKey { host_id: host.clone(), member_id: "head".into() },
            deployment_id: "deployment".into(), operation_id: "operation".into(),
            command_id: "slow-inspect".into(), step_id: "slow-step".into(),
            generation: 1, revision: 1, deadline_ms: mllm_protocol::now_unix_ms() + 20_000,
            payload_digest: [0; 32], expected_state: "stopped".into(), profile_fingerprint: "local-profile".into(),
            instance_index: 0,
        },
        action: mllm_protocol::execution::MemberAction::Inspect,
    };
    command.identity.payload_digest = command.canonical_digest();
    let result = tokio::time::timeout(Duration::from_secs(15), sessions.execute(command.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.state, "completed");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let after = sessions.inspect(&host).unwrap();
    assert!(after.online && after.reconciled);
    assert_eq!(after.session_id, first.session_id);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    server.abort();
}

/// A host whose clock leads the controller's by `lead_ms`: its results and its
/// inventory carry host-clock times slightly in the controller's future.
struct LeadingExecutor {
    journal: Arc<HostJournal>,
    lead_ms: i64,
}
impl mllm_agent::session::SessionExecution for LeadingExecutor {
    fn execute(&self, session: u64, command: mllm_protocol::execution::MemberCommand) -> mllm_agent::session::ExecutionFuture {
        let journal = self.journal.clone();
        let lead = self.lead_ms;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let now = mllm_protocol::now_unix_ms() + lead;
                if let mllm_agent::journal::Acceptance::Fresh(ticket) = journal.accept(session, &command, now, &InspectPolicy).map_err(|_| mllm_agent::session::SessionError)? {
                    journal.execute(ticket, now, &InspectPolicy).map_err(|_| mllm_agent::session::SessionError)?;
                }
                journal.execution_result(&command.identity.command_id, mllm_protocol::now_unix_ms() + lead).map_err(|_| mllm_agent::session::SessionError)
            }).await.map_err(|_| mllm_agent::session::SessionError)?
        })
    }
    fn inventory(&self) -> Option<pb::ReportInventory> {
        let mut observation = domain(2048);
        observation.observed_at_unix_ms += self.lead_ms;
        Some(pb::ReportInventory { domains: vec![observation], ..Default::default() })
    }
}

// T33 T29 (U5 recovery live run, host-a): the host clock led the
// controller's by 12 ms. Every host result was refused as a "stale host
// observation" and the session torn down, so no remote launch could ever
// complete. SPEC §7: a lead within the bound is accepted and recorded on the
// controller clock, so no downstream freshness check sees a future time.
#[tokio::test]
async fn a_host_clock_leading_within_the_bound_keeps_the_session_and_its_results() {
    let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
    let journal_dir = directory();
    let journal = HostJournal::open(journal_dir.path(), &identity.controller_id(), &host).unwrap();
    let executor = Arc::new(LeadingExecutor { journal: journal.clone(), lead_ms: 150 });
    let controller_id = identity.controller_id();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        mllm_agent::session::run_session_with_execution(
            &identity,
            journal,
            pb::ReportInventory { domains: vec![domain(1024)], ..Default::default() },
            shutdown,
            Some(executor),
        )
        .await
    });
    eventually(|| sessions.inspect(&host).is_some_and(|s| s.reconciled)).await;
    let first = sessions.inspect(&host).unwrap();
    assert!(first.domains[0].observed_at_unix_ms <= mllm_protocol::now_unix_ms());
    let mut command = mllm_protocol::execution::MemberCommand {
        identity: mllm_domain::group::CommandIdentity {
            controller_id,
            member: mllm_domain::group::MemberKey { host_id: host.clone(), member_id: "head".into() },
            deployment_id: "deployment".into(), operation_id: "operation".into(),
            command_id: "leading-inspect".into(), step_id: "leading-step".into(),
            generation: 1, revision: 1, deadline_ms: mllm_protocol::now_unix_ms() + 10_000,
            payload_digest: [0; 32], expected_state: "stopped".into(), profile_fingerprint: "local-profile".into(),
            instance_index: 0,
        },
        action: mllm_protocol::execution::MemberAction::Inspect,
    };
    command.identity.payload_digest = command.canonical_digest();
    let result = tokio::time::timeout(Duration::from_secs(9), sessions.execute(command))
        .await
        .expect("a result from a slightly leading host clock is accepted")
        .unwrap();
    assert_eq!(result.state, "completed");
    assert!(result.observed_at_unix_ms <= mllm_protocol::now_unix_ms(), "recorded on the controller clock");
    let after = sessions.inspect(&host).unwrap();
    assert_eq!(after.session_id, first.session_id, "the session was never torn down");
    // A lead beyond the bound is still refused.
    let now = mllm_protocol::now_unix_ms();
    assert_eq!(mllm_controller::agent_sessions::controller_time(now + 100, now), Some(now));
    assert_eq!(mllm_controller::agent_sessions::controller_time(now - 100, now), Some(now - 100));
    assert_eq!(mllm_controller::agent_sessions::controller_time(now + 501, now), None);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    server.abort();
}

// G2 T33 T38 T06: over real mTLS, every host session change is published at
// once, and a result names the authenticated session it arrived on. Readiness
// proven on one session must never be credited to the next: here the host's
// first session drops mid-command, and the replayed result names the new one.
#[tokio::test]
async fn session_changes_are_published_and_results_name_their_session() {
    let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
    let mut changes = sessions.subscribe();
    let before = *changes.borrow_and_update();
    assert!(sessions.current_session(&host).is_none());
    let journal_dir = directory();
    let journal = HostJournal::open(journal_dir.path(), &identity.controller_id(), &host).unwrap();
    let fresh = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executor = Arc::new(JournalExecutor {
        journal: journal.clone(),
        fresh: fresh.clone(),
        lose_first: std::sync::atomic::AtomicBool::new(true),
    });
    let controller_id = identity.controller_id();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        mllm_agent::session::run_session_with_execution(
            &identity,
            journal,
            pb::ReportInventory { domains: vec![domain(1024)], ..Default::default() },
            shutdown,
            Some(executor),
        )
        .await
    });
    eventually(|| sessions.current_session(&host).is_some()).await;
    let first = sessions.current_session(&host).unwrap();
    assert!(*changes.borrow_and_update() > before, "connect and reconcile are published");
    let mut command = mllm_protocol::execution::MemberCommand {
        identity: mllm_domain::group::CommandIdentity {
            controller_id,
            member: mllm_domain::group::MemberKey { host_id: host.clone(), member_id: "head".into() },
            deployment_id: "deployment".into(), operation_id: "operation".into(),
            command_id: "inspect-across".into(), step_id: "inspect-across".into(),
            generation: 1, revision: 1, deadline_ms: mllm_protocol::now_unix_ms() + 15_000,
            payload_digest: [0; 32], expected_state: "stopped".into(), profile_fingerprint: "local-profile".into(),
            instance_index: 0,
        },
        action: mllm_protocol::execution::MemberAction::Inspect,
    };
    command.identity.payload_digest = command.canonical_digest();
    let (answered_on, result) = tokio::time::timeout(
        Duration::from_secs(14),
        sessions.execute_on_session(command.clone(), None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.state, "completed");
    // The first session died with the lost acknowledgement; the result came
    // back on the host's next authenticated session, and it says so.
    assert_ne!(answered_on, first);
    assert_eq!(sessions.current_session(&host).as_deref(), Some(answered_on.as_str()));
    assert!(changes.has_changed().unwrap() || *changes.borrow() > before);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    eventually(|| sessions.current_session(&host).is_none()).await;
    server.abort();
}

/// A host that reports engine load for one instance every 250 ms, and can be
/// switched to naming another host in its reports.
struct LoadingExecutor {
    host: String,
    lie: Arc<std::sync::atomic::AtomicBool>,
}
impl mllm_agent::session::SessionExecution for LoadingExecutor {
    fn execute(&self, _: u64, _: mllm_protocol::execution::MemberCommand) -> mllm_agent::session::ExecutionFuture {
        Box::pin(async { Err(mllm_agent::session::SessionError) })
    }
    fn inventory(&self) -> Option<pb::ReportInventory> {
        Some(pb::ReportInventory { domains: vec![domain(2048)], ..Default::default() })
    }
    fn load_interval(&self) -> Duration {
        Duration::from_millis(250)
    }
    fn load_reports(&self) -> Option<mllm_agent::session::LoadFuture> {
        let host = if self.lie.load(std::sync::atomic::Ordering::SeqCst) { "other-host".into() } else { self.host.clone() };
        Some(Box::pin(async move {
            vec![mllm_protocol::reports::LoadReport {
                host_id: host,
                samples: vec![mllm_protocol::reports::LoadSample {
                    deployment_id: "deployment".into(),
                    generation: 2,
                    owned_handle: "launch-2".into(),
                    sampled_at_ms: mllm_protocol::now_unix_ms(),
                    ingress_in_flight: 1,
                    engine: Some(mllm_protocol::reports::EngineLoad { running: 3, waiting: 4, kv_usage_ppm: 500_000 }),
                    latency: None,
                }],
            }
            .to_wire()]
        }))
    }
}

// SPEC §10, ADR 0013 §10, D9 — T18 T34 T38 §17: over real mTLS a reconciled host's
// load reports reach the controller's table keyed by (deployment, generation);
// another generation reads nothing; a report naming another host ends the
// session; a lost session leaves no load behind.
#[tokio::test]
async fn host_load_reports_reach_the_load_table_and_are_fenced() {
    use mllm_controller::load_table::InstanceKey;
    let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
    let table = sessions.load_table();
    let journal_dir = directory();
    let journal = HostJournal::open(journal_dir.path(), &identity.controller_id(), &host).unwrap();
    let lie = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let executor = Arc::new(LoadingExecutor { host: host.clone(), lie: lie.clone() });
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        mllm_agent::session::run_session_with_execution(
            &identity,
            journal,
            pb::ReportInventory { domains: vec![domain(1024)], ..Default::default() },
            shutdown,
            Some(executor),
        )
        .await
    });
    let current = InstanceKey::new("deployment", 2);
    eventually(|| table.fresh(&current, &host).is_some()).await;
    let view = table.fresh(&current, &host).unwrap();
    assert_eq!((view.owned_handle.as_str(), view.ingress_in_flight, view.engine_queue()), ("launch-2", 1, Some(7)));
    assert_eq!(view.engine.unwrap().kv_usage_ppm, 500_000);
    // T34: the previous generation of the same deployment never reads this load.
    assert!(table.fresh(&InstanceKey::new("deployment", 1), &host).is_none());
    // Only the reporting host's view counts.
    assert!(table.fresh(&current, "other-host").is_none());
    let first = sessions.current_session(&host).unwrap();
    // A report naming another host is refused and ends the session.
    lie.store(true, std::sync::atomic::Ordering::SeqCst);
    eventually(|| sessions.current_session(&host).is_none_or(|s| s != first)).await;
    eventually(|| table.sample_at(&current, mllm_protocol::now_unix_ms()).is_none()).await;
    assert!(table.snapshot_at(mllm_protocol::now_unix_ms()).iter().all(|v| v.host_id == host));
    lie.store(false, std::sync::atomic::Ordering::SeqCst);
    eventually(|| table.fresh(&current, &host).is_some()).await;
    // SPEC §13.2: the host goes away; its load goes with it.
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    eventually(|| table.sample_at(&current, mllm_protocol::now_unix_ms()).is_none()).await;
    server.abort();
}

// SPEC §4.3, Phase B follow-up — T17 T18 T38: over real mTLS, a host beginning
// a graceful shutdown announces it; the controller stops counting that session
// as proof of readiness, runs the dispatch suspension, and only then
// acknowledges. The host learns dispatch is suspended before it closes ingress.
#[tokio::test]
async fn a_draining_host_is_suspended_before_it_is_acknowledged() {
    let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
    let suspended = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let suspended = suspended.clone();
        sessions.on_host_draining(Arc::new(move |host: &str| {
            suspended.lock().unwrap().push(host.to_owned());
        }));
    }
    let journal_dir = directory();
    let journal = HostJournal::open(journal_dir.path(), &identity.controller_id(), &host).unwrap();
    let executor = Arc::new(LoadingExecutor {
        host: host.clone(),
        lie: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });
    let drain = mllm_agent::session::DrainSignal::new();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = {
        let drain = drain.clone();
        tokio::spawn(async move {
            mllm_agent::session::run_session_with_drain(
                &identity,
                journal,
                pb::ReportInventory { domains: vec![domain(1024)], ..Default::default() },
                shutdown,
                Some(executor),
                Some(drain),
            )
            .await
        })
    };
    eventually(|| sessions.current_session(&host).is_some()).await;
    let announced = drain.announce(Duration::from_secs(5)).await;
    assert_eq!(announced, mllm_agent::session::DrainAnnouncement::Acknowledged);
    assert_eq!(*suspended.lock().unwrap(), vec![host.clone()], "suspended before the ack");
    // The session stays up (admitted streams still finish through it), but it
    // no longer stands for readiness, so nothing re-opens dispatch.
    assert!(sessions.current_session(&host).is_none());
    assert!(sessions.inspect(&host).unwrap().online);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    server.abort();
}

// SPEC §4.3: with no control session up, the host does not wait on a notice
// nobody can receive; the controller already treats its engines as unproven.
#[tokio::test]
async fn a_drain_with_no_session_does_not_wait() {
    let drain = mllm_agent::session::DrainSignal::new();
    let started = std::time::Instant::now();
    assert_eq!(
        drain.announce(Duration::from_secs(5)).await,
        mllm_agent::session::DrainAnnouncement::NotConnected
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

/// SPEC §13 (WE3 limit 1), over real mTLS with the real host executor: a host
/// whose checkpoint no longer measures to the recorded digest refused the
/// launch by failing its session, and the controller redelivered it until the
/// Initialize deadline. The refusal is now a terminal, typed answer: the
/// provision reports `checkpoint_mismatch` at once, a delivered launch
/// completes refused with no claim and no process, nothing is journaled on the
/// host, and the session that answered stays up.
// T14 T20 T34
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checkpoint_mismatch_is_answered_and_the_session_stays_up() {
    use mllm_agent::{
        checkpoint::CheckpointVerifier, ingress::Ingress, ingress_identity::IngressIdentities,
        native_execution::NativeHostExecution,
    };
    use mllm_config::remote_roles::HostConfig;
    use mllm_controller::agent_sessions::ProvisionOutcome;
    let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
    let root = directory();
    let keys = directory();
    let models = root.path().join("models");
    std::fs::create_dir_all(models.join("toy")).unwrap();
    std::fs::write(models.join("toy/config.json"), "{}").unwrap();
    std::fs::write(models.join("toy/model.safetensors"), "weights").unwrap();
    std::fs::create_dir_all(root.path().join("runtime")).unwrap();
    std::fs::write(root.path().join("runtime/mllm_vllm_guard.py"), "").unwrap();
    std::fs::write(root.path().join("runtime").join(mllm_adapters::vllm::VLLM_ENTRY), "").unwrap();
    // ADR 0008: a sleep-mode vLLM launch also imports the capability probes.
    std::fs::write(root.path().join("runtime/engine_capabilities.py"), "").unwrap();
    // SPEC §9.1 / T21: a prepared host's runtime directory is the agent user's
    // and not group- or other-writable, whatever the umask that wrote it.
    for path in [
        root.path().join("runtime"),
        root.path().join("runtime/mllm_vllm_guard.py"),
        root.path().join("runtime").join(mllm_adapters::vllm::VLLM_ENTRY),
        root.path().join("runtime/engine_capabilities.py"),
    ] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let mut document: serde_json::Value =
        serde_json::from_str(&HostConfig::template(root.path())).unwrap();
    for field in ["hardware_fingerprint", "environment_fingerprint", "resource_policy", "runtime_profiles"] {
        document[field] = fixture["host"][field].clone();
    }
    let config = HostConfig::parse(&document.to_string()).unwrap();
    let policy = mllm_config::remote_resources::policy_fingerprint(&config.document);
    let recorded = CheckpointVerifier::in_memory()
        .measure(&models, &models.join("toy"))
        .unwrap()
        .manifest
        .digest;
    // The checkpoint changes under the digest the controller recorded.
    std::fs::write(models.join("toy/model.safetensors"), "swapped").unwrap();
    let controller_id = identity.controller_id();
    let journal = HostJournal::open(root.path(), &controller_id, &host).unwrap();
    let executor = NativeHostExecution::new(
        journal.clone(),
        Ingress::new().unwrap(),
        IngressIdentities::new(IdentityDirectory::open(keys.path()).unwrap()),
        config,
        host.clone(),
        controller_id.clone(),
        root.path().join("runtime"),
        root.path().join("logs"),
        Default::default(),
    );
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let session_journal = journal.clone();
    let task = tokio::spawn(async move {
        mllm_agent::session::run_session_with_execution(
            &identity,
            session_journal,
            pb::ReportInventory { domains: vec![domain(1024)], ..Default::default() },
            shutdown,
            Some(executor),
        )
        .await
    });
    eventually(|| sessions.current_session(&host).is_some()).await;
    let first = sessions.current_session(&host).unwrap();
    let mut deployment = fixture["deployment"].clone();
    deployment["model"]["path"] = serde_json::json!("toy");
    let mut launch = mllm_protocol::execution::MemberCommand {
        identity: mllm_domain::group::CommandIdentity {
            controller_id,
            member: mllm_domain::group::MemberKey { host_id: host.clone(), member_id: "head".into() },
            deployment_id: "deployment".into(), operation_id: "operation".into(),
            command_id: "launch".into(), step_id: "launch".into(),
            generation: 1, revision: 1, deadline_ms: mllm_protocol::now_unix_ms() + 60_000,
            payload_digest: [0; 32], expected_state: "reserved".into(), profile_fingerprint: "vllm-build-1".into(),
            instance_index: 0,
        },
        action: mllm_protocol::execution::MemberAction::LaunchSingle(mllm_protocol::execution::SingleLaunchPlan {
            deployment_config: deployment.to_string(),
            profile_name: "local".into(),
            checkpoint_fingerprint: "sha256:model".into(),
            host_policy_fingerprint: policy,
            binding_id: "01K00000000000000000000001".into(),
            incarnation: "01K00000000000000000000002".into(),
            grant_id: "01K00000000000000000000003".into(),
            service_port: 8100,
            issued_at_ms: 1,
            coordinator_session_id: "01K00000000000000000000004".into(),
            checkpoint_digest: recorded,
            checkpoint_weights_bytes: None,
            startup_bytes: None,
        }),
    };
    launch.identity.payload_digest = launch.canonical_digest();
    // Answered well inside one redelivery interval pair, not at the deadline.
    let outcome = tokio::time::timeout(Duration::from_secs(5), sessions.provision_ingress(&launch, [7; 32]))
        .await
        .expect("the refusal is answered, not left to the deadline")
        .unwrap();
    assert_eq!(outcome, ProvisionOutcome::Refused("checkpoint_mismatch".into()));
    let (answered_on, result) =
        tokio::time::timeout(Duration::from_secs(5), sessions.execute_on_session(launch.clone(), None))
            .await
            .expect("the refused launch is a terminal result")
            .unwrap();
    assert_eq!(result.refused, "checkpoint_mismatch");
    assert_eq!(result.state, "completed");
    assert!(!result.claim_retained && !result.model_usable && result.processes.is_empty());
    assert_eq!(answered_on, first, "the session that refused is the one still up");
    assert_eq!(sessions.current_session(&host).as_deref(), Some(first.as_str()));
    assert!(journal.history(0, 100).unwrap().is_empty(), "nothing was journaled");
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    server.abort();
}

/// A host that answers every command with one fixed launch observation: the
/// engine was launched and is not usable, and its processes have the given
/// presence.
struct LaunchObservation {
    presence: &'static str,
}
impl mllm_agent::session::SessionExecution for LaunchObservation {
    fn execute(&self, _: u64, command: mllm_protocol::execution::MemberCommand) -> mllm_agent::session::ExecutionFuture {
        let presence = self.presence;
        Box::pin(async move {
            Ok(pb::MemberExecutionResult {
                identity: command.to_wire().identity,
                state: "launched".into(),
                processes: vec![pb::OwnedProcessObservation {
                    role: "api".into(),
                    pid: 4242,
                    boot_id: "boot".into(),
                    start_ticks: 7,
                    presence: presence.into(),
                }],
                observed_at_unix_ms: mllm_protocol::now_unix_ms(),
                claim_retained: true,
                model_usable: false,
                // SPEC §§6.4, 13.2: an exited launch says why, bounded.
                launch_failure: if presence == "gone" { EXITED.into() } else { String::new() },
                ..Default::default()
            })
        })
    }
}

const EXITED: &str = "the engine exited before readiness with exit code 2; it rejected argument --moe-backend";

fn launch_command(controller_id: String, host: &str, deadline_ms: i64) -> mllm_protocol::execution::MemberCommand {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../mllm-config/tests/fixtures/f2-deployment.json")).unwrap();
    let mut launch = mllm_protocol::execution::MemberCommand {
        identity: mllm_domain::group::CommandIdentity {
            controller_id,
            member: mllm_domain::group::MemberKey { host_id: host.into(), member_id: "head".into() },
            deployment_id: "deployment".into(), operation_id: "operation".into(),
            command_id: "launch".into(), step_id: "launch".into(),
            generation: 1, revision: 1, deadline_ms,
            payload_digest: [0; 32], expected_state: "reserved".into(), profile_fingerprint: "sglang-0.5.20".into(),
            instance_index: 0,
        },
        action: mllm_protocol::execution::MemberAction::LaunchSingle(mllm_protocol::execution::SingleLaunchPlan {
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
            checkpoint_digest: String::new(),
            checkpoint_weights_bytes: None,
            startup_bytes: None,
        }),
    };
    launch.identity.payload_digest = launch.canonical_digest();
    launch
}

/// M16 live run 2026-09-23 (host-a, s92-27): the SGLang entry refused its
/// server arguments and exited within seconds. The host reported the launch
/// with every process gone, but the controller waited for a usable or released
/// result until the 15-minute Initialize deadline, holding the single
/// coordinator loop for every host. SPEC §13.1: that result is terminal
/// evidence. A launch whose process is still alive stays in progress.
// T09 T20 T33
#[tokio::test]
async fn an_engine_gone_before_readiness_is_a_terminal_launch_result() {
    for (presence, terminal) in [("gone", true), ("alive", false), ("unknown", false)] {
        let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
        let journal_dir = directory();
        let journal = HostJournal::open(journal_dir.path(), &identity.controller_id(), &host).unwrap();
        let controller_id = identity.controller_id();
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            mllm_agent::session::run_session_with_execution(
                &identity,
                journal,
                pb::ReportInventory { domains: vec![domain(1024)], ..Default::default() },
                shutdown,
                Some(Arc::new(LaunchObservation { presence })),
            )
            .await
        });
        eventually(|| sessions.inspect(&host).is_some_and(|s| s.reconciled)).await;
        let launch = launch_command(controller_id, &host, mllm_protocol::now_unix_ms() + 60_000);
        let outcome = tokio::time::timeout(Duration::from_secs(3), sessions.execute(launch)).await;
        if terminal {
            let result = outcome.expect("an engine gone before readiness is answered at once").unwrap();
            assert_eq!(result.state, "launched");
            assert!(!result.model_usable && result.claim_retained, "the claim stays until absence is settled");
            assert!(result.processes.iter().all(|p| p.presence == "gone"));
            // T20: the host's bounded reason survives the session.
            assert_eq!(result.launch_failure, EXITED);
        } else {
            assert!(outcome.is_err(), "a launch with a {presence} process is still in progress");
        }
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
        server.abort();
    }
}

/// SPEC §17 (M80): a host whose load reports come from a real host ingress and
/// load reporter, in front of a fake engine.
struct TimedExecutor {
    reporter: Arc<mllm_agent::load::LoadReporter>,
}
impl mllm_agent::session::SessionExecution for TimedExecutor {
    fn execute(&self, _: u64, _: mllm_protocol::execution::MemberCommand) -> mllm_agent::session::ExecutionFuture {
        Box::pin(async { Err(mllm_agent::session::SessionError) })
    }
    fn inventory(&self) -> Option<pb::ReportInventory> {
        Some(pb::ReportInventory { domains: vec![domain(2048)], ..Default::default() })
    }
    fn load_interval(&self) -> Duration {
        Duration::from_millis(250)
    }
    fn load_reports(&self) -> Option<mllm_agent::session::LoadFuture> {
        let reporter = self.reporter.clone();
        Some(Box::pin(async move { reporter.reports().await }))
    }
}

/// vLLM 0.29.0 exposition shape for the forwarded histograms (two label sets
/// summed per `le`). Fake engine; not qualification.
const TIMED_VLLM_METRICS: &str = "\
vllm:num_requests_running{engine=\"0\",model_name=\"model\"} 0.0
vllm:num_requests_waiting{engine=\"0\",model_name=\"model\"} 0.0
vllm:kv_cache_usage_perc{engine=\"0\",model_name=\"model\"} 0.0
vllm:time_to_first_token_seconds_bucket{engine=\"0\",le=\"0.02\",model_name=\"model\"} 1.0
vllm:time_to_first_token_seconds_bucket{engine=\"0\",le=\"0.04\",model_name=\"model\"} 2.0
vllm:time_to_first_token_seconds_bucket{engine=\"0\",le=\"+Inf\",model_name=\"model\"} 3.0
vllm:time_to_first_token_seconds_count{engine=\"0\",model_name=\"model\"} 3.0
vllm:time_to_first_token_seconds_sum{engine=\"0\",model_name=\"model\"} 0.2
";

// SPEC §17 T18 T38 (M80): a request through the real host ingress is timed on
// the ingress clock (headers, first and last engine byte); the engine's own
// histogram is forwarded as `source: engine`; both reach the controller's
// latency table over the real mTLS control session with the W8 load report,
// keyed by the instance incarnation.
#[tokio::test]
async fn ingress_and_engine_latency_ride_the_load_report() {
    use axum::{body::Body, routing::{get, post}};
    let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
    let latency = sessions.latency_table();
    // A fake engine: chat streams two chunks 60 ms apart after 40 ms.
    let engine = axum::Router::new()
        .route("/metrics", get(|| async { TIMED_VLLM_METRICS }))
        .route(
            "/v1/chat/completions",
            post(|| async {
                let chunks = futures::stream::unfold(0u8, |step| async move {
                    match step {
                        0 => {
                            tokio::time::sleep(Duration::from_millis(40)).await;
                            Some((Ok::<_, std::io::Error>("data: {\"choices\":[]}\n\n"), 1))
                        }
                        1 => {
                            tokio::time::sleep(Duration::from_millis(60)).await;
                            Some((Ok("data: [DONE]\n\n"), 2))
                        }
                        _ => None,
                    }
                });
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(chunks))
                    .unwrap()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, engine).await.unwrap() });
    let ingress = mllm_agent::ingress::Ingress::new().unwrap();
    let scope = mllm_agent::ingress::IngressScope {
        host_id: host.clone(),
        deployment_id: "deployment".into(),
        binding_id: "binding".into(),
        incarnation: "incarnation".into(),
        member_id: "head".into(),
        generation: 5,
        revision: 1,
        instance_index: 0,
    };
    ingress.register(scope.clone(), target, "model".into(), [7; 32], [8; 32]).unwrap();
    ingress.bind_handle(&scope, "launch-5").unwrap();
    ingress.open(&scope).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ingress_address = listener.local_addr().unwrap();
    {
        let router = ingress.clone().router();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    }
    let response = reqwest::Client::new()
        .post(format!("http://{ingress_address}/v1/chat/completions"))
        .bearer_auth(hex::encode([7u8; 32]))
        .header("content-type", "application/json")
        .body(r#"{"model":"model","stream":true,"messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    response.bytes().await.unwrap();
    let reporter = Arc::new(mllm_agent::load::LoadReporter::new(ingress.clone(), host.clone()).unwrap());
    let journal_dir = directory();
    let journal = HostJournal::open(journal_dir.path(), &identity.controller_id(), &host).unwrap();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        mllm_agent::session::run_session_with_execution(
            &identity,
            journal,
            pb::ReportInventory { domains: vec![domain(1024)], ..Default::default() },
            shutdown,
            Some(Arc::new(TimedExecutor { reporter })),
        )
        .await
    });
    let series = |name: &str| {
        latency
            .snapshot(Some("deployment"))
            .into_iter()
            .find(|v| v.series == name)
    };
    eventually(|| series("ingress_time_to_last_byte").is_some() && series("engine_time_to_first_token").is_some()).await;
    // Later ticks carry no new observations: counts do not grow.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let first = series("ingress_time_to_first_byte").unwrap();
    let last = series("ingress_time_to_last_byte").unwrap();
    assert_eq!((first.generation, first.host_id.as_str(), first.source), (5, host.as_str(), "mllm"));
    assert_eq!((first.histogram.count(), last.histogram.count()), (1, 1));
    assert!(first.histogram.sum() >= 0.04, "first engine byte after 40 ms");
    assert!(last.histogram.sum() >= 0.1, "last engine byte after 100 ms");
    assert!(series("ingress_time_to_headers").unwrap().histogram.sum() <= first.histogram.sum());
    let engine = series("engine_time_to_first_token").unwrap();
    assert_eq!((engine.source, engine.engine.as_deref(), engine.histogram.count()), ("engine", Some("vllm"), 3));
    assert_eq!(engine.histogram.bounds(), &[0.02, 0.04]);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    // Distributions outlive the session; only load is forgotten with it.
    assert!(series("engine_time_to_first_token").is_some());
    server.abort();
}

/// A reconciled control session driven by hand, standing in for a host agent
/// that reads and writes exactly what a test tells it to.
struct RawSession {
    send: tokio::sync::mpsc::Sender<pb::AgentToServer>,
    stream: tonic::Streaming<pb::ServerToAgent>,
    session_id: String,
}
async fn raw_session(identity: &PendingEnrollment, host: &str) -> RawSession {
    let channel = identity.control_endpoint(now()).unwrap().connect().await.unwrap();
    let (send, recv) = tokio::sync::mpsc::channel(64);
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::Connect(pb::Connect {
            host_id: host.into(),
            protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let mut stream = AgentControlClient::new(channel)
        .session(ReceiverStream::new(recv))
        .await
        .unwrap()
        .into_inner();
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::ReportInventory(pb::ReportInventory {
            envelope: Some(pb::Envelope {
                host_id: host.into(),
                protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
                ..Default::default()
            }),
            ..Default::default()
        })),
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
    let session_id = loop {
        match stream.message().await.unwrap().unwrap().msg {
            Some(pb::server_to_agent::Msg::SessionReady(ready)) => break ready.session_id,
            _ => continue,
        }
    };
    RawSession { send, stream, session_id }
}
fn inspect_command(controller_id: String, host: &str, id: &str, deadline_ms: i64) -> mllm_protocol::execution::MemberCommand {
    let mut command = mllm_protocol::execution::MemberCommand {
        identity: mllm_domain::group::CommandIdentity {
            controller_id,
            member: mllm_domain::group::MemberKey { host_id: host.into(), member_id: "head".into() },
            deployment_id: "deployment".into(), operation_id: "operation".into(),
            command_id: id.into(), step_id: id.into(),
            generation: 1, revision: 1, deadline_ms,
            payload_digest: [0; 32], expected_state: "stopped".into(), profile_fingerprint: "local-profile".into(),
            instance_index: 0,
        },
        action: mllm_protocol::execution::MemberAction::Inspect,
    };
    command.identity.payload_digest = command.canonical_digest();
    command
}
fn completed(command: &mllm_protocol::execution::MemberCommand, observed_at_unix_ms: i64) -> pb::AgentToServer {
    pb::AgentToServer {
        msg: Some(agent_to_server::Msg::MemberResult(pb::MemberExecutionResult {
            identity: command.to_wire().identity,
            state: "completed".into(),
            observed_at_unix_ms,
            ..Default::default()
        })),
    }
}
/// The next command the controller sent on a raw session.
async fn next_command(stream: &mut tonic::Streaming<pb::ServerToAgent>) -> pb::ExecuteMember {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), stream.message()).await.unwrap().unwrap().unwrap().msg {
            Some(pb::server_to_agent::Msg::ExecuteMember(command)) => return command,
            _ => continue,
        }
    }
}

// T33 T38, SPEC §13: a result whose observation is too old to be evidence is
// not evidence, but it is not a protocol violation either. It is ignored and
// the session stays up; the next fresh result for the same command is taken.
#[tokio::test]
async fn a_stale_result_is_ignored_and_the_session_stays_up() {
    let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
    let mut raw = raw_session(&identity, &host).await;
    let command = inspect_command(identity.controller_id(), &host, "stale-then-fresh", mllm_protocol::now_unix_ms() + 10_000);
    let execution = {
        let (sessions, command) = (sessions.clone(), command.clone());
        tokio::spawn(async move { sessions.execute(command).await })
    };
    next_command(&mut raw.stream).await;
    raw.send.send(completed(&command, mllm_protocol::now_unix_ms() - 5_000)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(sessions.current_session(&host).as_deref(), Some(raw.session_id.as_str()), "a stale result never ends the session");
    assert!(!execution.is_finished(), "a stale result is not evidence");
    raw.send.send(completed(&command, mllm_protocol::now_unix_ms())).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), execution).await.unwrap().unwrap().unwrap();
    assert_eq!(result.state, "completed");
    assert_eq!(sessions.current_session(&host).as_deref(), Some(raw.session_id.as_str()));
    server.abort();
}

/// Retains remote evidence slowly, as a busy store would.
struct SlowRetain {
    entered: Arc<std::sync::atomic::AtomicBool>,
    release: Arc<std::sync::atomic::AtomicBool>,
}
impl mllm_controller::agent_sessions::RemoteEvidenceObserver for SlowRetain {
    fn retain(&self, _: &mllm_protocol::execution::MemberCommand, _: &pb::MemberExecutionResult) -> Result<(), Box<tonic::Status>> {
        self.entered.store(true, std::sync::atomic::Ordering::SeqCst);
        let started = std::time::Instant::now();
        while !self.release.load(std::sync::atomic::Ordering::SeqCst) && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
}

// T33 T38, SPEC §13: store work done for one host's message never holds the
// session table. Readiness supervisors and placement read it for every host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_work_for_a_result_does_not_hold_the_session_table() {
    let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
    let mut raw = raw_session(&identity, &host).await;
    let command = inspect_command(identity.controller_id(), &host, "slow-retain", mllm_protocol::now_unix_ms() + 15_000);
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observer = Arc::new(SlowRetain { entered: entered.clone(), release: release.clone() });
    let execution = {
        let (sessions, command) = (sessions.clone(), command.clone());
        tokio::spawn(async move { sessions.execute_observed(command, Some(observer)).await })
    };
    next_command(&mut raw.stream).await;
    raw.send.send(completed(&command, mllm_protocol::now_unix_ms())).await.unwrap();
    // The runtime's timer may be driven by the very worker the retain blocks,
    // so wait on the plain clock.
    let waited = std::time::Instant::now();
    while !entered.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(waited.elapsed() < Duration::from_secs(5), "the result reached the observer");
        std::thread::sleep(Duration::from_millis(5));
    }
    // A plain thread and channel: the bound must not depend on the runtime.
    let (answer, answered) = std::sync::mpsc::channel();
    {
        let sessions = sessions.clone();
        let host = host.clone();
        std::thread::spawn(move || answer.send(sessions.current_session(&host)));
    }
    let current = answered.recv_timeout(Duration::from_secs(1));
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        current.expect("the session table stays readable during store work").as_deref(),
        Some(raw.session_id.as_str())
    );
    let result = tokio::time::timeout(Duration::from_secs(5), execution).await.unwrap().unwrap().unwrap();
    assert_eq!(result.state, "completed");
    server.abort();
}

// T17 T38, SPEC §4.3: a host whose command queue is full still gets its drain
// acknowledged. Commands never take the capacity the session's own replies
// need, so a busy queue cannot end the session the host is draining through.
#[tokio::test]
async fn a_full_command_queue_never_ends_a_draining_session() {
    let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
    let mut raw = raw_session(&identity, &host).await;
    let command = inspect_command(identity.controller_id(), &host, "fill", mllm_protocol::now_unix_ms() + 30_000);
    // The host reads nothing, so once the transport's window is full the
    // queue stays full while commands keep arriving.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (full, filled) = tokio::sync::oneshot::channel();
    let hammer = {
        let (sessions, host, wire, stop) = (sessions.clone(), host.clone(), command.to_wire(), stop.clone());
        std::thread::spawn(move || {
            let mut full = Some(full);
            let mut refused_since = None;
            let started = std::time::Instant::now();
            while !stop.load(std::sync::atomic::Ordering::SeqCst) && started.elapsed() < Duration::from_secs(30) {
                if sessions.dispatch(&host, wire.clone()).is_ok() {
                    refused_since = None;
                    continue;
                }
                let since = *refused_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() > Duration::from_millis(300) {
                    if let Some(full) = full.take() {
                        let _ = full.send(());
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    tokio::time::timeout(Duration::from_secs(30), filled).await.expect("the command queue stays full").unwrap();
    raw.send
        .send(pb::AgentToServer {
            msg: Some(agent_to_server::Msg::HostDraining(pb::HostDraining { host_id: host.clone() })),
        })
        .await
        .unwrap();
    let acknowledged = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match raw.stream.message().await {
                Ok(Some(pb::ServerToAgent { msg: Some(pb::server_to_agent::Msg::HostDrainAcknowledged(_)) })) => return true,
                Ok(Some(_)) => continue,
                _ => return false,
            }
        }
    })
    .await
    .unwrap();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    hammer.join().unwrap();
    assert!(acknowledged, "the drain is acknowledged, not the session ended");
    let view = sessions.inspect(&host).unwrap();
    assert!(view.online);
    assert_eq!(view.session_id, raw.session_id);
    server.abort();
}

// T09 T33, SPEC §13: a command already delivered on a live session is not
// resent twice a second while its effect runs; redelivery backs off on the
// same session and happens at once on a new one.
#[tokio::test]
async fn a_delivered_command_is_redelivered_with_backoff_not_every_tick() {
    let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
    let mut raw = raw_session(&identity, &host).await;
    let command = inspect_command(identity.controller_id(), &host, "unanswered", mllm_protocol::now_unix_ms() + 2_200);
    let execution = {
        let (sessions, command) = (sessions.clone(), command.clone());
        tokio::spawn(async move { sessions.execute(command).await })
    };
    let mut delivered = 0;
    let window = tokio::time::Instant::now() + Duration::from_millis(2_100);
    while let Ok(Ok(Some(message))) = tokio::time::timeout_at(window, raw.stream.message()).await {
        if matches!(message.msg, Some(pb::server_to_agent::Msg::ExecuteMember(_))) {
            delivered += 1;
        }
    }
    assert!((1..=2).contains(&delivered), "delivered {delivered} times in 2.1 s");
    assert!(tokio::time::timeout(Duration::from_secs(3), execution).await.unwrap().unwrap().is_err());
    assert_eq!(sessions.current_session(&host).as_deref(), Some(raw.session_id.as_str()));
    server.abort();
}

// T18 T34, SPEC §10, §13.2: a host session replaced by a reconnect leaves no
// load behind. The new session has proven nothing yet; its host reports again.
#[tokio::test]
async fn a_replaced_session_leaves_no_load_behind() {
    use mllm_controller::load_table::InstanceKey;
    let Enrolled { sessions, identity, host, server, _dirs } = enrolled_host().await;
    let table = sessions.load_table();
    let first = raw_session(&identity, &host).await;
    first
        .send
        .send(pb::AgentToServer {
            msg: Some(agent_to_server::Msg::ReportLoad(
                mllm_protocol::reports::LoadReport {
                    host_id: host.clone(),
                    samples: vec![mllm_protocol::reports::LoadSample {
                        deployment_id: "deployment".into(),
                        generation: 2,
                        owned_handle: "launch-2".into(),
                        sampled_at_ms: mllm_protocol::now_unix_ms(),
                        ingress_in_flight: 1,
                        engine: None,
                        latency: None,
                    }],
                }
                .to_wire(),
            )),
        })
        .await
        .unwrap();
    let current = InstanceKey::new("deployment", 2);
    eventually(|| table.fresh(&current, &host).is_some()).await;
    let second = raw_session(&identity, &host).await;
    assert_ne!(second.session_id, first.session_id);
    assert!(table.fresh(&current, &host).is_none(), "the replaced session's load is gone");
    drop(first);
    server.abort();
}

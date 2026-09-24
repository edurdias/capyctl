use mllm_agent::{
    enrollment::{JoinInvitation, PendingEnrollment},
    identity::{CertificateAuthority, HostKey},
    identity_storage::IdentityDirectory,
};
use mllm_controller::{enrollment::EnrollmentAuthority, OwnedCoordinatorState};
use mllm_protocol::pb::{
    bootstrap_client::BootstrapClient, bootstrap_server::BootstrapServer,
    host_identity_client::HostIdentityClient, host_identity_server::HostIdentityServer,
    EnrollRequest, RenewHostRequest, VerifyHostRequest,
};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig};
struct CountingBootstrap {
    authority: Arc<EnrollmentAuthority>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
#[tonic::async_trait]
impl mllm_protocol::pb::bootstrap_server::Bootstrap for CountingBootstrap {
    async fn enroll(
        &self,
        request: tonic::Request<EnrollRequest>,
    ) -> Result<tonic::Response<mllm_protocol::pb::EnrollResponse>, tonic::Status> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        mllm_protocol::pb::bootstrap_server::Bootstrap::enroll(self.authority.as_ref(), request)
            .await
    }
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
// T05 T06 T37: actual TLS bootstrap, registry-bound mTLS and live revocation.
#[tokio::test]
async fn trusted_bootstrap_recovery_renewal_and_revocation() {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let owned = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let ca = CertificateAuthority::generate(now()).unwrap();
    let ca_pem = ca.certificate_pem().to_owned();
    let server_key = HostKey::generate().unwrap();
    let server_cert = ca.issue_server("localhost", &server_key, now()).unwrap();
    let unregistered_key = HostKey::generate().unwrap();
    let unregistered = ca
        .issue_host(
            "unregistered-host",
            &unregistered_key.enrollment_request().unwrap(),
            now(),
        )
        .unwrap();
    let authority = Arc::new(EnrollmentAuthority::new(owned, ca));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let invitation = authority.invite("host-a", 300, now()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bootstrap = tokio::spawn(
        Server::builder()
            .tls_config(ServerTlsConfig::new().identity(Identity::from_pem(
                &server_cert.pem,
                server_key.private_key_pem(),
            )))
            .unwrap()
            .add_service(BootstrapServer::new(CountingBootstrap {
                authority: authority.clone(),
                calls: calls.clone(),
            }))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    let wrong_ca = CertificateAuthority::generate(now()).unwrap();
    let bad_dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(bad_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let bad_storage = IdentityDirectory::open(bad_dir.path()).unwrap();
    let bad_invitation = JoinInvitation {
        version: 1,
        server_address: format!("https://localhost:{}", addr.port()),
        control_address: format!("https://localhost:{}", addr.port()),
        server_ca: wrong_ca.certificate_pem().into(),
        invitation_id: invitation.id.clone(),
        invitation_secret: invitation.secret.clone(),
        host_name: invitation.host_name.clone(),
        expires_unix: invitation.expires_unix,
    };
    let mut untrusted = PendingEnrollment::prepare(&bad_storage, &bad_invitation).unwrap();
    assert!(
        untrusted
            .enroll(&bad_storage, &bad_invitation, now())
            .await
            .is_err(),
        "TLS must reject before sending the secret-bearing RPC"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "untrusted server must receive no enrollment RPC"
    );
    let channel = Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(
            ClientTlsConfig::new()
                .domain_name("localhost")
                .ca_certificate(Certificate::from_pem(&ca_pem)),
        )
        .unwrap()
        .connect()
        .await
        .unwrap();
    let key = HostKey::generate().unwrap();
    let csr = key.enrollment_request().unwrap();
    let request = EnrollRequest {
        invitation_id: invitation.id.clone(),
        invitation_secret: invitation.secret.clone(),
        host_name: "host-a".into(),
        transaction_id: "transaction-one".into(),
        csr_der: csr.clone(),
        protocol_version: "1".into(),
        ..Default::default()
    };
    let mut client = BootstrapClient::new(channel);
    let first = client.enroll(request.clone()).await.unwrap().into_inner();
    let replay = client.enroll(request).await.unwrap().into_inner();
    assert_eq!(first, replay);
    let host_dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(host_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let storage = IdentityDirectory::open(host_dir.path()).unwrap();
    let second_invitation = authority.invite("host-b", 300, now()).unwrap();
    let join = JoinInvitation {
        version: 1,
        server_address: format!("https://localhost:{}", addr.port()),
        control_address: format!("https://localhost:{}", addr.port()),
        server_ca: ca_pem.clone(),
        invitation_id: second_invitation.id,
        invitation_secret: second_invitation.secret,
        host_name: second_invitation.host_name,
        expires_unix: second_invitation.expires_unix,
    };
    let mut pending = PendingEnrollment::prepare(&storage, &join).unwrap();
    let second_host = pending.enroll(&storage, &join, now()).await.unwrap();
    drop(pending);
    drop(storage);
    let storage = IdentityDirectory::open(host_dir.path()).unwrap();
    let mut pending = PendingEnrollment::load(&storage).unwrap();
    assert_eq!(pending.host_id(), Some(second_host.as_str()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let enrolled = tokio::spawn(
        Server::builder()
            .tls_config(
                ServerTlsConfig::new()
                    .identity(Identity::from_pem(
                        &server_cert.pem,
                        server_key.private_key_pem(),
                    ))
                    .client_ca_root(Certificate::from_pem(&ca_pem)),
            )
            .unwrap()
            .add_service(HostIdentityServer::from_arc(authority.clone()))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    let channel = Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(
            ClientTlsConfig::new()
                .domain_name("localhost")
                .ca_certificate(Certificate::from_pem(&ca_pem))
                .identity(Identity::from_pem(
                    &first.client_certificate,
                    key.private_key_pem(),
                )),
        )
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = HostIdentityClient::new(channel);
    pending
        .renew(
            &storage,
            &format!("https://localhost:{}", addr.port()),
            now(),
        )
        .await
        .unwrap();
    assert_eq!(
        PendingEnrollment::load(&storage).unwrap().host_id(),
        Some(second_host.as_str())
    );

    let unregistered_channel = Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(
            ClientTlsConfig::new()
                .domain_name("localhost")
                .ca_certificate(Certificate::from_pem(&ca_pem))
                .identity(Identity::from_pem(
                    &unregistered.pem,
                    unregistered_key.private_key_pem(),
                )),
        )
        .unwrap()
        .connect()
        .await
        .unwrap();
    assert!(HostIdentityClient::new(unregistered_channel)
        .verify(VerifyHostRequest {
            host_id: "unregistered-host".into()
        })
        .await
        .is_err());
    assert!(client
        .verify(VerifyHostRequest {
            host_id: "someone-else".into()
        })
        .await
        .is_err());
    assert_eq!(
        client
            .verify(VerifyHostRequest {
                host_id: first.host_id.clone()
            })
            .await
            .unwrap()
            .into_inner()
            .host_id,
        first.host_id
    );
    let renewed = client
        .renew(RenewHostRequest {
            csr_der: csr,
            protocol_version: "1".into(),
            transaction_id: "renewal-one".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(renewed.host_id, first.host_id);
    let mut revocations = authority.revocations();
    authority.revoke(&first.host_id).unwrap();
    revocations.changed().await.unwrap();
    assert!(client
        .verify(VerifyHostRequest {
            host_id: first.host_id
        })
        .await
        .is_err());
    bootstrap.abort();
    enrolled.abort();
}

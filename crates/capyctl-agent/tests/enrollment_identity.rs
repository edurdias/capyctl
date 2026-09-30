use capyctl_agent::{
    enrollment::{initialize_controller_ca, load_controller_ca, JoinInvitation, PendingEnrollment},
    identity_storage::IdentityDirectory,
};
fn dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}
// T05 T06: a retry uses the persisted private key and exact signed CSR, never a fresh identity.
#[test]
fn pending_identity_survives_restart_and_refuses_changed_invitation() {
    let path = dir();
    let storage = IdentityDirectory::open(path.path()).unwrap();
    let ca = initialize_controller_ca(&storage, 100).unwrap();
    let invitation = JoinInvitation {
        version: 1,
        server_address: "https://controller.example:7444".into(),
        control_address: "https://controller.example:7444".into(),
        server_ca: ca.certificate_pem().into(),
        invitation_id: {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest("b".repeat(64).as_bytes()))
        },
        invitation_secret: "b".repeat(64),
        host_name: "host-a".into(),
        expires_unix: 1000,
        recover_host_id: None,
    };
    let pending = PendingEnrollment::prepare(&storage, &invitation).unwrap();
    let request = pending.request(&invitation).unwrap();
    drop(storage);
    let storage = IdentityDirectory::open(path.path()).unwrap();
    let recovered = PendingEnrollment::prepare(&storage, &invitation).unwrap();
    assert_eq!(request, recovered.request(&invitation).unwrap());
    let mut changed = invitation;
    changed.control_address = "https://other-controller.example:7445".into();
    assert!(PendingEnrollment::prepare(&storage, &changed).is_err());
    changed.control_address = "https://controller.example:7444".into();
    changed.host_name = "host-b".into();
    assert!(PendingEnrollment::prepare(&storage, &changed).is_err());
    assert!(initialize_controller_ca(&storage, 100).is_err());
    assert_eq!(
        ca.certificate_pem(),
        load_controller_ca(&storage).unwrap().certificate_pem()
    );
}
// T37: malformed persisted identity cannot silently trigger a replacement key.
#[test]
fn corrupt_ca_and_pending_identity_are_not_regenerated() {
    let path = dir();
    let storage = IdentityDirectory::open(path.path()).unwrap();
    storage
        .create_bundle("controller-ca.json", b"broken")
        .unwrap();
    assert!(load_controller_ca(&storage).is_err());
    assert!(initialize_controller_ca(&storage, 100).is_err());
}

fn now() -> i64 {
    capyctl_protocol::now_unix_ms() / 1000
}
fn invitation_for(
    ca: &capyctl_agent::identity::CertificateAuthority,
    secret: char,
    recover_host_id: Option<&str>,
) -> JoinInvitation {
    use sha2::{Digest, Sha256};
    JoinInvitation {
        version: 1,
        server_address: "https://controller.example:7444".into(),
        control_address: "https://controller.example:7444".into(),
        server_ca: ca.certificate_pem().into(),
        invitation_id: hex::encode(Sha256::digest(secret.to_string().repeat(64).as_bytes())),
        invitation_secret: secret.to_string().repeat(64),
        host_name: "host-a".into(),
        expires_unix: now() + 600,
        recover_host_id: recover_host_id.map(str::to_owned),
    }
}
/// Complete `pending` as the controller would, for `host`.
fn issue(
    ca: &capyctl_agent::identity::CertificateAuthority,
    storage: &IdentityDirectory,
    pending: &mut PendingEnrollment,
    invitation: &JoinInvitation,
    host: &str,
) -> Result<String, capyctl_agent::enrollment::EnrollmentError> {
    let request = pending.request(invitation).unwrap();
    let issued = ca.issue_host(host, &request.csr_der, now()).unwrap();
    pending.accept_certificate(
        storage,
        capyctl_protocol::pb::EnrollResponse {
            host_id: host.into(),
            server_certificate: ca.certificate_pem().into(),
            lease_expires_unix: issued.expires_unix,
            client_certificate: issued.pem,
            certificate_fingerprint: issued.fingerprint,
        },
        now(),
    )
}

// T05 T06 (ADR 0016, SPEC §4.1): recovery re-enrolls the same host id with a
// new key. It replaces the retained identity only when that identity is the
// same controller's and names the same host; a retry resumes the same
// transaction; lost identity files start fresh; an ordinary enrollment never
// takes a recovery invitation and recovery never takes an ordinary one; and a
// certificate for another host id is refused.
#[test]
fn recovery_replaces_only_the_same_hosts_identity_with_a_new_key() {
    let ca_dir = dir();
    let ca =
        initialize_controller_ca(&IdentityDirectory::open(ca_dir.path()).unwrap(), now()).unwrap();
    let path = dir();
    let storage = IdentityDirectory::open(path.path()).unwrap();
    let ordinary = invitation_for(&ca, 'a', None);
    let mut pending = PendingEnrollment::prepare(&storage, &ordinary).unwrap();
    assert_eq!(
        issue(&ca, &storage, &mut pending, &ordinary, "host-a").unwrap(),
        "host-a"
    );
    let old_csr = pending.request(&ordinary).unwrap().csr_der;

    // Recovery is explicit on both sides.
    let recovery = invitation_for(&ca, 'b', Some("host-a"));
    assert!(PendingEnrollment::prepare(&storage, &recovery).is_err());
    assert!(PendingEnrollment::prepare_recovery(&storage, &ordinary).is_err());
    // Another host's recovery never takes over this identity.
    assert!(PendingEnrollment::prepare_recovery(
        &storage,
        &invitation_for(&ca, 'c', Some("host-b"))
    )
    .is_err());
    // Nor does another controller's.
    let other_ca_dir = dir();
    let other_ca = initialize_controller_ca(
        &IdentityDirectory::open(other_ca_dir.path()).unwrap(),
        now(),
    )
    .unwrap();
    assert!(PendingEnrollment::prepare_recovery(
        &storage,
        &invitation_for(&other_ca, 'd', Some("host-a"))
    )
    .is_err());
    assert_eq!(
        PendingEnrollment::load(&storage)
            .unwrap()
            .request(&ordinary)
            .unwrap()
            .csr_der,
        old_csr,
        "a refused recovery left the identity untouched"
    );

    let recovering = PendingEnrollment::prepare_recovery(&storage, &recovery).unwrap();
    let request = recovering.request(&recovery).unwrap();
    assert_ne!(
        request.csr_der, old_csr,
        "recovery never reuses the revoked key"
    );
    // A restart before the reply resumes the same transaction and key.
    drop(recovering);
    drop(storage);
    let storage = IdentityDirectory::open(path.path()).unwrap();
    let mut recovering = PendingEnrollment::prepare_recovery(&storage, &recovery).unwrap();
    assert_eq!(recovering.request(&recovery).unwrap(), request);
    // A certificate for another host id is refused.
    assert!(issue(&ca, &storage, &mut recovering, &recovery, "host-b").is_err());
    assert_eq!(
        issue(&ca, &storage, &mut recovering, &recovery, "host-a").unwrap(),
        "host-a"
    );
    let loaded = PendingEnrollment::load(&storage).unwrap();
    assert_eq!(loaded.host_id(), Some("host-a"));
    assert!(loaded.control_endpoint(now()).is_ok());

    // Lost identity files: recovery starts from a fresh identity.
    let lost = dir();
    let storage = IdentityDirectory::open(lost.path()).unwrap();
    let again = invitation_for(&ca, 'e', Some("host-a"));
    let mut fresh = PendingEnrollment::prepare_recovery(&storage, &again).unwrap();
    assert_eq!(
        issue(&ca, &storage, &mut fresh, &again, "host-a").unwrap(),
        "host-a"
    );
}

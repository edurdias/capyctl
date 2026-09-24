use mllm_agent::{
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

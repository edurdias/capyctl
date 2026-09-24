use mllm_store::{
    enrollment::{CertificateRecord, Redemption},
    host_publication::HostPublication,
    Store,
};
// T06, T07, T33: only an enrolled non-revoked identity can publish its own config.
#[test]
fn host_publication_survives_reopen_and_revocation_blocks_updates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite3");
    let store = Store::open(&path).unwrap();
    store
        .create_host_invitation(&"b".repeat(64), "spark", 100, 0)
        .unwrap();
    let cert = store
        .redeem_host_invitation(
            &Redemption {
                invitation_digest: "b".repeat(64),
                transaction_id: "transaction".into(),
                host_name: "spark".into(),
                key_digest: "c".repeat(64),
                csr_digest: "d".repeat(64),
            },
            1,
            |host| {
                Ok(CertificateRecord {
                    host_id: host.into(),
                    fingerprint: "a".repeat(64),
                    certificate_pem: "certificate".into(),
                    expires_unix: 500,
                })
            },
        )
        .unwrap();
    let document = mllm_config::remote_roles::HostConfig::template(std::path::Path::new(
        "/home/operator/host",
    ));
    let fingerprint = mllm_config::remote_resources::policy_fingerprint(
        &serde_json::from_str(&document).unwrap(),
    );
    let publication = HostPublication {
        host_id: cert.host_id.clone(),
        config_json: document,
        boot_id: "boot-a".into(),
        fingerprint,
        received_at_ms: 100,
    };
    store.publish_host_configuration(&publication).unwrap();
    // SPEC §§3.1, 7.3 (T24 T33): per-launch claims are whatever the latest
    // publication said; a publication that does not say so (an older agent)
    // makes the host single-claim again, and the answer survives reopen.
    assert!(!store.host_has_per_launch_claims(&cert.host_id).unwrap());
    store
        .publish_host_configuration_with_claims(&publication, true)
        .unwrap();
    assert!(store.host_has_per_launch_claims(&cert.host_id).unwrap());
    drop(store);
    let store = Store::open(&path).unwrap();
    assert!(store.host_has_per_launch_claims(&cert.host_id).unwrap());
    assert!(!store.host_has_per_instance_fencing(&cert.host_id).unwrap());
    // ADR 0013 §4, §5 (T24 T34): per-instance fencing implies per-launch
    // claims, and is replaced by whatever the next publication says.
    store
        .publish_host_configuration_with_launch_claims(
            &publication,
            Some(mllm_store::host_publication::LaunchClaims::PerInstance),
        )
        .unwrap();
    assert!(store.host_has_per_launch_claims(&cert.host_id).unwrap());
    assert!(store.host_has_per_instance_fencing(&cert.host_id).unwrap());
    store
        .publish_host_configuration_with_claims(&publication, true)
        .unwrap();
    assert!(!store.host_has_per_instance_fencing(&cert.host_id).unwrap());
    store.publish_host_configuration(&publication).unwrap();
    assert!(!store.host_has_per_launch_claims(&cert.host_id).unwrap());
    drop(store);
    let store = Store::open(&path).unwrap();
    let saved = store.host_publication("spark").unwrap().unwrap();
    assert_eq!(saved.host_id, cert.host_id);
    assert_eq!(saved.fingerprint, publication.fingerprint);
    let mut wrong = publication.clone();
    wrong.host_id = "not-enrolled".into();
    assert!(store.publish_host_configuration(&wrong).is_err());
    wrong = publication.clone();
    wrong.fingerprint = "f".repeat(64);
    assert!(store.publish_host_configuration(&wrong).is_err());
    store.revoke_host(&cert.host_id).unwrap();
    assert!(store.publish_host_configuration(&publication).is_err());
    assert!(store.host_publication("spark").unwrap().is_none());
}

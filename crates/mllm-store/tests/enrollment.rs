use mllm_store::{
    enrollment::{CertificateRecord, Redemption},
    Store,
};
fn certificate(host: &str) -> CertificateRecord {
    CertificateRecord {
        host_id: host.into(),
        fingerprint: "a".repeat(64),
        certificate_pem: "certificate".into(),
        expires_unix: 500,
    }
}
fn request(tx: &str) -> Redemption {
    Redemption {
        invitation_digest: "b".repeat(64),
        transaction_id: tx.into(),
        host_name: "host-a".into(),
        key_digest: "c".repeat(64),
        csr_digest: "d".repeat(64),
    }
}
// T05 T06: one-use invitation and lost-response recovery survive restart.
#[test]
fn redemption_recovers_exact_result_after_expiry_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite3");
    let first = {
        let store = Store::open(&path).unwrap();
        store
            .create_host_invitation(&"b".repeat(64), "host-a", 100, 0)
            .unwrap();
        store
            .redeem_host_invitation(&request("transaction-one"), 1, |host| Ok(certificate(host)))
            .unwrap()
    };
    let store = Store::open(&path).unwrap();
    let recovered = store
        .redeem_host_invitation(&request("transaction-one"), 101, |_| {
            panic!("must replay persisted result")
        })
        .unwrap();
    assert_eq!(first, recovered);
    assert!(store
        .redeem_host_invitation(&request("other-transaction"), 2, |host| Ok(certificate(
            host
        )))
        .is_err());
    let mut changed = request("transaction-one");
    changed.key_digest = "e".repeat(64);
    assert!(store
        .redeem_host_invitation(&changed, 2, |host| Ok(certificate(host)))
        .is_err());
    changed = request("transaction-one");
    changed.host_name = "other".into();
    assert!(store
        .redeem_host_invitation(&changed, 2, |host| Ok(certificate(host)))
        .is_err());
    assert!(store.certificate_host(&first.fingerprint, 501).is_err());
    store.revoke_host(&first.host_id).unwrap();
    assert!(store.certificate_host(&first.fingerprint, 2).is_err());
    assert!(store
        .redeem_host_invitation(&request("transaction-one"), 2, |host| Ok(certificate(host)))
        .is_err());
}
// T05: failures never consume a valid invitation or replace an existing name.
#[test]
fn expiry_collision_and_issuance_failure_fail_closed() {
    let store = Store::open_in_memory().unwrap();
    store
        .create_host_invitation(&"b".repeat(64), "host-a", 100, 0)
        .unwrap();
    assert!(store
        .redeem_host_invitation(&request("transaction-one"), 100, |host| Ok(certificate(
            host
        )))
        .is_err());
    assert!(store
        .redeem_host_invitation(&request("transaction-one"), 1, |_| Err(
            mllm_store::StoreError::Conflict
        ))
        .is_err());
    store
        .redeem_host_invitation(&request("transaction-one"), 2, |host| Ok(certificate(host)))
        .unwrap();
    assert!(store
        .create_host_invitation(&"f".repeat(64), "host-a", 100, 2)
        .is_err());
}
// T05: two simultaneous redemptions cannot mint two identities.
#[test]
fn concurrent_redemption_has_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite3");
    Store::open(&path)
        .unwrap()
        .create_host_invitation(&"b".repeat(64), "host-a", 100, 0)
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                store
                    .redeem_host_invitation(&request(&format!("transaction-{i}")), 1, |host| {
                        Ok(certificate(host))
                    })
                    .is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| usize::from(h.join().unwrap()))
            .sum::<usize>(),
        1
    );
}
// T06: renewal keeps identity and replays its exact response after a lost reply.
#[test]
fn renewal_transaction_replays_and_rejects_changed_content() {
    let store = Store::open_in_memory().unwrap();
    store
        .create_host_invitation(&"b".repeat(64), "host-a", 100, 0)
        .unwrap();
    let first = store
        .redeem_host_invitation(&request("transaction-one"), 1, |host| Ok(certificate(host)))
        .unwrap();
    let renewed = store
        .renew_host_certificate(
            &first.fingerprint,
            &"c".repeat(64),
            "renewal-one",
            &"d".repeat(64),
            2,
            |host| {
                let mut c = certificate(host);
                c.fingerprint = "e".repeat(64);
                Ok(c)
            },
        )
        .unwrap();
    let replay = store
        .renew_host_certificate(
            &first.fingerprint,
            &"c".repeat(64),
            "renewal-one",
            &"d".repeat(64),
            3,
            |_| panic!("must replay"),
        )
        .unwrap();
    assert_eq!(renewed, replay);
    assert_eq!(first.host_id, renewed.host_id);
    assert!(store
        .renew_host_certificate(
            &first.fingerprint,
            &"c".repeat(64),
            "renewal-one",
            &"f".repeat(64),
            3,
            |host| Ok(certificate(host))
        )
        .is_err());
}

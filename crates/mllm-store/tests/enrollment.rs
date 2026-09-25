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

// T06 (SPEC §§4.1, 6.4, 13.3): a host is revoked by id or by its name; the
// first revocation is journaled once and a repeat is an idempotent no-op; an
// unknown or malformed name is refused and changes nothing.
#[test]
fn revocation_resolves_id_or_name_and_is_idempotent_and_journaled_once() {
    let store = Store::open_in_memory().unwrap();
    store
        .create_host_invitation(&"b".repeat(64), "host-a", 100, 0)
        .unwrap();
    let issued = store
        .redeem_host_invitation(&request("transaction-one"), 1, |host| Ok(certificate(host)))
        .unwrap();
    assert!(store.revoke_host("unknown-host").is_err());
    assert!(store.revoke_host("bad name").is_err());
    let first = store.revoke_host("host-a").unwrap();
    assert_eq!(first.host_id, issued.host_id);
    assert_eq!(first.host_name, "host-a");
    assert!(first.newly_revoked);
    let again = store.revoke_host(&issued.host_id).unwrap();
    assert_eq!(again.host_id, issued.host_id);
    assert!(!again.newly_revoked);
    assert!(store.enrolled_hosts().unwrap()[0].revoked);
    assert!(store.certificate_host(&issued.fingerprint, 2).is_err());
    let events = store.events_after(None, 100).unwrap().events;
    let revoked: Vec<_> = events.iter().filter(|e| e.kind == "host_revoked").collect();
    assert_eq!(revoked.len(), 1);
    let payload: serde_json::Value = serde_json::from_str(&revoked[0].payload_json).unwrap();
    assert_eq!(payload["host_id"], issued.host_id.as_str());
    assert_eq!(payload["host_name"], "host-a");
}

fn recovery(invitation: &str, tx: &str, key: &str) -> Redemption {
    Redemption {
        invitation_digest: invitation.repeat(64),
        transaction_id: tx.into(),
        host_name: "host-a".into(),
        key_digest: key.repeat(64),
        csr_digest: "9".repeat(64),
    }
}
fn certificate_with(host: &str, fingerprint: &str) -> CertificateRecord {
    let mut c = certificate(host);
    c.fingerprint = fingerprint.repeat(64);
    c
}

// T05 T06 (ADR 0016, SPEC §4.1): a revoked host re-enrolls under its same
// host id through an explicit recovery invitation. The new certificate is
// bound to the same id and the new key; the old certificate stays revoked by
// its own fingerprint for ever; the invitation is single-use (an exact retry
// replays, another transaction is refused) and expires; recovery of a host
// that is not revoked, or of an unknown host, is refused; both steps are
// journaled; and an ordinary invitation for the revoked name is still refused.
#[test]
fn recovery_reenrolls_the_same_host_and_keeps_the_old_certificate_revoked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite3");
    let store = Store::open(&path).unwrap();
    store
        .create_host_invitation(&"b".repeat(64), "host-a", 100, 0)
        .unwrap();
    let old = store
        .redeem_host_invitation(&request("transaction-one"), 1, |host| Ok(certificate(host)))
        .unwrap();
    // Only a revoked host may be recovered; an unknown one is not found.
    assert!(matches!(
        store.create_host_recovery_invitation(&"1".repeat(64), "host-a", 100, 2),
        Err(mllm_store::enrollment::RecoveryInvitationError::NotRevoked)
    ));
    assert!(matches!(
        store.create_host_recovery_invitation(&"1".repeat(64), "no-such-host", 100, 2),
        Err(mllm_store::enrollment::RecoveryInvitationError::NotFound)
    ));
    store.revoke_host("host-a").unwrap();
    // Name collision rules are unchanged: the revoked name takes no new host.
    assert!(store
        .create_host_invitation(&"2".repeat(64), "host-a", 100, 2)
        .is_err());
    // Short-lived: longer than an hour is refused.
    assert!(matches!(
        store.create_host_recovery_invitation(&"1".repeat(64), "host-a", 3700, 2),
        Err(mllm_store::enrollment::RecoveryInvitationError::Invalid)
    ));
    // By id or by name.
    let target = store
        .create_host_recovery_invitation(&"1".repeat(64), &old.host_id, 100, 2)
        .unwrap();
    assert_eq!(target.host_id, old.host_id);
    assert_eq!(target.host_name, "host-a");
    store
        .create_host_recovery_invitation(&"3".repeat(64), "host-a", 10, 2)
        .unwrap();
    // Expired: the second invitation is past its expiry and cannot be redeemed.
    assert!(store
        .redeem_host_invitation(&recovery("3", "late", "7"), 10, |host| {
            Ok(certificate_with(host, "8"))
        })
        .is_err());
    // A recovery invitation carries the enrolled name; another name is refused.
    let mut renamed = recovery("1", "recover-one", "7");
    renamed.host_name = "host-b".into();
    assert!(store
        .redeem_host_invitation(&renamed, 3, |host| Ok(certificate_with(host, "8")))
        .is_err());

    let recovered = store
        .redeem_host_invitation(&recovery("1", "recover-one", "7"), 3, |host| {
            Ok(certificate_with(host, "8"))
        })
        .unwrap();
    assert_eq!(recovered.host_id, old.host_id, "the same host id");
    assert_ne!(recovered.fingerprint, old.fingerprint, "a new certificate");
    let hosts = store.enrolled_hosts().unwrap();
    assert_eq!(hosts.len(), 1, "no second host record");
    assert!(!hosts[0].revoked);
    assert_eq!(store.certificate_host(&recovered.fingerprint, 4).unwrap().host_id, old.host_id);
    // The old certificate stays revoked, whatever the host's state now.
    assert!(store.certificate_host(&old.fingerprint, 4).is_err());
    // An exact retry of the recovery replays its result; any other
    // transaction on the same invitation is refused (single use).
    let replay = store
        .redeem_host_invitation(&recovery("1", "recover-one", "7"), 5, |_| {
            panic!("must replay the recorded recovery")
        })
        .unwrap();
    assert_eq!(replay, recovered);
    assert!(store
        .redeem_host_invitation(&recovery("1", "recover-two", "7"), 5, |host| {
            Ok(certificate_with(host, "a"))
        })
        .is_err());
    // The other outstanding recovery invitation is spent too: the host is no
    // longer revoked, so it recovers nothing.
    store
        .create_host_recovery_invitation(&"4".repeat(64), "host-a", 100, 5)
        .map(|_| ())
        .expect_err("an active host takes no recovery invitation");
    // Renewal follows the new key only.
    assert!(store
        .renew_host_certificate(&recovered.fingerprint, &"c".repeat(64), "renewal", &"d".repeat(64), 6, |host| {
            Ok(certificate_with(host, "e"))
        })
        .is_err());
    // Revoking again revokes the new certificate too; recovery never makes an
    // earlier certificate valid, and survives a reopen of the store.
    store.revoke_host("host-a").unwrap();
    store
        .create_host_recovery_invitation(&"5".repeat(64), "host-a", 100, 7)
        .unwrap();
    store
        .redeem_host_invitation(&recovery("5", "recover-three", "6"), 8, |host| {
            Ok(certificate_with(host, "f"))
        })
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert!(store.certificate_host(&old.fingerprint, 9).is_err());
    assert!(store.certificate_host(&recovered.fingerprint, 9).is_err());
    assert!(store.certificate_host(&"f".repeat(64), 9).is_ok());
    let events = store.events_after(None, 100).unwrap().events;
    let kinds: Vec<_> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "host_recovery_invited").count(), 3);
    assert_eq!(kinds.iter().filter(|k| **k == "host_recovered").count(), 2);
    let invited = events.iter().find(|e| e.kind == "host_recovery_invited").unwrap();
    let payload: serde_json::Value = serde_json::from_str(&invited.payload_json).unwrap();
    assert_eq!(payload["host_id"], old.host_id.as_str());
    assert_eq!(payload["expires_unix"], 100);
}

// T06 (ADR 0016): a host revoked before per-certificate revocation existed
// keeps its certificate refused after the upgrade and after recovery.
#[test]
fn certificates_of_hosts_revoked_before_v32_stay_revoked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite3");
    let old = {
        let store = Store::open(&path).unwrap();
        store
            .create_host_invitation(&"b".repeat(64), "host-a", 100, 0)
            .unwrap();
        let old = store
            .redeem_host_invitation(&request("transaction-one"), 1, |host| Ok(certificate(host)))
            .unwrap();
        store.revoke_host("host-a").unwrap();
        old
    };
    // Roll the store back to v31: forget the per-certificate revocation and
    // every later migration (a v31 store has none of them).
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE revoked_host_certificates; DROP TABLE host_recovery_invitations;
             DROP TABLE IF EXISTS model_sources;
             DELETE FROM schema_migrations WHERE version>=32;",
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    store
        .create_host_recovery_invitation(&"1".repeat(64), "host-a", 100, 2)
        .unwrap();
    store
        .redeem_host_invitation(&recovery("1", "recover-one", "7"), 3, |host| {
            Ok(certificate_with(host, "8"))
        })
        .unwrap();
    assert!(store.certificate_host(&old.fingerprint, 4).is_err());
    assert!(store.certificate_host(&"8".repeat(64), 4).is_ok());
}

// T06 (SPEC §4.1, ADR 0016): the controller tells a host its certificate is
// revoked only when the store says so for that exact certificate. An active
// certificate, an unknown fingerprint and the recovered host's new
// certificate are not revoked; the old one stays revoked after recovery.
#[test]
fn certificate_revoked_names_only_revoked_certificates() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("store.sqlite3")).unwrap();
    store
        .create_host_invitation(&"b".repeat(64), "host-a", 100, 0)
        .unwrap();
    let old = store
        .redeem_host_invitation(&request("transaction-one"), 1, |host| Ok(certificate(host)))
        .unwrap();
    assert!(!store.certificate_revoked(&old.fingerprint).unwrap());
    assert!(!store.certificate_revoked(&"e".repeat(64)).unwrap());
    assert!(store.certificate_revoked("not-a-fingerprint").is_err());

    store.revoke_host("host-a").unwrap();
    assert!(store.certificate_revoked(&old.fingerprint).unwrap());
    assert!(!store.certificate_revoked(&"e".repeat(64)).unwrap());

    store
        .create_host_recovery_invitation(&"1".repeat(64), "host-a", 100, 2)
        .unwrap();
    store
        .redeem_host_invitation(&recovery("1", "recover-one", "7"), 3, |host| {
            Ok(certificate_with(host, "8"))
        })
        .unwrap();
    assert!(store.certificate_revoked(&old.fingerprint).unwrap());
    assert!(!store.certificate_revoked(&"8".repeat(64)).unwrap());
}

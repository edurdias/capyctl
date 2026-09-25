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

fn enrolled(store: &Store) -> String {
    store
        .create_host_invitation(&"e".repeat(64), "spark-r", 100, 0)
        .unwrap();
    store
        .redeem_host_invitation(
            &Redemption {
                invitation_digest: "e".repeat(64),
                transaction_id: "tx-r".into(),
                host_name: "spark-r".into(),
                key_digest: "c".repeat(64),
                csr_digest: "d".repeat(64),
            },
            1,
            |host| {
                Ok(CertificateRecord {
                    host_id: host.into(),
                    fingerprint: "f".repeat(64),
                    certificate_pem: "c".into(),
                    expires_unix: 500,
                })
            },
        )
        .unwrap()
        .host_id
}

fn publication(host: &str, document: &serde_json::Value) -> HostPublication {
    HostPublication {
        host_id: host.into(),
        config_json: document.to_string(),
        boot_id: "boot-a".into(),
        fingerprint: mllm_config::remote_resources::policy_fingerprint(document),
        received_at_ms: 100,
    }
}

fn base_document() -> serde_json::Value {
    serde_json::from_str(&mllm_config::remote_roles::HostConfig::template(
        std::path::Path::new("/home/operator/host"),
    ))
    .unwrap()
}

fn vllm_profile() -> serde_json::Value {
    serde_json::json!({"engine":"vllm","revision":1,"executable":"/v/bin/vllm","build_fingerprint":"0.29.0",
        "args":[],"env":{},"log_policy":{"max_file_bytes":"16MiB","retained_files":3},
        "security":{"deep_park":"enabled","trust_remote_code":false,"credential_ref":"secret://engine-key","admin_credential_ref":"secret://admin-key"}})
}

// T07 T33 (ADR 0018 §3, §4): a live re-publication replaces the approved
// document only when it changes runtime profiles alone, only over the
// publication it was based on, and only drops a profile whose retirement the
// server confirmed; a refusal keeps the previous document.
#[test]
fn a_live_republication_changes_profiles_only_and_never_unretired_ones() {
    use mllm_store::host_publication::RepublishRefusal;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("s.sqlite3")).unwrap();
    let host = enrolled(&store);
    let base = base_document();
    store
        .publish_host_configuration(&publication(&host, &base))
        .unwrap();
    let mut added = base.clone();
    added["runtime_profiles"]["vllm"] = vllm_profile();
    let base_fp = mllm_config::remote_resources::policy_fingerprint(&base);
    store
        .republish_host_configuration(&publication(&host, &added), &base_fp)
        .unwrap();
    assert_eq!(
        store.host_publication(&host).unwrap().unwrap().config_json,
        added.to_string()
    );
    // Based on a stale publication: refused, nothing changes.
    assert_eq!(
        store.republish_host_configuration(&publication(&host, &base), &base_fp),
        Err(RepublishRefusal::PublicationChanged)
    );
    // Anything but profiles: refused.
    let added_fp = mllm_config::remote_resources::policy_fingerprint(&added);
    let mut edited = added.clone();
    // Within the 250ms..5s bound, so the document stays valid.
    edited["load_report_interval"] = "2s".into();
    assert_eq!(
        store.republish_host_configuration(&publication(&host, &edited), &added_fp),
        Err(RepublishRefusal::NotProfilesOnly)
    );
    // Dropping a profile the server did not retire: refused.
    assert_eq!(
        store.republish_host_configuration(&publication(&host, &base), &added_fp),
        Err(RepublishRefusal::NotRetired("vllm".into()))
    );
    assert_eq!(
        store.host_publication(&host).unwrap().unwrap().config_json,
        added.to_string(),
        "a refusal keeps the previous approved document"
    );
    // After a confirmed retirement: accepted, and the retirement is gone.
    assert!(matches!(
        store
            .begin_profile_retirement(&host, "vllm", "k", 1, 10, false)
            .unwrap(),
        mllm_store::profile_retirement::RetirementStart::Clear
    ));
    store
        .republish_host_configuration(&publication(&host, &base), &added_fp)
        .unwrap();
    assert!(store.profile_retirement(&host, "vllm").unwrap().is_none());
    assert_eq!(
        store.host_publication(&host).unwrap().unwrap().config_json,
        base.to_string()
    );
}

// T32 (ADR 0018 §4): a confirmed retirement is not an abandoned one. It
// outlives its deadline while the host's publication still lists the profile
// (so placement stays excluded), and the re-publication that drops the
// profile clears it.
#[test]
fn a_confirmed_retirement_outlives_its_deadline_until_the_profile_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("s.sqlite3")).unwrap();
    let host = enrolled(&store);
    let base = base_document();
    let mut added = base.clone();
    added["runtime_profiles"]["vllm"] = vllm_profile();
    store
        .publish_host_configuration(&publication(&host, &added))
        .unwrap();
    assert!(matches!(
        store
            .begin_profile_retirement(&host, "vllm", "k", 1, 10, false)
            .unwrap(),
        mllm_store::profile_retirement::RetirementStart::Clear
    ));
    assert!(store.expire_profile_retirements(1_000).unwrap().is_empty());
    let (_, state, _) = store.profile_retirement(&host, "vllm").unwrap().unwrap();
    assert_eq!(state, "confirmed");
    let added_fp = mllm_config::remote_resources::policy_fingerprint(&added);
    store
        .republish_host_configuration(&publication(&host, &base), &added_fp)
        .unwrap();
    assert!(store.profile_retirement(&host, "vllm").unwrap().is_none());
}

// T32 T33 (ADR 0018 §4, controller ruling): any accepted publication clears
// the host's confirmed retirements for profiles it no longer lists, the
// startup publication as well as a live one. A confirmed retirement survives a
// host restart only while the startup publication still lists the profile.
#[test]
fn a_startup_publication_clears_confirmed_retirements_only_for_dropped_profiles() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("s.sqlite3")).unwrap();
    let host = enrolled(&store);
    let base = base_document();
    let mut added = base.clone();
    added["runtime_profiles"]["vllm"] = vllm_profile();
    store
        .publish_host_configuration(&publication(&host, &added))
        .unwrap();
    assert!(matches!(
        store
            .begin_profile_retirement(&host, "vllm", "k", 1, 10, false)
            .unwrap(),
        mllm_store::profile_retirement::RetirementStart::Clear
    ));
    // Restart, still listing the profile: the retirement stays confirmed.
    let mut restarted = publication(&host, &added);
    restarted.boot_id = "boot-b".into();
    store.publish_host_configuration(&restarted).unwrap();
    let (_, state, _) = store.profile_retirement(&host, "vllm").unwrap().unwrap();
    assert_eq!(state, "confirmed");
    // Restart without the profile: the retirement is cleared.
    let mut dropped = publication(&host, &base);
    dropped.boot_id = "boot-c".into();
    store.publish_host_configuration(&dropped).unwrap();
    assert!(store.profile_retirement(&host, "vllm").unwrap().is_none());
}

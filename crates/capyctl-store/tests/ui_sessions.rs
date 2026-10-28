//! ADR 0040 §§4, 5, 7: browser sessions and one-time sign-in links, stored by
//! SHA-256 digest. Store-only tests; they qualify no engine.
use capyctl_store::ui_sessions::SigninOutcome;
use capyctl_store::Store;

const MINUTE_MS: i64 = 60_000;
const LINK_LIFETIME_MS: i64 = 10 * MINUTE_MS;
const ADMIN_A: [u8; 32] = [0xa0; 32];
const ADMIN_B: [u8; 32] = [0xb0; 32];

fn store() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("srv.sqlite3");
    let store = Store::open(&path).unwrap();
    (dir, path, store)
}

fn digest(byte: u8) -> [u8; 32] {
    [byte; 32]
}

/// Issues link `link` under `admin` at `now` and consumes it at `now` for the
/// session token `session`, returning the new session's id.
fn sign_in(store: &Store, link: u8, session: u8, admin: &[u8; 32], now: i64) -> String {
    store
        .issue_ui_signin_link(&digest(link), admin, now + LINK_LIFETIME_MS)
        .unwrap();
    match store
        .consume_ui_signin_link(
            &digest(link),
            &digest(session),
            admin,
            "Firefox on Linux",
            now,
        )
        .unwrap()
    {
        SigninOutcome::SignedIn { session_id } => session_id,
        other => panic!("expected a session, got {other:?}"),
    }
}

// ADR 0040 §5 (AE1): a link works once; the second consume creates nothing.
#[test]
fn a_consumed_link_is_used_and_creates_no_second_session() {
    let (_dir, _path, store) = store();
    let id = sign_in(&store, 1, 2, &ADMIN_A, 1_000);
    let session = store.ui_session(&digest(2)).unwrap().expect("session");
    assert_eq!(session.id, id);
    assert_eq!(session.label, "Firefox on Linux");
    assert_eq!(session.created_at_ms, 1_000);
    assert_eq!(
        store
            .consume_ui_signin_link(&digest(1), &digest(3), &ADMIN_A, "Safari", 2_000)
            .unwrap(),
        SigninOutcome::Used
    );
    assert!(store.ui_session(&digest(3)).unwrap().is_none());
    assert_eq!(store.ui_sessions().unwrap().len(), 1);
}

// ADR 0040 §5: a link lasts 10 minutes from issue.
#[test]
fn a_link_consumed_ten_minutes_and_a_second_after_issue_is_expired() {
    let (_dir, _path, store) = store();
    let issued = 5_000;
    store
        .issue_ui_signin_link(&digest(1), &ADMIN_A, issued + LINK_LIFETIME_MS)
        .unwrap();
    assert_eq!(
        store
            .consume_ui_signin_link(
                &digest(1),
                &digest(2),
                &ADMIN_A,
                "Chrome",
                issued + LINK_LIFETIME_MS + 1_000
            )
            .unwrap(),
        SigninOutcome::Expired
    );
    assert!(store.ui_session(&digest(2)).unwrap().is_none());
    assert!(store.ui_sessions().unwrap().is_empty());
}

// ADR 0040 §5: a link the store never issued is unknown.
#[test]
fn an_unissued_link_is_unknown() {
    let (_dir, _path, store) = store();
    assert_eq!(
        store
            .consume_ui_signin_link(&digest(9), &digest(2), &ADMIN_A, "Chrome", 0)
            .unwrap(),
        SigninOutcome::Unknown
    );
}

// ADR 0040 §4: consumption is one IMMEDIATE transaction, so two concurrent
// consumes of one link (two connections, as two requests) yield one session.
#[test]
fn two_concurrent_consumes_of_one_link_yield_exactly_one_session() {
    let (_dir, path, store) = store();
    store
        .issue_ui_signin_link(&digest(1), &ADMIN_A, LINK_LIFETIME_MS)
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let racers: Vec<_> = [2u8, 3u8]
        .into_iter()
        .map(|session| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                store
                    .consume_ui_signin_link(&digest(1), &digest(session), &ADMIN_A, "Phone", 10)
                    .unwrap()
            })
        })
        .collect();
    let outcomes: Vec<SigninOutcome> = racers.into_iter().map(|r| r.join().unwrap()).collect();
    let signed = outcomes
        .iter()
        .filter(|o| matches!(o, SigninOutcome::SignedIn { .. }))
        .count();
    let used = outcomes
        .iter()
        .filter(|o| **o == SigninOutcome::Used)
        .count();
    assert_eq!((signed, used), (1, 1), "{outcomes:?}");
    assert_eq!(store.ui_sessions().unwrap().len(), 1);
}

// ADR 0040 §7 (AE2): reopening the store under another admin fingerprint and
// sweeping, as a role start does, deletes the session.
#[test]
fn reopening_under_another_admin_fingerprint_deletes_the_session() {
    let (_dir, path, store) = store();
    sign_in(&store, 1, 2, &ADMIN_A, 0);
    assert!(store.ui_session(&digest(2)).unwrap().is_some());
    drop(store);
    let store = Store::open(&path).unwrap();
    store.sweep_ui_credentials(&ADMIN_B).unwrap();
    assert!(store.ui_session(&digest(2)).unwrap().is_none());
    assert!(store.ui_sessions().unwrap().is_empty());
}

// ADR 0040 §7: a sweep under the same fingerprint keeps sessions and links.
#[test]
fn a_sweep_under_the_same_admin_fingerprint_keeps_everything() {
    let (_dir, _path, store) = store();
    sign_in(&store, 1, 2, &ADMIN_A, 0);
    store
        .issue_ui_signin_link(&digest(5), &ADMIN_A, LINK_LIFETIME_MS)
        .unwrap();
    store.sweep_ui_credentials(&ADMIN_A).unwrap();
    assert!(store.ui_session(&digest(2)).unwrap().is_some());
    assert!(matches!(
        store
            .consume_ui_signin_link(&digest(5), &digest(6), &ADMIN_A, "Chrome", 1)
            .unwrap(),
        SigninOutcome::SignedIn { .. }
    ));
}

// ADR 0040 §7: consuming checks the link's fingerprint, so a link issued under
// A and consumed after a restart under B is expired and signs no one in; the
// role-start sweep then deletes it.
#[test]
fn a_link_issued_under_another_admin_fingerprint_is_expired() {
    let (_dir, path, store) = store();
    store
        .issue_ui_signin_link(&digest(1), &ADMIN_A, LINK_LIFETIME_MS)
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .consume_ui_signin_link(&digest(1), &digest(2), &ADMIN_B, "Chrome", 1)
            .unwrap(),
        SigninOutcome::Expired
    );
    assert!(store.ui_session(&digest(2)).unwrap().is_none());
    store.sweep_ui_credentials(&ADMIN_B).unwrap();
    assert_eq!(
        store
            .consume_ui_signin_link(&digest(1), &digest(2), &ADMIN_B, "Chrome", 2)
            .unwrap(),
        SigninOutcome::Unknown
    );
    assert!(store.ui_sessions().unwrap().is_empty());
}

// ADR 0040 §6: revoke-all removes every session; revoking an unknown id finds
// nothing.
#[test]
fn revoke_all_removes_every_session_and_an_unknown_id_is_not_found() {
    let (_dir, _path, store) = store();
    let first = sign_in(&store, 1, 2, &ADMIN_A, 0);
    sign_in(&store, 3, 4, &ADMIN_A, 0);
    sign_in(&store, 5, 6, &ADMIN_A, 0);
    assert!(store.revoke_ui_session(&first).unwrap());
    assert!(store.ui_session(&digest(2)).unwrap().is_none());
    assert!(!store.revoke_ui_session(&first).unwrap());
    assert!(!store
        .revoke_ui_session("01ARZ3NDEKTSV4RRFFQ69G5FAV")
        .unwrap());
    assert_eq!(store.revoke_all_ui_sessions().unwrap(), 2);
    assert!(store.ui_sessions().unwrap().is_empty());
    assert!(store.ui_session(&digest(4)).unwrap().is_none());
    assert!(store.ui_session(&digest(6)).unwrap().is_none());
}

// ADR 0040 §4: last-seen is written at most once a minute.
#[test]
fn last_seen_is_not_rewritten_by_two_lookups_within_a_minute() {
    let (_dir, _path, store) = store();
    let id = sign_in(&store, 1, 2, &ADMIN_A, 1_000);
    let seen = |now| {
        let session = store.ui_session(&digest(2)).unwrap().expect("session");
        store.record_ui_session_seen(&session.id, now).unwrap()
    };
    assert!(!seen(1_000 + 20_000));
    assert!(!seen(1_000 + 59_999));
    assert_eq!(store.ui_sessions().unwrap()[0].last_seen_at_ms, 1_000);
    assert!(seen(1_000 + MINUTE_MS));
    assert!(!seen(1_000 + MINUTE_MS + 30_000));
    let listed = &store.ui_sessions().unwrap()[0];
    assert_eq!(
        (listed.id.as_str(), listed.last_seen_at_ms),
        (id.as_str(), 61_000)
    );
    assert!(!store.record_ui_session_seen("unknown", 500_000).unwrap());
}

// ADR 0040 §4: the cookie's last refresh is recorded per session.
#[test]
fn a_cookie_refresh_is_recorded() {
    let (_dir, _path, store) = store();
    let id = sign_in(&store, 1, 2, &ADMIN_A, 1_000);
    assert_eq!(
        store
            .ui_session(&digest(2))
            .unwrap()
            .unwrap()
            .cookie_refreshed_at_ms,
        1_000
    );
    assert!(store
        .record_ui_session_cookie_refresh(&id, 90_000_000)
        .unwrap());
    assert_eq!(
        store
            .ui_session(&digest(2))
            .unwrap()
            .unwrap()
            .cookie_refreshed_at_ms,
        90_000_000
    );
    assert!(!store
        .record_ui_session_cookie_refresh("unknown", 1)
        .unwrap());
}

// ADR 0040 §4: the session list carries ids and labels, and no token digest.
#[test]
fn the_session_list_contains_ids_and_labels_and_no_digest() {
    let (_dir, _path, store) = store();
    let first = sign_in(&store, 1, 2, &ADMIN_A, 1_000);
    store
        .issue_ui_signin_link(&digest(3), &ADMIN_A, LINK_LIFETIME_MS)
        .unwrap();
    let SigninOutcome::SignedIn { session_id: second } = store
        .consume_ui_signin_link(&digest(3), &digest(4), &ADMIN_A, "Safari on iPhone", 2_000)
        .unwrap()
    else {
        panic!("expected a session");
    };
    let listed = store.ui_sessions().unwrap();
    let rows: Vec<(&str, &str, i64)> = listed
        .iter()
        .map(|s| (s.id.as_str(), s.label.as_str(), s.created_at_ms))
        .collect();
    assert_eq!(
        rows,
        vec![
            (first.as_str(), "Firefox on Linux", 1_000),
            (second.as_str(), "Safari on iPhone", 2_000)
        ]
    );
    // Session ids are ULIDs, never digests.
    for session in &listed {
        assert!(
            ulid::Ulid::from_string(&session.id).is_ok(),
            "{}",
            session.id
        );
    }
    let rendered = format!("{listed:?}");
    for token in [digest(2), digest(4)] {
        assert!(!rendered.contains(&hex::encode(token)), "{rendered}");
        assert!(!rendered.contains(&format!("{:?}", token)), "{rendered}");
    }
}

// ADR 0040 §5: pruning deletes links past their expiry, used or not, and keeps
// live ones, so a link used within its window still reads as used.
#[test]
fn pruning_deletes_only_expired_links() {
    let (_dir, _path, store) = store();
    sign_in(&store, 1, 2, &ADMIN_A, 0);
    store
        .issue_ui_signin_link(&digest(3), &ADMIN_A, LINK_LIFETIME_MS)
        .unwrap();
    store
        .issue_ui_signin_link(&digest(4), &ADMIN_A, 2 * LINK_LIFETIME_MS)
        .unwrap();
    assert_eq!(
        store.prune_ui_signin_links(LINK_LIFETIME_MS - 1).unwrap(),
        0
    );
    assert_eq!(
        store
            .consume_ui_signin_link(&digest(1), &digest(9), &ADMIN_A, "x", LINK_LIFETIME_MS - 1)
            .unwrap(),
        SigninOutcome::Used
    );
    assert_eq!(store.prune_ui_signin_links(LINK_LIFETIME_MS).unwrap(), 2);
    for (link, outcome) in [(1, SigninOutcome::Unknown), (3, SigninOutcome::Unknown)] {
        assert_eq!(
            store
                .consume_ui_signin_link(&digest(link), &digest(9), &ADMIN_A, "x", LINK_LIFETIME_MS)
                .unwrap(),
            outcome
        );
    }
    assert!(matches!(
        store
            .consume_ui_signin_link(&digest(4), &digest(9), &ADMIN_A, "x", LINK_LIFETIME_MS)
            .unwrap(),
        SigninOutcome::SignedIn { .. }
    ));
    // Sessions are not links: pruning left the first one.
    assert!(store.ui_session(&digest(2)).unwrap().is_some());
}

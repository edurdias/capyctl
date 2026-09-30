//! Owner decision (4), 2026-09-22: a drain of a host leaves a durable marker
//! while any of its Stops is unsettled, and the host takes no new placements
//! while it stands. Store-only tests; they qualify no engine.
use capyctl_store::Store;

fn store() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("srv.sqlite3");
    let store = Store::open(&path).unwrap();
    (dir, path, store)
}

fn operation(path: &std::path::Path, id: &str, state: &str) {
    let sql = rusqlite::Connection::open(path).unwrap();
    sql.execute(
        "INSERT INTO operations(id,kind,state) VALUES(?1,'ordinary_cleanup',?2)
         ON CONFLICT(id) DO UPDATE SET state=excluded.state",
        [id, state],
    )
    .unwrap();
}

// T10 T33: the marker stands while any of the drain's Stops is unsettled,
// survives reopening the store, and clears once every Stop settled, whatever
// its outcome.
#[test]
fn a_drain_marker_stands_until_every_stop_settles() {
    let (_dir, path, store) = store();
    assert!(!store.host_drain_pending("lab").unwrap());
    operation(&path, "op-a", "running");
    operation(&path, "op-b", "queued");
    store
        .record_host_drain("lab", &["op-a".to_string(), "op-b".to_string()], 10)
        .unwrap();
    assert!(store.host_drain_pending("lab").unwrap());
    assert!(!store.host_drain_pending("other").unwrap());
    assert_eq!(
        store.hosts_with_pending_drain().unwrap(),
        ["lab".to_string()].into_iter().collect()
    );
    // Recording the same drain again (a retried request) changes nothing.
    store
        .record_host_drain("lab", &["op-a".to_string(), "op-b".to_string()], 11)
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert!(
        store.host_drain_pending("lab").unwrap(),
        "durable across restart"
    );
    operation(&path, "op-a", "succeeded");
    assert!(
        store.host_drain_pending("lab").unwrap(),
        "one Stop still open"
    );
    operation(&path, "op-b", "failed");
    assert!(!store.host_drain_pending("lab").unwrap());
    assert!(store.hosts_with_pending_drain().unwrap().is_empty());
}

// T10: a marker naming no operation, or an operation that does not exist, is
// refused rather than recorded as a pending drain nothing can ever clear.
#[test]
fn a_drain_marker_names_existing_operations_only() {
    let (_dir, _path, store) = store();
    assert!(store.record_host_drain("lab", &[], 1).is_err());
    assert!(store
        .record_host_drain("lab", &["missing".to_string()], 1)
        .is_err());
    assert!(store
        .record_host_drain("", &["missing".to_string()], 1)
        .is_err());
    assert!(!store.host_drain_pending("lab").unwrap());
}

// T10 T33 (router review item 14): the drain intent is written before any Stop
// exists, in the same transaction that enumerates the host's instances, and it
// alone holds the host out of placement. A drain interrupted before its first
// Stop (a crash, a refused Stop) leaves the host held, durably, until a drain
// of it completes.
#[test]
fn a_drain_intent_holds_the_host_before_any_stop_exists() {
    let (_dir, path, store) = store();
    let named = store.begin_host_drain("lab", "key-1", 5).unwrap();
    assert!(named.is_empty(), "no instance holds a runtime on the host");
    assert!(store.host_drain_pending("lab").unwrap());
    assert!(!store.host_drain_pending("other").unwrap());
    assert_eq!(
        store.hosts_with_pending_drain().unwrap(),
        ["lab".to_string()].into_iter().collect()
    );
    drop(store);
    let store = Store::open(&path).unwrap();
    assert!(
        store.host_drain_pending("lab").unwrap(),
        "an interrupted drain keeps the host held across restart"
    );
    store.complete_host_drain("lab", "key-1", 6).unwrap();
    assert!(!store.host_drain_pending("lab").unwrap());
    assert!(store.hosts_with_pending_drain().unwrap().is_empty());
}

// T10 T33: once the intent is complete, the drain stays pending until every
// Stop it issued settles, exactly as the marker did before intents existed.
// Completing a drain completes every open intent on the host: its enumeration
// ran after they were written, so it named whatever they would have.
#[test]
fn a_completed_intent_leaves_the_host_held_until_its_stops_settle() {
    let (_dir, path, store) = store();
    store.begin_host_drain("lab", "interrupted", 1).unwrap();
    store.begin_host_drain("lab", "key-2", 2).unwrap();
    operation(&path, "op-a", "running");
    store
        .record_host_drain("lab", &["op-a".to_string()], 3)
        .unwrap();
    store.complete_host_drain("lab", "key-2", 4).unwrap();
    assert!(store.host_drain_pending("lab").unwrap(), "op-a is open");
    operation(&path, "op-a", "succeeded");
    assert!(
        !store.host_drain_pending("lab").unwrap(),
        "the interrupted intent was completed by the later drain"
    );
    // A retried drain under a completed key holds the host again until it
    // completes, so an instance it names is never placed around.
    store.begin_host_drain("lab", "key-2", 5).unwrap();
    assert!(store.host_drain_pending("lab").unwrap());
    store.complete_host_drain("lab", "key-2", 6).unwrap();
    assert!(!store.host_drain_pending("lab").unwrap());
}

// T10: an intent names a host and a drain key.
#[test]
fn a_drain_intent_names_a_host_and_a_key() {
    let (_dir, _path, store) = store();
    assert!(store.begin_host_drain("", "k", 1).is_err());
    assert!(store.begin_host_drain("lab", "", 1).is_err());
    assert!(store.begin_host_drain("lab", "k", -1).is_err());
    assert!(!store.host_drain_pending("lab").unwrap());
}

fn expired_events(store: &Store) -> Vec<serde_json::Value> {
    store
        .events_after(None, 1000)
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == "host_drain_intent_expired")
        .map(|e| serde_json::from_str(&e.payload_json).unwrap())
        .collect()
}

// T10 T33 (SPEC §4.3): a drain whose server stopped after it opened its intent
// and before it recorded any Stop no longer holds the host forever. Before its
// deadline the intent holds; after it, with no Stop of the host open, it is
// completed and journaled once, and the host is a candidate again.
#[test]
fn an_abandoned_intent_expires_after_its_deadline() {
    let (_dir, _path, store) = store();
    store
        .begin_host_drain_until("lab", "crashed", 1_000, Some(5_000))
        .unwrap();
    assert!(store.expire_host_drain_intents(4_999).unwrap().is_empty());
    assert!(
        store.host_drain_pending("lab").unwrap(),
        "held before its deadline"
    );
    assert_eq!(
        store.expire_host_drain_intents(5_000).unwrap(),
        vec![("lab".to_owned(), "crashed".to_owned())]
    );
    assert!(!store.host_drain_pending("lab").unwrap());
    assert!(store.hosts_with_pending_drain().unwrap().is_empty());
    assert!(
        store.expire_host_drain_intents(9_000).unwrap().is_empty(),
        "journaled once"
    );
    let events = expired_events(&store);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["host_id"], "lab");
    assert_eq!(events[0]["drain_key"], "crashed");
    assert_eq!(events[0]["deadline_ms"], 5_000);
}

// T10 T32 (SPEC §4.3, fail closed): an intent past its deadline stays open
// while any Stop recorded for its host is unsettled; it expires only once
// every one of them has settled.
#[test]
fn an_intent_past_its_deadline_holds_while_a_stop_is_open() {
    let (_dir, path, store) = store();
    store
        .begin_host_drain_until("lab", "crashed", 1_000, Some(5_000))
        .unwrap();
    operation(&path, "op-a", "running");
    store
        .record_host_drain("lab", &["op-a".to_string()], 2_000)
        .unwrap();
    assert!(store.expire_host_drain_intents(60_000).unwrap().is_empty());
    assert!(store.host_drain_pending("lab").unwrap());
    operation(&path, "op-a", "failed");
    assert_eq!(store.expire_host_drain_intents(60_000).unwrap().len(), 1);
    assert!(!store.host_drain_pending("lab").unwrap());
}

// T10 T33: an intent recorded without a deadline (before schema v31) expires
// the drain window after its recording, and a retried drain keeps the first
// deadline its key recorded.
#[test]
fn a_legacy_intent_expires_after_the_drain_window_and_retries_keep_the_deadline() {
    let (_dir, _path, store) = store();
    store.begin_host_drain("old", "v30", 1_000).unwrap();
    let window = capyctl_store::host_drain::LEGACY_INTENT_WINDOW_MS;
    assert!(store
        .expire_host_drain_intents(1_000 + window - 1)
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .expire_host_drain_intents(1_000 + window)
            .unwrap()
            .len(),
        1
    );

    store
        .begin_host_drain_until("lab", "key", 1_000, Some(5_000))
        .unwrap();
    store
        .begin_host_drain_until("lab", "key", 2_000, Some(90_000))
        .unwrap();
    assert_eq!(store.expire_host_drain_intents(5_000).unwrap().len(), 1);
    assert!(store
        .begin_host_drain_until("lab", "key", 1, Some(-1))
        .is_err());
}

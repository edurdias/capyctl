//! Owner decision (4), 2026-09-22: a drain of a host leaves a durable marker
//! while any of its Stops is unsettled, and the host takes no new placements
//! while it stands. Store-only tests; they qualify no engine.
use mllm_store::Store;

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

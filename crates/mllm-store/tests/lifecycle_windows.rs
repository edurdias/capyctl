//! Owner decision 2026-09-22 (1), ADR 0014 amendment A1: the Initialize and
//! Stop windows a caller that names no deadline is given, and their status
//! view. The store refuses any lifecycle deadline beyond the revision's request
//! deadline (SPEC §6), so neither window may exceed it. CPU-only.

use mllm_config::effective::{resolve_effective, PENDING_INITIALIZE_MS, STOP_WINDOW_MS};
use mllm_store::Store;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

fn effective(request_deadline: &str, timeouts: Option<Value>) -> Value {
    let source: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let (mut deployment, host) = (
        source["input"]["deployment"].clone(),
        source["input"]["host"].clone(),
    );
    deployment["request_deadline"] = json!(request_deadline);
    if let Some(timeouts) = timeouts {
        deployment["timeouts"] = timeouts;
    }
    serde_json::to_value(resolve_effective(&deployment, &host).unwrap()).unwrap()
}

fn store_with(effective: &Value, on_host: Option<&Value>) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("windows.sqlite3");
    let store = Store::open(&path).unwrap();
    let writer = Connection::open(path).unwrap();
    writer
        .execute_batch(
            "INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('d','toy','managed','ready',1,0,1,1);",
        )
        .unwrap();
    writer
        .execute(
            "INSERT INTO effective_revisions VALUES('d',1,?1,'digest')",
            params![effective.to_string()],
        )
        .unwrap();
    if let Some(host) = on_host {
        writer
            .execute(
                "INSERT INTO host_effective_revisions(deployment_id,revision,host_id,outcome,effective_json,fingerprint) VALUES('d',1,'h2','resolved',?1,'digest')",
                params![host.to_string()],
            )
            .unwrap();
    }
    (dir, store)
}

/// T14 T20: a declared Initialize timeout is the start window; the Stop window
/// is the fixed one lowered to the request deadline; status shows both with
/// the declared/derived provenance.
// T14 T20
#[test]
fn declared_timeouts_are_the_windows_and_status_shows_them() {
    let e = effective("300s", Some(json!({"initialize": "4m"})));
    let (_dir, store) = store_with(&e, None);
    let windows = store.lifecycle_windows("d").unwrap().unwrap();
    assert_eq!(windows.request_deadline_ms, 300_000);
    assert_eq!(windows.initialize_ms, 240_000);
    assert_eq!(windows.stop_ms, 300_000.min(STOP_WINDOW_MS));
    assert_eq!(windows.provenance["initialize"], "declared");
    assert_eq!(windows.provenance["wake"], "derived");
    let snapshot = serde_json::to_value(store.snapshot().unwrap()).unwrap();
    let shown = &snapshot["deployments"][0]["timeouts"];
    assert_eq!(shown["initialize_ms"], 240_000);
    assert_eq!(shown["stop_ms"], 300_000);
    assert_eq!(shown["provenance"]["initialize"], "declared");
    assert!(store.lifecycle_windows("unknown").unwrap().is_none());
}

/// T20: the windows never exceed the smallest request deadline any host
/// resolved the revision with, so the store admits them on every host.
// T20
#[test]
fn windows_take_the_smallest_request_deadline_of_any_host() {
    let canonical = effective("600s", None);
    let shorter = effective("120s", None);
    let (_dir, store) = store_with(&canonical, Some(&shorter));
    let windows = store.lifecycle_windows("d").unwrap().unwrap();
    assert_eq!(windows.request_deadline_ms, 120_000);
    assert_eq!(windows.initialize_ms, 120_000);
    assert_eq!(windows.stop_ms, 120_000);
}

/// T08: a revision frozen before timeouts existed keeps working: the start
/// window is the pending value within its request deadline, and status shows
/// no provenance it never recorded.
// T08 T20
#[test]
fn a_revision_without_timeouts_gets_the_pending_window() {
    let mut e = effective("600s", None);
    e.as_object_mut().unwrap().remove("timeouts");
    let (_dir, store) = store_with(&e, None);
    let windows = store.lifecycle_windows("d").unwrap().unwrap();
    assert_eq!(windows.initialize_ms, PENDING_INITIALIZE_MS.min(600_000));
    assert_eq!(windows.wake_ms, None);
    assert!(windows.provenance.is_empty());
}

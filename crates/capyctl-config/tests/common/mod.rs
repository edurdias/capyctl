//! Shared helpers for the `capyctl-config` integration tests.
//!
//! For F0, [`engine_executed_marker`] performs an empty-state assertion:
//! startup must never create any entry under the state dir other than the
//! generated `config/` and `identity/` trees (no engine execution, no
//! other runtime side effects), so any other entry is reported as an
//  executed-engine marker. SPEC §15.2: "do not execute detected engines
//! merely because they are on PATH".

use std::path::PathBuf;

pub use capyctl_config::defaults::{resolve_startup, LoadOutcome};
pub use capyctl_config::ConfigKind;

/// Fresh per-test state directory under the system temp dir.
pub fn temp_state_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "capyctl-noconfig-test-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// True if anything outside the generated set (`config/`, `identity/`)
/// was created under `state_dir` during startup — for F0 this is the
/// stand-in for an engine-executed marker.
pub fn engine_executed_marker(state_dir: &std::path::Path) -> bool {
    match std::fs::read_dir(state_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .any(|e| e.file_name() != "config" && e.file_name() != "identity"),
        Err(_) => false,
    }
}

/// Entries under `state_dir/identity/` (credential files created there).
pub fn credential_fingerprint(state_dir: &std::path::Path) -> Box<dyn Iterator<Item = PathBuf>> {
    match std::fs::read_dir(state_dir.join("identity")) {
        Ok(rd) => Box::new(rd.filter_map(|e| e.ok()).map(|e| e.path())),
        Err(_) => Box::new(std::iter::empty()),
    }
}

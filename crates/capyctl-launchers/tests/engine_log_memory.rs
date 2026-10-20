//! Owner decision 2026-10-20 (SPEC §13.3 / T21): under `engine_logs: memory` a
//! launch's redacted output lives only in its log writer. Each test launches a
//! real child with this test binary as the writer (`relay_fixture`), then reads
//! the log through the same readers the management tail and the launch-failure
//! reason use. Its own test binary: the setting is per process.
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use capyctl_adapters::engine_log::{read_tail, TailError, TAIL_DEFAULT_BYTES};
use capyctl_adapters::traits::{Launcher, RenderedCommand};
use capyctl_launchers::engine_log_relay::{keep_in_memory, relay_main, LogRelay, LOG_VARIABLE};
use capyctl_launchers::ExecLauncher;

/// The writer process: this binary re-run with only this test selected. It
/// does nothing in an ordinary test run, where the log variable is unset.
#[test]
fn relay_fixture() {
    if std::env::var_os(LOG_VARIABLE).is_some() {
        std::process::exit(relay_main());
    }
}

fn relay() -> LogRelay {
    LogRelay::new(
        std::env::current_exe().unwrap(),
        [
            "--exact",
            "relay_fixture",
            "--nocapture",
            "--test-threads=1",
        ],
    )
}

const KEY: &str = "vllm-inference-key-in-memory-0001";

fn launch(log: &Path, script: &str) {
    keep_in_memory(true).unwrap();
    let command = RenderedCommand {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        env: BTreeMap::from([
            (LOG_VARIABLE.to_owned(), log.to_str().unwrap().to_owned()),
            ("VLLM_API_KEY".to_owned(), KEY.to_owned()),
        ]),
    };
    ExecLauncher::new()
        .with_log_relay(relay())
        .spawn(&command)
        .unwrap();
}

/// Read until `done` appears (the engine has written everything), or 20 s.
fn read_until(done: impl Fn(&str) -> bool, read: impl Fn() -> String) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let text = read();
        if done(&text) || Instant::now() >= deadline {
            return text;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// T21 (owner decision 2026-10-20): memory mode writes no file, and the
// management read is served from the writer, redacted, after the engine exits.
#[test]
fn memory_mode_writes_no_file_and_serves_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("memory.log");
    launch(
        &log,
        "echo \"key $VLLM_API_KEY\"; echo ordinary output; echo engine-done",
    );
    let text = read_until(
        |text| text.contains("engine-done"),
        || {
            read_tail(&log, TAIL_DEFAULT_BYTES)
                .map(|tail| tail.text)
                .unwrap_or_default()
        },
    );
    assert!(text.contains("key <redacted>"), "{text}");
    assert!(text.contains("ordinary output"), "{text}");
    assert!(!text.contains(KEY), "{text}");
    // The engine has exited; the buffer still answers.
    std::thread::sleep(Duration::from_millis(200));
    let tail = read_tail(&log, TAIL_DEFAULT_BYTES).unwrap();
    assert!(tail.text.contains("engine-done"), "{}", tail.text);
    assert!(!tail.truncated);
    // Nothing reached the disk: no log, no rotation, no raw marker.
    assert!(!log.exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    // Another log has no buffer.
    assert_eq!(
        read_tail(&dir.path().join("other.log"), 1024),
        Err(TailError::Missing)
    );
}

// T20 T21 (owner decision 2026-10-20): the reason a launch failed is still
// read from the engine's own output when that output is kept in memory.
#[test]
fn a_startup_failure_is_classified_from_memory() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("failed.log");
    launch(
        &log,
        "echo starting; echo 'vllm serve: error: unrecognized arguments: --bogus-flag' >&2; exit 2",
    );
    let path = log.to_str().unwrap();
    let tail = read_until(
        |tail| tail.contains("--bogus-flag"),
        || capyctl_adapters::launch_failure::log_tail(Some(path)),
    );
    assert_eq!(
        capyctl_adapters::launch_failure::summary(&tail, None),
        "the engine exited before readiness; it rejected argument --bogus-flag"
    );
    assert!(!log.exists());
}

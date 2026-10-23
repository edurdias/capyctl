//! ADR 0033 §§3, 7, 10: the LMCache server as a second owned process group of
//! one launch, with real child processes: its closed environment and sibling
//! log, the once-only spawn keys, the per-launch spawn policy (no core dumps,
//! a memory-only log while the host keeps files), and the verified stop of
//! both groups. Its own test binary: the log writer fixture re-runs it.
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use capyctl_adapters::engine_log::{read_tail, TAIL_DEFAULT_BYTES};
use capyctl_adapters::traits::{OwnedProcessLaunch, RenderedCommand, RuntimeError};
use capyctl_domain::completion::ProcessIdentity;
use capyctl_launchers::engine_log_relay::{relay_main, LogRelay, LOG_VARIABLE};
use capyctl_launchers::{AssociationError, DurableProcessLaunch, LaunchAssociation, SpawnPolicy};

/// The log writer process: this binary re-run with only this test selected.
/// It does nothing in an ordinary test run, where the log variable is unset.
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

/// Records what was persisted, under which kind, and accepts it.
#[derive(Default)]
struct Recorder(Mutex<Vec<(String, ProcessIdentity)>>);

impl Recorder {
    fn kinds(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }
}

impl LaunchAssociation for Recorder {
    fn persist_api_identity(&self, identity: &ProcessIdentity) -> Result<(), AssociationError> {
        self.0
            .lock()
            .unwrap()
            .push(("api".into(), identity.clone()));
        Ok(())
    }
    fn persist_lmcache_identity(&self, identity: &ProcessIdentity) -> Result<(), AssociationError> {
        self.0
            .lock()
            .unwrap()
            .push(("lmcache".into(), identity.clone()));
        Ok(())
    }
    fn persist_wake_api_identity(
        &self,
        wake: &str,
        identity: &ProcessIdentity,
    ) -> Result<(), AssociationError> {
        self.0
            .lock()
            .unwrap()
            .push((format!("wake:{wake}"), identity.clone()));
        Ok(())
    }
}

fn command(script: &str, env: &[(&str, &str)]) -> RenderedCommand {
    RenderedCommand {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The variable names `env` printed into `text`, sorted.
fn printed_names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = text
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name.to_owned()))
        .collect();
    names.sort();
    names
}

// T45 T21 T37 (ADR 0033 §3, A7): the server runs with exactly PATH, HOME,
// CUDA_VISIBLE_DEVICES and LMCache's usage telemetry switched off, whatever
// else its command names (no engine key, no log variable, no other
// `LMCACHE_*`, a telemetry switch only with its off value, nothing the gate
// shell adds), and its output reaches its own sibling log. Its identity is
// recorded as `lmcache` before it runs. The server here is `env` itself,
// which prints the environment it was given. Found live: the server started
// with HOME and PATH only, and reported usage off the host.
#[test]
fn the_server_starts_with_exactly_its_closed_environment_and_its_own_log() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let tools = DurableProcessLaunch::new(recorder.clone());
    let spawn = |incarnation: &str, telemetry: &[(&str, &str)]| {
        let log = dir.path().join(format!("{incarnation}.lmcache.log"));
        let mut env: std::collections::BTreeMap<String, String> = [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/nonexistent"),
            ("CUDA_VISIBLE_DEVICES", "GPU-x"),
            (LOG_VARIABLE, log.to_str().unwrap()),
            ("VLLM_API_KEY", "must-not-leak"),
            ("LMCACHE_CONFIG_FILE", "/tmp/x"),
            (capyctl_adapters::engine_log::OWNED_VALUES_VARIABLE, "x"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        env.extend(
            telemetry
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        let server = tools
            .spawn_lmcache(
                incarnation,
                &RenderedCommand {
                    argv: vec!["/usr/bin/env".into()],
                    env,
                },
            )
            .unwrap();
        assert_eq!(server.role, "lmcache");
        let mut text = String::new();
        wait_for("the server's output", || {
            text = std::fs::read_to_string(&log).unwrap_or_default();
            text.contains("CUDA_VISIBLE_DEVICES=GPU-x")
        });
        tools.stop_group(&[server], Duration::from_secs(1)).unwrap();
        text
    };
    // The server's switches under the host's `engine_telemetry: off`.
    let text = spawn(
        "inc-env",
        &[("DO_NOT_TRACK", "1"), ("LMCACHE_TRACK_USAGE", "false")],
    );
    assert_eq!(recorder.kinds(), ["lmcache"]);
    assert_eq!(
        printed_names(&text),
        [
            "CUDA_VISIBLE_DEVICES",
            "DO_NOT_TRACK",
            "HOME",
            "LMCACHE_TRACK_USAGE",
            "PATH"
        ],
        "{text}"
    );
    assert!(text.contains("DO_NOT_TRACK=1\n") && text.contains("LMCACHE_TRACK_USAGE=false\n"));
    assert!(!text.contains("must-not-leak"));
    // A switch with any other value never reaches the server.
    let text = spawn(
        "inc-env-on",
        &[("DO_NOT_TRACK", "0"), ("LMCACHE_TRACK_USAGE", "true")],
    );
    assert_eq!(
        printed_names(&text),
        ["CUDA_VISIBLE_DEVICES", "HOME", "PATH"],
        "{text}"
    );
}

// T45 (ADR 0033 §§3, 7): one launch spawns its engine once, its server once,
// and its engine again once per wake step, each under its own key.
#[test]
fn each_spawn_of_a_launch_has_its_own_once_only_key() {
    let recorder = Arc::new(Recorder::default());
    let tools = DurableProcessLaunch::new(recorder.clone());
    let quick = command("exec sleep 30", &[("PATH", "/usr/bin:/bin")]);
    let engine = tools.spawn_durable("inc-keys", &quick).unwrap();
    let server = tools.spawn_lmcache("inc-keys", &quick).unwrap();
    assert!(matches!(
        tools.spawn_lmcache("inc-keys", &quick),
        Err(RuntimeError::Uncertain(_))
    ));
    let woken = tools
        .spawn_durable_wake("inc-keys", "step-1", &quick, None)
        .unwrap();
    assert!(tools
        .spawn_durable_wake("inc-keys", "step-1", &quick, None)
        .is_err());
    assert!(tools
        .spawn_durable_wake("inc-keys", "", &quick, None)
        .is_err());
    let again = tools
        .spawn_durable_wake("inc-keys", "step-2", &quick, None)
        .unwrap();
    assert!(tools.spawn_durable("inc-keys", &quick).is_err());
    assert_eq!(engine.role, "api");
    assert_eq!(woken.role, "api");
    assert_eq!(
        recorder.kinds(),
        ["api", "lmcache", "wake:step-1", "wake:step-2"]
    );
    tools
        .terminate_owned(&[engine, woken, again], Duration::from_secs(1))
        .unwrap();
    tools.stop_group(&[server], Duration::from_secs(1)).unwrap();
}

// T45 (ADR 0033 §10): a launch with `no_core_dumps` starts with RLIMIT_CORE 0,
// soft and hard, as the kernel reports it.
#[test]
fn a_no_core_dumps_launch_starts_with_a_zero_core_limit() {
    let tools = DurableProcessLaunch::new(Arc::new(Recorder::default())).with_policy(SpawnPolicy {
        no_core_dumps: true,
        memory_logs: false,
    });
    let engine = tools
        .spawn_durable(
            "inc-core",
            &command("exec sleep 30", &[("PATH", "/usr/bin:/bin")]),
        )
        .unwrap();
    let server = tools
        .spawn_lmcache(
            "inc-core",
            &command("exec sleep 30", &[("PATH", "/usr/bin:/bin")]),
        )
        .unwrap();
    for identity in [&engine, &server] {
        let limits = std::fs::read_to_string(format!("/proc/{}/limits", identity.pid)).unwrap();
        let core = limits
            .lines()
            .find(|line| line.starts_with("Max core file size"))
            .unwrap();
        let fields: Vec<&str> = core.split_whitespace().collect();
        assert_eq!(&fields[4..6], ["0", "0"], "{core}");
    }
    tools
        .terminate_owned(&[engine, server], Duration::from_secs(1))
        .unwrap();
}

// T45 T21 (ADR 0033 §10): a `retention: zero` launch keeps its output in the
// writer's memory while the host's `engine_logs` is `files` (this process
// never switched the global mode): no file is written, the tail is served.
#[test]
fn a_per_launch_memory_log_writes_no_file_under_host_files_mode() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("memory.log");
    let tools = DurableProcessLaunch::new(Arc::new(Recorder::default()))
        .with_policy(SpawnPolicy {
            no_core_dumps: false,
            memory_logs: true,
        })
        .with_log_relay(relay());
    let engine = tools
        .spawn_durable(
            "inc-memory",
            &command(
                "echo \"key $VLLM_API_KEY\"; echo memory-done; exec sleep 30",
                &[
                    (LOG_VARIABLE, log.to_str().unwrap()),
                    ("VLLM_API_KEY", "vllm-inference-key-memory-launch-01"),
                ],
            ),
        )
        .unwrap();
    let mut text = String::new();
    wait_for("the memory tail", || {
        text = read_tail(&log, TAIL_DEFAULT_BYTES)
            .map(|tail| tail.text)
            .unwrap_or_default();
        text.contains("memory-done")
    });
    let tail = read_tail(&log, TAIL_DEFAULT_BYTES).unwrap().text;
    assert!(tail.contains("key <redacted>"), "{tail}");
    assert!(!tail.contains("vllm-inference-key-memory-launch-01"));
    assert!(!log.exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    tools
        .terminate_owned(&[engine], Duration::from_secs(1))
        .unwrap();

    // The same host mode writes a file for a launch without the policy.
    let files = dir.path().join("files.log");
    let plain = DurableProcessLaunch::new(Arc::new(Recorder::default())).with_log_relay(relay());
    let engine = plain
        .spawn_durable(
            "inc-files",
            &command(
                "echo files-done; exec sleep 30",
                &[(LOG_VARIABLE, files.to_str().unwrap())],
            ),
        )
        .unwrap();
    wait_for("the file log", || {
        std::fs::read_to_string(&files).is_ok_and(|text| text.contains("files-done"))
    });
    plain
        .terminate_owned(&[engine], Duration::from_secs(1))
        .unwrap();
}

fn observed(
    what: &str,
    expected: usize,
    observe: impl Fn() -> Result<Vec<ProcessIdentity>, RuntimeError>,
) -> Vec<ProcessIdentity> {
    let mut group = Vec::new();
    wait_for(what, || {
        group = observe().unwrap_or_default();
        group.len() == expected
    });
    group
}

/// The live members of the group `leader` leads, as the verified stop reads
/// them (a held, exited leader is not one).
fn group_members(leader: &ProcessIdentity) -> usize {
    capyctl_launchers::group_observation::observe_process_group_or_empty(leader)
        .unwrap()
        .len()
}

fn touch(path: &Path) {
    std::fs::write(path, b"cuda").unwrap();
}

// T45 (ADR 0033 §3): one stop of a launch that holds both groups proves every
// recorded identity gone and both groups empty, children included; then
// (owner requirement from LC1) the stopped server's CUDA driver file is
// removed, and nothing else: the engine's file, another uid's file and a
// symlink in the server group's name stay.
#[test]
fn a_launch_stop_proves_both_groups_empty_and_removes_only_the_servers_shm_file() {
    let shm = tempfile::tempdir().unwrap();
    let tools = DurableProcessLaunch::new(Arc::new(Recorder::default()))
        .with_shm_root(shm.path().to_path_buf());
    let tree = command("sleep 60 & exec sleep 60", &[("PATH", "/usr/bin:/bin")]);
    let api = tools.spawn_durable("inc-stop", &tree).unwrap();
    let server = tools.spawn_lmcache("inc-stop", &tree).unwrap();
    let engine_group = observed("the engine's worker", 2, || tools.observe_group(&api));
    let server_group = observed("the server's child", 2, || {
        tools.observe_lmcache_group(&server)
    });
    assert_eq!(
        server_group
            .iter()
            .map(|identity| identity.role.as_str())
            .collect::<Vec<_>>(),
        ["lmcache", "lmcache-0"]
    );
    assert!(engine_group.iter().all(ProcessIdentity::is_engine));

    let uid = nix::unistd::geteuid().as_raw();
    let name = |uid: u32, pid: u32| shm.path().join(format!("cuda.shm.{uid:x}.{pid:x}.1"));
    let servers_file = name(uid, server.pid);
    let engines_file = name(uid, api.pid);
    let other_uid_file = name(uid + 1, server.pid);
    let target = shm.path().join("target");
    let member_link = name(uid, server_group[1].pid);
    for file in [&servers_file, &engines_file, &other_uid_file, &target] {
        touch(file);
    }
    std::os::unix::fs::symlink(&target, &member_link).unwrap();

    let all: Vec<ProcessIdentity> = engine_group
        .iter()
        .chain(server_group.iter())
        .cloned()
        .collect();
    tools.terminate_owned(&all, Duration::from_secs(1)).unwrap();
    assert_eq!(group_members(&api), 0, "the engine's group is empty");
    assert_eq!(group_members(&server), 0, "the server's group is empty");
    assert_eq!(
        capyctl_launchers::process_absence::verify_gone(&all),
        capyctl_launchers::process_absence::GoneProof::AllGone
    );
    assert!(!servers_file.exists(), "the server's file is removed");
    assert!(
        engines_file.exists(),
        "an engine's file is not the server's"
    );
    assert!(
        other_uid_file.exists(),
        "another uid's file is never touched"
    );
    assert!(
        std::fs::symlink_metadata(&member_link).is_ok() && target.exists(),
        "a symlink is never followed or removed"
    );
}

// T45 (ADR 0033 §7): a group stop names exactly one owned group: the
// engine's alone (the server keeps running), never both at once.
#[test]
fn a_group_stop_stops_one_group_and_leaves_the_other() {
    let tools = DurableProcessLaunch::new(Arc::new(Recorder::default()));
    let tree = command("sleep 60 & exec sleep 60", &[("PATH", "/usr/bin:/bin")]);
    let api = tools.spawn_durable("inc-park", &tree).unwrap();
    let server = tools.spawn_lmcache("inc-park", &tree).unwrap();
    let engine_group = observed("the engine's worker", 2, || tools.observe_group(&api));
    let server_group = observed("the server's child", 2, || {
        tools.observe_lmcache_group(&server)
    });
    let both: Vec<ProcessIdentity> = engine_group
        .iter()
        .chain(server_group.iter())
        .cloned()
        .collect();
    assert!(tools.stop_group(&both, Duration::from_secs(1)).is_err());
    tools
        .stop_group(&engine_group, Duration::from_secs(1))
        .unwrap();
    assert_eq!(group_members(&api), 0);
    assert_eq!(group_members(&server), 2, "the server keeps running");
    tools
        .stop_group(&server_group, Duration::from_secs(1))
        .unwrap();
    assert_eq!(group_members(&server), 0);
}

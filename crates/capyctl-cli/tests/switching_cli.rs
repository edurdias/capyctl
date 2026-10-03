//! W10 wiring and owner decision 2026-09-23: `start deployment --evict` and `start instance
//! --evict`. A default start never evicts; `--evict` runs the W10 switch plan
//! and reports the victims. Fake engine only; not qualification of any
//! native engine recipe (SPEC §18).
mod support;

/// The binary under test (`support::capyctl`), told the management address this
/// test serves on, which the test sets as `CAPYCTL_MANAGEMENT_ADDR` in its own
/// environment (the isolation drops the developer's `CAPYCTL_*` variables).
fn capyctl() -> std::process::Command {
    let mut command = support::capyctl();
    if let Ok(address) = std::env::var(capyctl_cli::roles::MANAGEMENT_ADDR_ENV) {
        command.env(capyctl_cli::roles::MANAGEMENT_ADDR_ENV, address);
    }
    command
}

use capyctl_cli::grammar::{parse_invocation, Command, LifecycleAction};
use capyctl_config::effective::{Engine, ModelSource};
use serde_json::Value;

// T10: the flag parses on both start forms, defaults off, and is refused
// anywhere else; help names it.
#[test]
fn evict_is_a_start_flag_only() {
    let parsed = parse_invocation(["capyctl", "start", "deployment", "b", "--evict"]).unwrap();
    assert!(parsed.evict);
    assert_eq!(
        parsed.command,
        Command::Lifecycle {
            action: LifecycleAction::Start,
            deployment: "b".into()
        }
    );
    let parsed = parse_invocation(["capyctl", "start", "instance", "b/1", "--evict"]).unwrap();
    assert!(parsed.evict);
    assert_eq!(
        parsed.command,
        Command::InstanceLifecycle {
            action: LifecycleAction::Start,
            deployment: "b".into(),
            instance: 1
        }
    );
    assert!(
        !parse_invocation(["capyctl", "start", "deployment", "b"])
            .unwrap()
            .evict
    );
    for refused in [
        vec!["capyctl", "stop", "deployment", "b", "--evict"],
        vec!["capyctl", "park", "deployment", "b", "--evict"],
        vec!["capyctl", "deploy", "model", "--activate", "--evict"],
    ] {
        assert!(parse_invocation(refused.clone()).is_err(), "{refused:?}");
    }
    let help = match parse_invocation(["capyctl", "start", "deployment", "--help"]) {
        Err(capyctl_cli::grammar::CliError::Clap(error)) => error.to_string(),
        Ok(_) => panic!("help is not an invocation"),
    };
    assert!(help.contains("--evict"), "{help}");
}

fn cli(state: &std::path::Path, args: &[&str]) -> Value {
    let mut command = capyctl();
    command.env("CAPYCTL_STATE_DIR", state).args(args);
    // `--format` and `--json` conflict; a caller that names a format keeps it.
    if !args.contains(&"--format") {
        command.arg("--json");
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

// T09 T10 T15: the shipped CLI sends `--evict` through the authenticated
// management API; with room on the host nothing is released and the receipt
// says so. The body, `evict` included, is journaled before it is sent, so a
// rerun by request id replays the same command and receives the same start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_evict_reports_victims_and_replays_by_request_id() {
    let dir = support::safe_state_dir();
    let app = support::boot(dir.path()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    std::env::set_var(capyctl_cli::roles::MANAGEMENT_ADDR_ENV, address.to_string());
    let router = app.management_router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let config = capyctl_cli::standalone_config::deployment_document(
        "evict-model",
        "evict-model",
        &ModelSource::Local {
            path: "/models/evict-model".into(),
        },
        Engine::Vllm,
        // The capacity the app was booted with (`support::test_memory`), not
        // this machine's: the deployment must be sized against the same host.
        &capyctl_cli::standalone_config::TemplateMemory::Unified {
            capacity_bytes: support::TEST_CAPACITY_BYTES,
        },
        capyctl_cli::standalone_config::DEFAULT_REQUEST_DEADLINE,
        // ADR 0012: the Fake host opts out of deep parking, so its
        // generated deployment is restart_only.
        false,
        "local",
    )
    .expect("the unified template");
    let path = dir.path().join("deployment.json");
    std::fs::write(&path, config.to_string()).unwrap();
    let deployed = cli(
        dir.path(),
        &["deploy", "model", "--file", path.to_str().unwrap()],
    );
    let id = deployed["deployment_id"].as_str().unwrap().to_owned();
    let request = ulid::Ulid::new().to_string();
    let args = [
        "start",
        "deployment",
        &id,
        "--evict",
        "--request-id",
        &request,
    ];
    let receipt = cli(dir.path(), &args);
    assert!(receipt["operation_id"].is_string(), "{receipt}");
    assert_eq!(receipt["victims"], serde_json::json!([]), "{receipt}");
    let replay = cli(dir.path(), &args);
    assert_eq!(replay["operation_id"], receipt["operation_id"]);
    // The journal holds the exact intent and body, `evict` included.
    let journaled: Value = serde_json::from_str(
        &std::fs::read_to_string(
            dir.path()
                .join("requests")
                .join(&request)
                .join("request.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(journaled["intent"]["evict"], true, "{journaled}");
    assert_eq!(
        journaled["mutations"]["action"]["body"]["evict"], true,
        "{journaled}"
    );
    // A different intent under the same request id is refused.
    let reused = capyctl()
        .env("CAPYCTL_STATE_DIR", dir.path())
        .args(["start", "deployment", &id, "--request-id", &request])
        .output()
        .unwrap();
    assert!(!reused.status.success());
    server.abort();
}

// T19 (SPEC §10 step 1, §16.2; W10 gap b): the router's waiting-request bounds
// are the embedded host's published `resource_policy.queue`, not built-in
// defaults: the standalone policy's 1800 s request deadline bounds a wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_waiting_bounds_follow_the_host_queue_policy() {
    let dir = support::safe_state_dir();
    let app = support::boot(dir.path()).await;
    let limits = app.deps().inflight.waiting.limits();
    assert_eq!(limits.deadline, std::time::Duration::from_secs(1800));
    assert_eq!(limits.max_pending_per_deployment, 64);
    assert_eq!(limits.max_pending_total, 256);
    assert_eq!(limits.max_buffered_bytes_total, 64 << 20);
    assert_eq!(limits.stream_idle, std::time::Duration::from_secs(120));
}

// T19 (SPEC §10, §16.2; owner rule 2026-09-25: standalone is a server and one
// host, every setting three ways; found live 2026-10-02): the standalone
// document's `host.resource_policy.queue` bounds the router as a host's does,
// and `--set` and `CAPYCTL_SET__…` override it (`--set` > environment > YAML).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_queue_bounds_follow_the_document_and_its_overrides() {
    use capyctl_cli::roles::SettingOverrides;
    use capyctl_config::ConfigKind;
    let dir = support::safe_state_dir();
    let config = dir.path().join("standalone.yaml");
    std::fs::write(
        &config,
        "schema_version: 1\nkind: standalone\nname: s\nhost:\n  resource_policy:\n    queue:\n      stream_idle_timeout: 300s\n      request_deadline: 900s\n",
    )
    .unwrap();
    // One port range for every boot, so the restarts below keep the stored
    // policy's shape and only its queue bounds change.
    let ports = support::engine_ports();
    let app = support::boot_with_overrides_on(
        dir.path(),
        Some(&config),
        &SettingOverrides::none(ConfigKind::Standalone),
        ports,
    )
    .await
    .expect("standalone boots with its queue bounds");
    let limits = app.deps().inflight.waiting.limits();
    assert_eq!(limits.stream_idle, std::time::Duration::from_secs(300));
    assert_eq!(limits.deadline, std::time::Duration::from_secs(900));
    // Unstated bounds keep the standalone defaults.
    assert_eq!(limits.max_pending_per_deployment, 64);
    let _ = app.shutdown().await;
    let overrides = SettingOverrides::parse(
        ConfigKind::Standalone,
        &["host.resource_policy.queue.stream_idle_timeout=600s".to_owned()],
        &[(
            "CAPYCTL_SET__HOST__RESOURCE_POLICY__QUEUE__STREAM_IDLE_TIMEOUT".to_owned(),
            "450s".to_owned(),
        )],
    )
    .unwrap();
    // A restart applies the changed bounds to the stored policy.
    let app = support::boot_with_overrides_on(dir.path(), Some(&config), &overrides, ports)
        .await
        .expect("standalone boots with its overrides");
    let limits = app.deps().inflight.waiting.limits();
    assert_eq!(limits.stream_idle, std::time::Duration::from_secs(600));
    assert_eq!(limits.deadline, std::time::Duration::from_secs(900));
    let _ = app.shutdown().await;
    // A bound outside the host's range is refused at boot, as on a host.
    let refused = SettingOverrides::parse(
        ConfigKind::Standalone,
        &["host.resource_policy.queue.stream_idle_timeout=999ms".to_owned()],
        &[],
    )
    .unwrap();
    let error =
        match support::boot_with_overrides_on(dir.path(), Some(&config), &refused, ports).await {
            Ok(_) => panic!("an out-of-range stream idle bound booted"),
            Err(error) => error.to_string(),
        };
    assert!(error.contains("resource_policy"), "{error}");
}

// T03 T19 (owner decision 2026-10-03: standalone is a server and one host,
// every setting three ways; found live 2026-10-03, the fixed 50 % limit
// refused an 84 GiB declaration): the standalone document's
// `host.resource_policy.memory.system` limits replace the derived ones in the
// stored policy, `--set` and `CAPYCTL_SET__…` override them, a restart applies
// a change, and `auto` returns to the derived default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_memory_limits_follow_the_document_and_its_overrides() {
    use capyctl_cli::roles::SettingOverrides;
    use capyctl_config::ConfigKind;
    const CAPACITY: i64 = support::TEST_CAPACITY_BYTES;
    let dir = support::safe_state_dir();
    let config = dir.path().join("standalone.yaml");
    std::fs::write(
        &config,
        "schema_version: 1\nkind: standalone\nname: s\nhost:\n  resource_policy:\n    memory:\n      system:\n        managed_limit: 70%\n        free_reserve: auto\n",
    )
    .unwrap();
    let ports = support::engine_ports();
    let stored = |app: &capyctl_cli::roles::App| {
        let policy = app
            .store
            .resource_policy("standalone")
            .unwrap()
            .expect("the embedded host published its policy");
        let domain = &policy.controls.domains["unified"];
        (domain.managed_limit, domain.free_reserve)
    };
    let app = support::boot_with_overrides_on(
        dir.path(),
        Some(&config),
        &SettingOverrides::none(ConfigKind::Standalone),
        ports,
    )
    .await
    .expect("standalone boots with its stated memory limit");
    assert_eq!(stored(&app), (CAPACITY / 100 * 70, CAPACITY / 100 * 20));
    let _ = app.shutdown().await;

    // A restart applies `--set` over the environment over the document.
    let overrides = SettingOverrides::parse(
        ConfigKind::Standalone,
        &["host.resource_policy.memory.system.managed_limit=20GiB".to_owned()],
        &[
            (
                "CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__MANAGED_LIMIT".to_owned(),
                "60%".to_owned(),
            ),
            (
                "CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__FREE_RESERVE".to_owned(),
                "4GiB".to_owned(),
            ),
        ],
    )
    .unwrap();
    let app = support::boot_with_overrides_on(dir.path(), Some(&config), &overrides, ports)
        .await
        .expect("standalone boots with its overrides");
    assert_eq!(stored(&app), (20 << 30, 4 << 30));
    let _ = app.shutdown().await;

    // Limits that do not fit the memory together are refused at boot.
    let refused = SettingOverrides::parse(
        ConfigKind::Standalone,
        &["host.resource_policy.memory.system.managed_limit=30GiB".to_owned()],
        &[],
    )
    .unwrap();
    let error =
        match support::boot_with_overrides_on(dir.path(), Some(&config), &refused, ports).await {
            Ok(_) => panic!("a managed limit beyond the memory booted"),
            Err(error) => error.to_string(),
        };
    assert!(
        error.contains("host.resource_policy.memory.system"),
        "{error}"
    );

    // `auto` returns to the derived default (50 %).
    let auto = SettingOverrides::parse(
        ConfigKind::Standalone,
        &["host.resource_policy.memory.system.managed_limit=auto".to_owned()],
        &[],
    )
    .unwrap();
    let app = support::boot_with_overrides_on(dir.path(), Some(&config), &auto, ports)
        .await
        .expect("standalone boots with the derived limit");
    assert_eq!(stored(&app), (CAPACITY / 100 * 50, CAPACITY / 100 * 20));
    let _ = app.shutdown().await;
}

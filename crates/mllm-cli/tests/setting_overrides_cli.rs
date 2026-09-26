//! Owner decision 2026-09-25: every YAML setting of a role is settable three
//! ways. `--set path=value` on the role starts, `validate config` and `config
//! show`; `MLLM_SET__PATH=value` in the environment; `--set` over
//! `MLLM_SET__…` over the named flag and variable over YAML over the default,
//! a named form and a generic override of one setting agreeing or refusing
//! the start. `config show` prints the effective configuration with each
//! value's source.
//!
//! The start tests here stop at the refusal, before any side effect, and the
//! positive boot runs the Fake installation: nothing here qualifies an engine
//! recipe (SPEC §18).

mod support;

use std::path::Path;
use std::process::Output;

use serde_json::Value;

fn repo(path: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
        .to_string_lossy()
        .into_owned()
}

/// `mllm <args>` with no MLLM_* variable of the developer's environment.
fn mllm(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = support::mllm();
    for (key, _) in std::env::vars() {
        if key.starts_with("MLLM_") || key == "HF_ENDPOINT" {
            command.env_remove(key);
        }
    }
    command.args(args).envs(env.iter().copied());
    command.output().unwrap()
}

fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
}

fn setting<'a>(shown: &'a Value, path: &str) -> &'a Value {
    shown["settings"]
        .as_array()
        .and_then(|settings| settings.iter().find(|s| s["path"] == path))
        .unwrap_or_else(|| panic!("{path} is not shown: {shown}"))
}

// T03 (owner decision 2026-09-25): `validate config --set` validates the
// document with the setting changed, for every role, and lists the
// overrides; `MLLM_SET__…` applies too.
#[test]
fn validate_config_applies_set_and_env_overrides() {
    for (file, set) in [
        (
            "docs/examples/server.yaml",
            "control.heartbeat_suspend_after=6s",
        ),
        ("docs/examples/host.yaml", "load_report_interval=2s"),
        (
            "docs/examples/standalone.yaml",
            "server.switching.drain_timeout=12s",
        ),
    ] {
        let out = mllm(
            &[
                "validate",
                "config",
                "--file",
                &repo(file),
                "--set",
                set,
                "--json",
            ],
            &[("MLLM_SET__SHUTDOWN__DRAIN_TIMEOUT", "45s")],
        );
        assert!(out.status.success(), "{file}: {}", said(&out));
        let value = json(&out);
        let applied = value["overrides"].as_array().unwrap();
        let path = set.split_once('=').unwrap().0;
        assert!(
            applied
                .iter()
                .any(|o| o["path"] == path && o["source"] == "set"),
            "{value}"
        );
        assert!(
            applied
                .iter()
                .any(|o| o["path"] == "shutdown.drain_timeout" && o["source"] == "env"),
            "{value}"
        );
    }
}

// T03 (SPEC §15.3): a value of the wrong type, one the role's range refuses,
// an unknown path (with the nearest valid ones) and a secret on the command
// line are refused, naming the override.
#[test]
fn validate_config_refuses_bad_overrides() {
    let server = repo("docs/examples/server.yaml");
    let host = repo("docs/examples/host.yaml");
    for (file, set, expected) in [
        (
            &server,
            "shutdown.drain_timeout=45",
            "set by --set shutdown.drain_timeout",
        ),
        (&server, "shutdown.drain_timeout=601s", "shutdown"),
        (
            &server,
            "shutdown.drain_timout=45s",
            "shutdown.drain_timeout",
        ),
        (&host, "load_report_intervl=1s", "load_report_interval"),
        (
            &host,
            "model_sources.huggingface_token_file=/etc/token",
            "secret",
        ),
    ] {
        let out = mllm(&["validate", "config", "--file", file, "--set", set], &[]);
        assert_eq!(out.status.code(), Some(2), "{set}: {}", said(&out));
        assert!(said(&out).contains(expected), "{set}: {}", said(&out));
    }
    // A secret may come from the environment.
    let out = mllm(
        &["validate", "config", "--file", &host],
        &[(
            "MLLM_SET__MODEL_SOURCES__HUGGINGFACE_TOKEN_FILE",
            "/etc/mllm/hf-token",
        )],
    );
    assert!(out.status.success(), "{}", said(&out));
}

// T03 (owner decision 2026-09-25): `config show` prints each effective value
// with its source, as a table by default and as JSON with --format json.
#[test]
fn config_show_names_each_values_source() {
    let state = tempfile::tempdir().unwrap();
    let root = state.path().to_str().unwrap();
    let args = [
        "--state-dir",
        root,
        "config",
        "show",
        "--set",
        "server.switching.drain_timeout=12s",
    ];
    let env = [
        ("MLLM_SET__SHUTDOWN__DRAIN_TIMEOUT", "45s"),
        ("MLLM_DEEP_PARK", "off"),
    ];
    let mut with_json = args.to_vec();
    with_json.extend(["--format", "json"]);
    let out = mllm(&with_json, &env);
    assert!(out.status.success(), "{}", said(&out));
    let shown = json(&out);
    assert_eq!(shown["role"], "standalone");
    assert_eq!(shown["document"], Value::Null);
    for (path, value, source) in [
        ("server.switching.drain_timeout", "12s", "set"),
        ("shutdown.drain_timeout", "45s", "env"),
        ("host.local_engine.deep_park", "off", "env"),
        ("state_dir", root, "flag"),
        (
            "server.listeners.management.bind",
            "127.0.0.1:7443",
            "default",
        ),
        ("host.model_sources.max_bytes", "500GiB", "default"),
    ] {
        let found = setting(&shown, path);
        assert_eq!(
            (found["value"].as_str(), found["source"].as_str()),
            (Some(value), Some(source)),
            "{path}"
        );
    }
    // The same, as a table.
    let out = mllm(&args, &env);
    assert!(out.status.success(), "{}", said(&out));
    let table = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        table.starts_with("standalone (no document; defaults)\n"),
        "{table}"
    );
    assert!(table.contains("SETTING"), "{table}");
    let row = table
        .lines()
        .find(|line| line.starts_with("server.switching.drain_timeout "))
        .unwrap();
    assert!(
        row.contains("12s") && row.trim_end().ends_with("set"),
        "{row}"
    );
    // A named document: its values are `yaml`.
    let out = mllm(
        &[
            "--config",
            &repo("docs/examples/host.yaml"),
            "config",
            "show",
            "--json",
        ],
        &[],
    );
    assert!(out.status.success(), "{}", said(&out));
    let shown = json(&out);
    assert_eq!(shown["role"], "host");
    assert_eq!(setting(&shown, "model_store.path")["source"], "yaml");
    assert_eq!(
        setting(&shown, "local_engine.deep_park")["source"],
        "default"
    );
}

// T03 (owner decision 2026-09-25): a named flag or variable and a generic
// override of the same setting must agree, or the start is refused before any
// side effect.
#[test]
fn a_named_form_and_a_generic_override_that_disagree_refuse_the_start() {
    let state = support::safe_state_dir();
    let root = state.path().to_str().unwrap();
    type Case<'a> = (&'a [&'a str], &'a [(&'a str, &'a str)], &'a str);
    let cases: [Case; 5] = [
        (
            &[
                "start",
                "standalone",
                "--deep-park",
                "on",
                "--set",
                "host.local_engine.deep_park=off",
            ],
            &[],
            "--deep-park",
        ),
        (
            &["start", "standalone"],
            &[
                ("MLLM_ENGINE_PORTS", "9000-9099"),
                (
                    "MLLM_SET__HOST__RESOURCE_POLICY__ENDPOINT_PORT_RANGE__START",
                    "9100",
                ),
            ],
            "MLLM_ENGINE_PORTS",
        ),
        (
            &[
                "start",
                "standalone",
                "--management-listen",
                "127.0.0.1:7601",
                "--set",
                "server.listeners.management.bind=127.0.0.1:7602",
            ],
            &[],
            "--management-listen",
        ),
        (
            &[
                "start",
                "server",
                "--listen",
                "127.0.0.1:9443",
                "--set",
                "listeners.inference.bind=127.0.0.1:9444",
            ],
            &[],
            "--listen",
        ),
        (
            &[
                "start",
                "host",
                "--runtime-dir",
                "/opt/a",
                "--set",
                "runtime_dir=/opt/b",
            ],
            &[],
            "--runtime-dir",
        ),
    ];
    for (args, env, named) in cases {
        let mut full = vec!["--state-dir", root];
        full.extend_from_slice(args);
        let out = mllm(&full, env);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", said(&out));
        let text = said(&out);
        assert!(
            text.contains("conflict") && text.contains(named),
            "{args:?}: {text}"
        );
    }
    // Nothing was created under the state root.
    assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
}

// T03: an unknown path or a secret refuses a role start with its reason.
#[test]
fn a_bad_override_refuses_a_role_start() {
    let state = support::safe_state_dir();
    let root = state.path().to_str().unwrap();
    for (role, set, expected) in [
        (
            "standalone",
            "shutdown.drain_timout=5s",
            "shutdown.drain_timeout",
        ),
        (
            "standalone",
            "host.model_sources.huggingface_token_file=/x",
            "secret",
        ),
        ("standalone", "shutdown.drain_timeout=601s", "shutdown"),
        ("host", "load_report_intervall=1s", "load_report_interval"),
        (
            "server",
            "control.heartbeat_suspend_afte=5s",
            "control.heartbeat_suspend_after",
        ),
    ] {
        let out = mllm(&["--state-dir", root, "start", role, "--set", set], &[]);
        assert_ne!(out.status.code(), Some(0), "{set}");
        assert!(said(&out).contains(expected), "{set}: {}", said(&out));
    }
}

// T03 T37 (owner decision 2026-09-25): the standalone listeners and the
// switch drain bound follow the overrides at boot, `--set` over
// `MLLM_SET__…` over the document; management stays on loopback.
#[tokio::test(flavor = "current_thread")]
async fn a_standalone_boot_honours_its_overrides() {
    use mllm_cli::roles::SettingOverrides;
    use mllm_config::ConfigKind;
    let state = support::safe_state_dir();
    let overrides = SettingOverrides::parse(
        ConfigKind::Standalone,
        &["server.listeners.management.bind=127.0.0.1:7611".to_owned()],
        &[
            (
                "MLLM_SET__SERVER__LISTENERS__MANAGEMENT__BIND".to_owned(),
                "127.0.0.1:7612".to_owned(),
            ),
            (
                "MLLM_SET__SERVER__LISTENERS__INFERENCE__BIND".to_owned(),
                "127.0.0.1:8611".to_owned(),
            ),
        ],
    )
    .unwrap();
    let app = support::boot_with_overrides(state.path(), None, &overrides)
        .await
        .expect("standalone boots with overrides");
    assert_eq!(app.management_bind().to_string(), "127.0.0.1:7611");
    assert_eq!(app.inference_bind().to_string(), "127.0.0.1:8611");
    let _ = app.shutdown().await;
    // A management address off loopback is refused like the YAML value.
    let refused = SettingOverrides::parse(
        ConfigKind::Standalone,
        &["server.listeners.management.bind=0.0.0.0:7611".to_owned()],
        &[],
    )
    .unwrap();
    let error = match support::boot_with_overrides(state.path(), None, &refused).await {
        Ok(_) => panic!("a public management address booted"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("server.listeners.management.bind"),
        "{error}"
    );
}

// T03 (owner decision 2026-09-25): a client command finds the standalone
// management address the document states, so moving it in YAML moves the
// clients too; `MLLM_MANAGEMENT_ADDR` still wins (checked in the binary).
#[test]
fn clients_find_the_management_address_in_the_standalone_document() {
    let state = tempfile::tempdir().unwrap();
    let config = state.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("standalone.yaml"),
        "schema_version: 1\nkind: standalone\nname: s\nserver:\n  listeners:\n    management:\n      bind: \"127.0.0.1:7621\"\n",
    )
    .unwrap();
    if std::env::var_os(mllm_cli::roles::MANAGEMENT_ADDR_ENV).is_none()
        && std::env::var_os(mllm_cli::roles::DEPRECATED_MANAGEMENT_ADDR_ENV).is_none()
    {
        assert_eq!(
            mllm_cli::roles::standalone_management_address(state.path())
                .unwrap()
                .to_string(),
            "127.0.0.1:7621"
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            mllm_cli::roles::standalone_management_address(empty.path())
                .unwrap()
                .to_string(),
            "127.0.0.1:7443"
        );
    }
    // Through the binary: the variable wins over the document, and a bad
    // one is refused with its name.
    std::fs::create_dir_all(state.path().join("identity")).unwrap();
    std::fs::write(
        state.path().join("identity/credentials"),
        "admin_token: test\n",
    )
    .unwrap();
    let out = mllm(
        &[
            "--state-dir",
            state.path().to_str().unwrap(),
            "list",
            "deployments",
        ],
        &[("MLLM_MANAGEMENT_ADDR", "0.0.0.0:7621")],
    );
    assert!(!out.status.success());
    assert!(
        said(&out).contains("MLLM_MANAGEMENT_ADDR"),
        "{}",
        said(&out)
    );
}

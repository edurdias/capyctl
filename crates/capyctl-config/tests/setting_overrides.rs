//! Owner decision 2026-09-25: every YAML setting of a role is settable three
//! ways. These tables drive settings that have no named flag or variable
//! through the generic override on every role, and read them back through the
//! role's own parser, so the override is exactly the YAML value.

use std::path::Path;
use std::time::Duration;

use capyctl_config::remote_roles::{
    drain_timeout, switch_drain_timeout, timing_header, HostConfig, ServerConfig,
};
use capyctl_config::setting_overrides::SettingOverrides;
use capyctl_config::ConfigKind;
use serde_json::{json, Value};

type Reader = fn(&Value) -> String;

struct Row {
    kind: ConfigKind,
    path: &'static str,
    yaml: &'static str,
    env: &'static str,
    set: &'static str,
    read: Reader,
    /// What the role reads with nothing stated, with the YAML value, with
    /// the YAML and the variable, and with all three.
    expected: [&'static str; 4],
}

fn base(kind: ConfigKind) -> Value {
    let root = Path::new("/srv/capyctl");
    match kind {
        ConfigKind::Server => serde_json::from_str(&ServerConfig::template(root)).unwrap(),
        ConfigKind::Host => serde_json::from_str(&HostConfig::template(root)).unwrap(),
        _ => json!({"schema_version": 1, "kind": "standalone", "name": "local"}),
    }
}

fn server(document: &Value) -> ServerConfig {
    ServerConfig::parse(&document.to_string()).unwrap()
}

fn host(document: &Value) -> HostConfig {
    HostConfig::parse(&document.to_string()).unwrap()
}

fn secs(duration: Duration) -> String {
    format!("{}ms", duration.as_millis())
}

fn env_name(path: &str) -> String {
    format!("CAPYCTL_SET__{}", path.replace('.', "__").to_ascii_uppercase())
}

// T03 (owner decision 2026-09-25, SPEC §15.2): for settings stated only in
// the document until now, `--set` > `CAPYCTL_SET__…` > YAML > default on every
// role, read back through the role's own parser.
#[test]
fn document_only_settings_follow_set_env_yaml_default() {
    let rows = [
        Row {
            kind: ConfigKind::Server,
            path: "shutdown.drain_timeout",
            yaml: "40s",
            env: "50s",
            set: "60s",
            read: |d| secs(server(d).drain_timeout),
            expected: ["30000ms", "40000ms", "50000ms", "60000ms"],
        },
        Row {
            kind: ConfigKind::Server,
            path: "control.heartbeat_suspend_after",
            yaml: "6s",
            env: "7s",
            set: "8s",
            read: |d| secs(server(d).heartbeat.suspend_after),
            expected: ["5000ms", "6000ms", "7000ms", "8000ms"],
        },
        Row {
            kind: ConfigKind::Server,
            path: "lifecycle_defaults.ready_idle_timeout",
            yaml: "10m",
            env: "20m",
            set: "30m",
            read: |d| format!("{:?}", server(d).idle.ready_idle),
            expected: ["None", "Some(600s)", "Some(1200s)", "Some(1800s)"],
        },
        Row {
            kind: ConfigKind::Server,
            path: "switching.drain_timeout",
            yaml: "11s",
            env: "12s",
            set: "13s",
            read: |d| secs(server(d).switch_drain_timeout),
            expected: ["30000ms", "11000ms", "12000ms", "13000ms"],
        },
        Row {
            kind: ConfigKind::Host,
            path: "load_report_interval",
            yaml: "2s",
            env: "3s",
            set: "4s",
            read: |d| secs(host(d).load_report_interval),
            expected: ["1000ms", "2000ms", "3000ms", "4000ms"],
        },
        Row {
            kind: ConfigKind::Host,
            path: "shutdown.drain_timeout",
            yaml: "5s",
            env: "6s",
            set: "7s",
            read: |d| secs(host(d).drain_timeout),
            expected: ["30000ms", "5000ms", "6000ms", "7000ms"],
        },
        Row {
            kind: ConfigKind::Standalone,
            path: "shutdown.drain_timeout",
            yaml: "5s",
            env: "6s",
            set: "7s",
            read: |d| secs(drain_timeout(d).unwrap()),
            expected: ["30000ms", "5000ms", "6000ms", "7000ms"],
        },
        Row {
            kind: ConfigKind::Standalone,
            path: "server.switching.drain_timeout",
            yaml: "5s",
            env: "6s",
            set: "7s",
            read: |d| secs(switch_drain_timeout(&d["server"]).unwrap()),
            expected: ["30000ms", "5000ms", "6000ms", "7000ms"],
        },
        Row {
            kind: ConfigKind::Standalone,
            path: "server.observability.timing_header",
            yaml: "false",
            env: "true",
            set: "false",
            read: |d| timing_header(&d["server"]).unwrap().to_string(),
            expected: ["false", "false", "true", "false"],
        },
    ];
    for row in rows {
        let yaml = SettingOverrides::parse(row.kind, &[format!("{}={}", row.path, row.yaml)], &[])
            .unwrap();
        let mut stated = base(row.kind);
        yaml.apply(&mut stated).unwrap();
        let env = [(env_name(row.path), row.env.to_owned())];
        let layers = [
            (base(row.kind), SettingOverrides::none(row.kind)),
            (stated.clone(), SettingOverrides::none(row.kind)),
            (
                stated.clone(),
                SettingOverrides::parse(row.kind, &[], &env).unwrap(),
            ),
            (
                stated,
                SettingOverrides::parse(row.kind, &[format!("{}={}", row.path, row.set)], &env)
                    .unwrap(),
            ),
        ];
        let seen = layers.map(|(document, overrides)| {
            (row.read)(&overrides.apply_and_validate(document).unwrap())
        });
        assert_eq!(seen, row.expected.map(str::to_owned), "{}", row.path);
    }
}

// T03 (SPEC §15.3): the role's own range checks apply to an override exactly
// as to the YAML value.
#[test]
fn an_override_outside_the_roles_range_is_refused_by_the_role() {
    for (kind, set) in [
        (ConfigKind::Server, "shutdown.drain_timeout=601s"),
        (ConfigKind::Server, "control.heartbeat_suspend_after=1s"),
        (ConfigKind::Host, "load_report_interval=10s"),
    ] {
        let overrides = SettingOverrides::parse(kind, &[set.to_owned()], &[]).unwrap();
        let document = overrides.apply_and_validate(base(kind)).unwrap();
        let refused = match kind {
            ConfigKind::Server => ServerConfig::parse(&document.to_string()).is_err(),
            _ => HostConfig::parse(&document.to_string()).is_err(),
        };
        assert!(refused, "{set}");
    }
}

// T03 (ADR 0018 §3, owner decision 2026-09-25): a host loads its document
// with the run's overrides and keeps them, so a live reload applies them
// again.
#[test]
fn a_host_keeps_its_overrides_for_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let document = dir.path().join("host.yaml");
    std::fs::write(&document, HostConfig::template(dir.path())).unwrap();
    let overrides = SettingOverrides::parse(
        ConfigKind::Host,
        &["load_report_interval=2s".to_owned()],
        &[],
    )
    .unwrap();
    let config =
        HostConfig::load_with_overrides(&document, &dir.path().join("engines.yaml"), &overrides)
            .unwrap()
            .with_engines(&Default::default(), &Default::default(), &|_| {
                Ok("fp".into())
            })
            .unwrap();
    assert_eq!(config.load_report_interval, Duration::from_secs(2));
    assert_eq!(config.overrides, overrides);
    let reloaded = HostConfig::load_with_overrides(
        &document,
        &dir.path().join("engines.yaml"),
        &config.overrides,
    )
    .unwrap();
    assert_eq!(reloaded.document, config.document);
}

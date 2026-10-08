//! Owner decision 2026-09-25: every YAML setting of a role is settable three
//! ways. These tables drive settings that have no named flag or variable
//! through the generic override on every role, and read them back through the
//! role's own parser, so the override is exactly the YAML value.

use std::path::Path;
use std::time::Duration;

use capyctl_config::remote_roles::{
    drain_timeout, group_stall_timeout_value, host_activation, idle_timeouts, switch_drain_timeout,
    timing_header, HostConfig, ServerConfig,
};
use capyctl_config::setting_overrides::{
    defaults, named_forms, NamedLayer, SettingOverrides, Source,
};
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
    format!(
        "CAPYCTL_SET__{}",
        path.replace('.', "__").to_ascii_uppercase()
    )
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
        // SPEC §6.5: the host's activation policy, `on_demand` unless
        // stated (YAML-only family: `--set` and `CAPYCTL_SET__…`).
        Row {
            kind: ConfigKind::Host,
            path: "lifecycle.activation",
            yaml: "explicit",
            env: "on_demand",
            set: "explicit",
            read: |d| format!("{:?}", host(d).activation),
            expected: ["OnDemand", "Explicit", "OnDemand", "Explicit"],
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
        // Owner rule (standalone is a server and one host): the server's idle
        // policy applies to standalone too.
        Row {
            kind: ConfigKind::Standalone,
            path: "server.lifecycle_defaults.ready_idle_timeout",
            yaml: "10m",
            env: "20m",
            set: "30m",
            read: |d| format!("{:?}", idle_timeouts(&d["server"]).unwrap().ready_idle),
            expected: ["None", "Some(600s)", "Some(1200s)", "Some(1800s)"],
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
        // Owner rule (standalone is a server and one host): the embedded
        // host's activation policy, as on a host.
        Row {
            kind: ConfigKind::Standalone,
            path: "host.lifecycle.activation",
            yaml: "explicit",
            env: "on_demand",
            set: "explicit",
            read: |d| format!("{:?}", host_activation(&d["host"]).unwrap()),
            expected: ["OnDemand", "Explicit", "OnDemand", "Explicit"],
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
            .with_engines(&Default::default(), &Default::default(), &|_, _| {
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

/// The server's settings as `start server` resolves them: the YAML `yaml`
/// stated in the server document, then `--group-stall-timeout` among `flags`
/// and the variables `env`.
fn try_server_settings(
    flags: &[&str],
    env: &[(&str, &str)],
    yaml: &str,
) -> Result<ServerConfig, capyctl_config::ConfigError> {
    let mut document = base(ConfigKind::Server);
    if !yaml.is_empty() {
        if let Value::Object(stated) = capyctl_config::parse_document(yaml)? {
            for (key, value) in stated {
                document[key] = value;
            }
        }
    }
    let flag = flags
        .windows(2)
        .find(|pair| pair[0] == "--group-stall-timeout")
        .map(|pair| group_stall_timeout_value("--group-stall-timeout", pair[1]))
        .transpose()?;
    let env: std::collections::BTreeMap<String, String> = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    ServerConfig::parse(&document.to_string())?
        .with_group_stall_timeout(flag, &|name| env.get(name).cloned())
}

fn server_settings(flags: &[&str], env: &[(&str, &str)], yaml: &str) -> ServerConfig {
    try_server_settings(flags, env, yaml).unwrap()
}

// T14 (decided 2026-10-06, ADR 0028 §11): the stall timeout comes three ways,
// flag > env > YAML > default.
#[test]
fn stall_timeout_three_ways() {
    assert_eq!(
        server_settings(&[], &[], "").groups.stall_timeout,
        Duration::from_secs(120)
    );
    assert_eq!(
        server_settings(&[], &[], "groups: {stall_timeout: 90s}")
            .groups
            .stall_timeout,
        Duration::from_secs(90)
    );
    assert_eq!(
        server_settings(
            &[],
            &[("CAPYCTL_GROUP_STALL_TIMEOUT", "60s")],
            "groups: {stall_timeout: 90s}"
        )
        .groups
        .stall_timeout,
        Duration::from_secs(60)
    );
    assert_eq!(
        server_settings(
            &["--group-stall-timeout", "30s"],
            &[("CAPYCTL_GROUP_STALL_TIMEOUT", "60s")],
            "groups: {stall_timeout: 90s}"
        )
        .groups
        .stall_timeout,
        Duration::from_secs(30)
    );
    // An empty variable counts as unset.
    assert_eq!(
        server_settings(
            &[],
            &[("CAPYCTL_GROUP_STALL_TIMEOUT", "")],
            "groups: {stall_timeout: 90s}"
        )
        .groups
        .stall_timeout,
        Duration::from_secs(90)
    );
}

// T14 (ADR 0028 §11): a zero, out-of-range or unreadable stall timeout is
// refused with the name of the form that stated it, never read as the default.
#[test]
fn a_zero_or_unreadable_stall_timeout_is_refused_by_its_name() {
    for bad in ["0s", "soon", "3601s"] {
        let error = try_server_settings(&[], &[], &format!("groups: {{stall_timeout: {bad}}}"))
            .unwrap_err();
        assert_eq!(error.path, "groups.stall_timeout", "{bad}: {error}");
        let error =
            try_server_settings(&[], &[("CAPYCTL_GROUP_STALL_TIMEOUT", bad)], "").unwrap_err();
        assert_eq!(error.path, "CAPYCTL_GROUP_STALL_TIMEOUT", "{bad}: {error}");
        let error = try_server_settings(&["--group-stall-timeout", bad], &[], "").unwrap_err();
        assert_eq!(error.path, "--group-stall-timeout", "{bad}: {error}");
    }
    // An unknown setting under `groups` is refused like any unknown field.
    assert!(try_server_settings(&[], &[], "groups: {stall: 90s}").is_err());
}

// T03 T14 (ADR 0028 §11): `groups.stall_timeout` is a named server setting:
// `config show` lists its default (`server.groups.stall_timeout` in a
// standalone document), names its flag and variable, and `--set` states it
// exactly as the YAML would.
#[test]
fn the_stall_timeout_is_a_named_server_setting() {
    let named = Some(("--group-stall-timeout", "CAPYCTL_GROUP_STALL_TIMEOUT"));
    assert_eq!(
        named_forms(ConfigKind::Server, "groups.stall_timeout"),
        named
    );
    assert_eq!(
        named_forms(ConfigKind::Standalone, "server.groups.stall_timeout"),
        named
    );
    let root = Path::new("/srv/capyctl");
    assert!(defaults(ConfigKind::Server, None, root)
        .contains(&("groups.stall_timeout".to_owned(), json!("120s"))));
    assert!(defaults(ConfigKind::Standalone, None, root)
        .contains(&("server.groups.stall_timeout".to_owned(), json!("120s"))));
    let env = NamedLayer {
        group_stall_timeout: Some("60s".into()),
        ..Default::default()
    };
    let stated = env.values(ConfigKind::Server, Source::Env);
    assert_eq!(stated.len(), 1);
    assert_eq!(
        (
            stated[0].path.as_str(),
            &stated[0].value,
            stated[0].origin.as_str()
        ),
        (
            "groups.stall_timeout",
            &json!("60s"),
            "CAPYCTL_GROUP_STALL_TIMEOUT"
        )
    );
    let overrides = SettingOverrides::parse(
        ConfigKind::Server,
        &["groups.stall_timeout=45s".to_owned()],
        &[],
    )
    .unwrap();
    let document = overrides
        .apply_and_validate(base(ConfigKind::Server))
        .unwrap();
    assert_eq!(
        server(&document).groups.stall_timeout,
        Duration::from_secs(45)
    );
}

use super::*;
use serde_json::json;

fn sets(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn env(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

// T03 (owner decision 2026-09-25): a value is typed as the same plain YAML
// scalar would be, a list is a YAML flow list, and a path matches the schema
// case-insensitively (variables are upper case).
#[test]
fn values_are_typed_as_yaml_and_paths_match_the_schema() {
    let overrides = SettingOverrides::parse(
        ConfigKind::Host,
        &sets(&[
            "shutdown.drain_timeout=45s",
            "local_engine.trust_remote_code=true",
            "resource_policy.endpoint_port_range.start=9100",
            "local_engine.args=[--enforce-eager, --max-num-seqs, 4]",
            "resource_policy.labels.zone=lab",
        ]),
        &env(&[
            ("CAPYCTL_SET__LOAD_REPORT_INTERVAL", "2s"),
            ("CAPYCTL_SET__RUNTIME_PROFILES__VLLM__ARGS", "[--x]"),
            ("UNRELATED", "1"),
        ]),
    )
    .unwrap();
    let values: Vec<(String, Value, Source)> = overrides
        .effective()
        .into_iter()
        .map(|item| (item.path.clone(), item.value.clone(), item.source))
        .collect();
    assert_eq!(
        values,
        vec![
            ("load_report_interval".into(), json!("2s"), Source::Env),
            (
                "local_engine.args".into(),
                json!(["--enforce-eager", "--max-num-seqs", 4]),
                Source::Set
            ),
            (
                "local_engine.trust_remote_code".into(),
                json!(true),
                Source::Set
            ),
            (
                "resource_policy.endpoint_port_range.start".into(),
                json!(9100),
                Source::Set
            ),
            (
                "resource_policy.labels.zone".into(),
                json!("lab"),
                Source::Set
            ),
            (
                "runtime_profiles.vllm.args".into(),
                json!(["--x"]),
                Source::Env
            ),
            ("shutdown.drain_timeout".into(), json!("45s"), Source::Set),
        ]
    );
    let mut document = json!({"schema_version": 1, "kind": "host", "name": "h",
        "resource_policy": {"endpoint_port_range": {"start": 8100, "end": 8199}}});
    overrides.apply(&mut document).unwrap();
    assert_eq!(document["shutdown"]["drain_timeout"], "45s");
    assert_eq!(
        document["resource_policy"]["endpoint_port_range"]["start"],
        9100
    );
    assert_eq!(
        document["resource_policy"]["endpoint_port_range"]["end"],
        8199
    );
    assert_eq!(document["runtime_profiles"]["vllm"]["args"], json!(["--x"]));
}

// T03: `--set` wins over `CAPYCTL_SET__…` for the same path, the last `--set`
// over an earlier one.
#[test]
fn the_command_line_wins_over_the_environment() {
    let overrides = SettingOverrides::parse(
        ConfigKind::Server,
        &sets(&["shutdown.drain_timeout=10s", "shutdown.drain_timeout=20s"]),
        &env(&[("CAPYCTL_SET__SHUTDOWN__DRAIN_TIMEOUT", "5s")]),
    )
    .unwrap();
    let item = overrides.get("shutdown.drain_timeout").unwrap();
    assert_eq!(
        (item.value.clone(), item.source),
        (json!("20s"), Source::Set)
    );
    assert_eq!(overrides.effective().len(), 1);
}

// T03 (SPEC §15.3): an override is validated exactly as YAML; the error at
// its path names it.
#[test]
fn an_override_is_validated_like_yaml() {
    let document = json!({"schema_version": 1, "kind": "server", "name": "s"});
    for (set, path) in [
        ("shutdown.drain_timeout=30", "shutdown.drain_timeout"),
        ("shutdown.drain_timeout=soon", "shutdown.drain_timeout"),
        (
            "lifecycle_defaults.ready_idle_timeout=10GiB",
            "lifecycle_defaults.ready_idle_timeout",
        ),
    ] {
        let overrides = SettingOverrides::parse(ConfigKind::Server, &sets(&[set]), &[]).unwrap();
        let error = overrides.apply_and_validate(document.clone()).unwrap_err();
        assert_eq!(error.path, path, "{set}");
        assert!(error.detail.contains("(set by --set"), "{error}");
    }
    let overrides = SettingOverrides::parse(
        ConfigKind::Server,
        &sets(&["shutdown.drain_timeout=45s"]),
        &[],
    )
    .unwrap();
    let valid = overrides.apply_and_validate(document).unwrap();
    assert_eq!(valid["shutdown"]["drain_timeout"], "45s");
}

// T03: an unknown path is refused with the valid paths nearest the typo.
#[test]
fn an_unknown_path_names_the_nearest_settings() {
    for (kind, given, near) in [
        (
            ConfigKind::Server,
            "shutdown.drain_timout=1s",
            "shutdown.drain_timeout",
        ),
        (
            ConfigKind::Host,
            "load_report_intervall=1s",
            "load_report_interval",
        ),
        (
            ConfigKind::Standalone,
            "server.switching.drain=1s",
            "server.switching.drain_timeout",
        ),
        (
            ConfigKind::Server,
            "listeners.inference.bnd=0.0.0.0:1",
            "listeners.inference.bind",
        ),
    ] {
        let error = SettingOverrides::parse(kind, &sets(&[given]), &[]).unwrap_err();
        assert_eq!(error.code, ConfigErrorCode::UnknownField, "{given}");
        assert!(error.detail.contains(near), "{given}: {error}");
    }
    let error = SettingOverrides::parse(
        ConfigKind::Host,
        &[],
        &env(&[("CAPYCTL_SET__LOAD__REPORT", "1s")]),
    )
    .unwrap_err();
    assert!(
        error.detail.contains("CAPYCTL_SET__LOAD__REPORT"),
        "{error}"
    );
    // A block is not a value, and the document's identity is not a setting.
    let error =
        SettingOverrides::parse(ConfigKind::Server, &sets(&["shutdown=1s"]), &[]).unwrap_err();
    assert!(error.detail.contains("shutdown.drain_timeout"), "{error}");
    assert!(SettingOverrides::parse(ConfigKind::Server, &sets(&["kind=host"]), &[]).is_err());
    assert!(
        SettingOverrides::parse(ConfigKind::Server, &sets(&["shutdown.drain_timeout"]), &[])
            .is_err()
    );
    assert!(
        SettingOverrides::parse(ConfigKind::Server, &sets(&["shutdown.drain_timeout="]), &[])
            .is_err()
    );
    assert!(
        SettingOverrides::parse(ConfigKind::Host, &sets(&["local_engine.args=--x"]), &[]).is_err()
    );
    // Only role documents take overrides.
    assert!(SettingOverrides::parse(ConfigKind::Deployment, &sets(&["name=x"]), &[]).is_err());
}

// T03 T21 (owner rule: a secret is never a flag): a secret-bearing setting is
// refused on the command line and accepted from the environment.
#[test]
fn a_secret_is_refused_on_the_command_line() {
    for set in [
        "model_sources.huggingface_token_file=/etc/capyctl/hf-token",
        "runtime_profiles.vllm.security.credential_ref=secret://k",
        "runtime_profiles.vllm.security.admin_credential_ref=secret://k",
        "runtime_profiles.vllm.env.HF_TOKEN=abc",
    ] {
        let error = SettingOverrides::parse(ConfigKind::Host, &sets(&[set]), &[]).unwrap_err();
        assert!(error.detail.contains("secret"), "{set}: {error}");
    }
    let overrides = SettingOverrides::parse(
        ConfigKind::Host,
        &[],
        &env(&[(
            "CAPYCTL_SET__MODEL_SOURCES__HUGGINGFACE_TOKEN_FILE",
            "/etc/capyctl/hf-token",
        )]),
    )
    .unwrap();
    assert!(overrides
        .get("model_sources.huggingface_token_file")
        .is_some());
    assert!(!is_secret("resource_policy.planner_max_states"));
    assert!(!is_secret("engine_config.sglang.max_total_tokens"));
}

// T03 (owner decision 2026-09-25): a named form and a generic override of the
// same setting must agree, whatever spelling each uses.
#[test]
fn a_named_form_and_a_generic_override_must_agree() {
    let overrides = SettingOverrides::parse(
        ConfigKind::Host,
        &sets(&["local_engine.deep_park=off", "model_sources.http=disabled"]),
        &env(&[("CAPYCTL_SET__LOCAL_ENGINE__TRUST_REMOTE_CODE", "on")]),
    )
    .unwrap();
    let named = |path: &str, value: Value, origin: &str| NamedValue {
        path: path.into(),
        value,
        source: Source::Flag,
        origin: origin.into(),
    };
    overrides
        .check_named(&[
            named("local_engine.deep_park", json!("off"), "--deep-park"),
            named("model_sources.http", json!("denied"), "--model-sources"),
            named(
                "local_engine.trust_remote_code",
                json!(true),
                "CAPYCTL_TRUST_REMOTE_CODE",
            ),
            named("runtime_dir", json!("/opt/r"), "--runtime-dir"),
        ])
        .unwrap();
    let error = overrides
        .check_named(&[named("local_engine.deep_park", json!("on"), "--deep-park")])
        .unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::ConflictingArgs);
    assert!(
        error.detail.contains("--set local_engine.deep_park")
            && error.detail.contains("--deep-park"),
        "{error}"
    );
}

// T03: every settable path is listed, a listener by its name.
#[test]
fn the_setting_paths_of_each_role() {
    let server = setting_paths(ConfigKind::Server);
    for path in [
        "shutdown.drain_timeout",
        "control.heartbeat_suspend_after",
        "lifecycle_defaults.ready_idle_timeout",
        "listeners.management.bind",
        "observability.timing_header",
    ] {
        assert!(server.contains(&path.to_owned()), "{path}");
    }
    assert!(!server.contains(&"kind".to_owned()));
    let standalone = setting_paths(ConfigKind::Standalone);
    assert!(standalone.contains(&"host.local_engine.cuda_home".to_owned()));
    assert!(standalone.contains(&"server.listeners.management.bind".to_owned()));
}

// T03 (final review I8, owner rule: standalone is a server and a host in one
// process): the management address and the state directory have the same
// named flag and variable on every role that has them.
#[test]
fn management_address_and_state_dir_are_named_alike_on_every_role() {
    let management = Some(("--management-listen", "CAPYCTL_MANAGEMENT_ADDR"));
    assert_eq!(
        named_forms(ConfigKind::Server, "listeners.management.bind"),
        management
    );
    assert_eq!(
        named_forms(ConfigKind::Standalone, "server.listeners.management.bind"),
        management
    );
    let state = Some(("--state-dir", "CAPYCTL_STATE_DIR"));
    for kind in [ConfigKind::Server, ConfigKind::Host, ConfigKind::Standalone] {
        assert_eq!(named_forms(kind, "state_dir"), state, "{kind:?}");
    }
}

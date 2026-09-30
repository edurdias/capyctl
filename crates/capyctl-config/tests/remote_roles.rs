use capyctl_config::remote_roles::{HostConfig, ServerConfig};
use std::path::Path;

// T01, T02, T07: safe role templates require no engine installation.
#[test]
fn role_templates_round_trip_without_engines() {
    let root = Path::new("/home/operator/.local/state/capyctl");
    let server = ServerConfig::parse(&ServerConfig::template(root)).unwrap();
    assert!(server.management.ip().is_loopback());
    // Design §9 (owner decision 5): the server's generated inference
    // listener serves every interface, with the API key.
    assert_eq!(server.inference.to_string(), "0.0.0.0:8443");
    assert!(server.bootstrap.ip().is_loopback());
    assert!(server.control.ip().is_loopback());
    assert_eq!(server.state_dir, root);
    let host = HostConfig::parse(&HostConfig::template(root)).unwrap();
    assert!(host.profiles.is_empty());
    assert_eq!(host.state_dir, root);
}

// T03, T37: reject malformed authority before creating any files.
#[test]
fn role_config_rejects_unknown_duplicate_and_unsafe_transport() {
    let yaml = ServerConfig::template(Path::new("/home/operator/state"));
    for bad in [
        format!("{yaml}\nsecret: wrong\n"),
        format!("{yaml}\nname: duplicate\n"),
        yaml.replace("127.0.0.1:7443", "0.0.0.0:7443"),
        yaml.replace("https://", "http://"),
        yaml.replace("/home/operator/state", "relative"),
        yaml.replace("127.0.0.1:7445", "127.0.0.1:7444"),
    ] {
        assert!(
            ServerConfig::parse(&bad).is_err(),
            "accepted unsafe document"
        );
    }
}

// T03, T37: host ingress is absent by default and requires an explicit protected link.
#[test]
fn host_runtime_and_ingress_are_explicit_and_fail_closed() {
    let root = Path::new("/home/operator/host");
    let mut document: serde_json::Value =
        serde_json::from_str(&HostConfig::template(root)).unwrap();
    let config = HostConfig::parse(&document.to_string()).unwrap();
    assert_eq!(config.runtime_dir, root.join("runtime"));
    // SPEC §3.3 / ADR 0001: undeclared, it is the managed embedded runtime.
    assert!(!config.runtime_dir_declared);
    assert!(config.ingress.is_none());
    document["runtime_dir"] = "/home/operator/capyctl/runtime".into();
    document["ingress"] = serde_json::json!({"bind":"100.64.0.10:9443","address":"http://100.64.0.10:9443","transport":"trusted_private_link"});
    let config = HostConfig::parse(&document.to_string()).unwrap();
    assert_eq!(config.runtime_dir, Path::new("/home/operator/capyctl/runtime"));
    assert!(config.runtime_dir_declared);
    assert_eq!(config.ingress.unwrap().bind.port(), 9443);
    for address in [
        "http://0.0.0.0:9443",
        "http://8.8.8.8:9443",
        "http://192.168.4.36:9443",
        "http://100.64.0.10:0",
        "http://100.64.0.10:9443/admin",
    ] {
        document["ingress"]["address"] = address.into();
        assert!(HostConfig::parse(&document.to_string()).is_err());
    }
}

// T37: the host's load-report period is bounded role configuration: 1 s when
// omitted, 250 ms to 5 s when set, and never part of the resolution document.
#[test]
fn host_load_report_interval_defaults_and_is_bounded() {
    use std::time::Duration;
    let root = Path::new("/home/operator/host");
    let mut document: serde_json::Value =
        serde_json::from_str(&HostConfig::template(root)).unwrap();
    let config = HostConfig::parse(&document.to_string()).unwrap();
    assert_eq!(config.load_report_interval, Duration::from_secs(1));
    for (text, expected) in [
        ("250ms", Duration::from_millis(250)),
        ("2s", Duration::from_secs(2)),
        ("5s", Duration::from_secs(5)),
    ] {
        document["load_report_interval"] = text.into();
        let config = HostConfig::parse(&document.to_string()).unwrap();
        assert_eq!(config.load_report_interval, expected, "{text}");
        // Role-local: dropped before the host document is resolved against.
        let local = capyctl_config::remote_resources::local_host_document(&config.document).unwrap();
        assert!(local.get("load_report_interval").is_none());
    }
    for text in ["249ms", "0s", "6s", "1m", "soon"] {
        document["load_report_interval"] = text.into();
        let error = HostConfig::parse(&document.to_string()).unwrap_err();
        assert_eq!(error.path, "load_report_interval", "{text}: {error}");
    }
}

// T03 T17: the shutdown drain bound is role configuration,
// `shutdown.drain_timeout`, in server, host and standalone documents: 30 s when
// omitted, 0 s to 600 s when set, refused outside that range.
#[test]
fn shutdown_drain_timeout_defaults_and_is_bounded() {
    use capyctl_config::remote_roles::{drain_timeout, DEFAULT_DRAIN_TIMEOUT};
    use capyctl_config::{parse_strict, ConfigKind};
    use std::time::Duration;
    let root = Path::new("/home/operator/role");
    let mut server: serde_json::Value =
        serde_json::from_str(&ServerConfig::template(root)).unwrap();
    let mut host: serde_json::Value = serde_json::from_str(&HostConfig::template(root)).unwrap();
    let mut standalone =
        serde_json::json!({"schema_version": 1, "kind": "standalone", "name": "local"});
    assert_eq!(DEFAULT_DRAIN_TIMEOUT, Duration::from_secs(30));
    assert_eq!(
        ServerConfig::parse(&server.to_string())
            .unwrap()
            .drain_timeout,
        DEFAULT_DRAIN_TIMEOUT
    );
    assert_eq!(
        HostConfig::parse(&host.to_string()).unwrap().drain_timeout,
        DEFAULT_DRAIN_TIMEOUT
    );
    let parsed = parse_strict(ConfigKind::Standalone, &standalone.to_string()).unwrap();
    assert_eq!(drain_timeout(&parsed).unwrap(), DEFAULT_DRAIN_TIMEOUT);
    for (text, expected) in [("0s", 0), ("45s", 45), ("10m", 600), ("600s", 600)] {
        let expected = Duration::from_secs(expected);
        for document in [&mut server, &mut host, &mut standalone] {
            document["shutdown"] = serde_json::json!({"drain_timeout": text});
        }
        assert_eq!(
            ServerConfig::parse(&server.to_string())
                .unwrap()
                .drain_timeout,
            expected
        );
        let config = HostConfig::parse(&host.to_string()).unwrap();
        assert_eq!(config.drain_timeout, expected);
        // Role-local: never part of the document a deployment resolves against.
        let local = capyctl_config::remote_resources::local_host_document(&config.document).unwrap();
        assert!(local.get("shutdown").is_none());
        let parsed = parse_strict(ConfigKind::Standalone, &standalone.to_string()).unwrap();
        assert_eq!(drain_timeout(&parsed).unwrap(), expected, "{text}");
    }
    for text in ["601s", "11m", "later"] {
        for document in [&mut server, &mut host, &mut standalone] {
            document["shutdown"] = serde_json::json!({"drain_timeout": text});
        }
        let error = ServerConfig::parse(&server.to_string()).unwrap_err();
        assert_eq!(error.path, "shutdown.drain_timeout", "{text}: {error}");
        let error = HostConfig::parse(&host.to_string()).unwrap_err();
        assert_eq!(error.path, "shutdown.drain_timeout", "{text}: {error}");
        let error = parse_strict(ConfigKind::Standalone, &standalone.to_string())
            .and_then(|parsed| drain_timeout(&parsed))
            .unwrap_err();
        assert_eq!(error.path, "shutdown.drain_timeout", "{text}: {error}");
    }
    // An unknown shutdown setting is refused like any other unknown field.
    host["shutdown"] = serde_json::json!({"drain_timeout": "5s", "grace": "1s"});
    assert!(HostConfig::parse(&host.to_string()).is_err());
}

// T17 T33 (owner decision 2026-09-23): the control-session heartbeat bounds are
// server configuration: 5 s and 30 s when omitted, bounded when set, and the
// lost bound must exceed the suspend bound.
#[test]
fn server_heartbeat_timeouts_default_and_are_bounded() {
    use capyctl_config::remote_roles::HeartbeatTimeouts;
    use std::time::Duration;
    let root = Path::new("/home/operator/role");
    let mut server: serde_json::Value =
        serde_json::from_str(&ServerConfig::template(root)).unwrap();
    assert_eq!(
        ServerConfig::parse(&server.to_string()).unwrap().heartbeat,
        HeartbeatTimeouts {
            suspend_after: Duration::from_secs(5),
            lost_after: Duration::from_secs(30)
        }
    );
    server["control"] =
        serde_json::json!({"heartbeat_suspend_after": "3s", "heartbeat_lost_after": "1m"});
    assert_eq!(
        ServerConfig::parse(&server.to_string()).unwrap().heartbeat,
        HeartbeatTimeouts {
            suspend_after: Duration::from_secs(3),
            lost_after: Duration::from_secs(60)
        }
    );
    for (control, path) in [
        (
            serde_json::json!({"heartbeat_suspend_after": "1s"}),
            "control.heartbeat_suspend_after",
        ),
        (
            serde_json::json!({"heartbeat_suspend_after": "121s"}),
            "control.heartbeat_suspend_after",
        ),
        (
            serde_json::json!({"heartbeat_suspend_after": "soon"}),
            "control.heartbeat_suspend_after",
        ),
        (
            serde_json::json!({"heartbeat_lost_after": "2s"}),
            "control.heartbeat_lost_after",
        ),
        (
            serde_json::json!({"heartbeat_lost_after": "601s"}),
            "control.heartbeat_lost_after",
        ),
        (
            serde_json::json!({"heartbeat_suspend_after": "10s", "heartbeat_lost_after": "10s"}),
            "control.heartbeat_lost_after",
        ),
        (
            serde_json::json!({"heartbeat_lost_after": "4s"}),
            "control.heartbeat_lost_after",
        ),
    ] {
        server["control"] = control.clone();
        let error = ServerConfig::parse(&server.to_string()).unwrap_err();
        assert_eq!(error.path, path, "{control}: {error}");
    }
    // An unknown control setting is refused like any other unknown field.
    server["control"] = serde_json::json!({"heartbeat_interval": "1s"});
    assert!(ServerConfig::parse(&server.to_string()).is_err());
}

// T17 T19 (SPEC §10, W10 gap b): the switch drain bound is server
// configuration, `switching.drain_timeout`, also accepted in a standalone
// document's `server:` block: 30 s when omitted, 1 s to 600 s when set,
// refused outside that range, never read as the default.
#[test]
fn switching_drain_timeout_defaults_and_is_bounded() {
    use capyctl_config::remote_roles::{switch_drain_timeout, DEFAULT_SWITCH_DRAIN_TIMEOUT};
    use capyctl_config::{parse_strict, ConfigKind};
    use std::time::Duration;
    let root = Path::new("/home/operator/role");
    let mut server: serde_json::Value =
        serde_json::from_str(&ServerConfig::template(root)).unwrap();
    assert_eq!(DEFAULT_SWITCH_DRAIN_TIMEOUT, Duration::from_secs(30));
    assert_eq!(
        ServerConfig::parse(&server.to_string())
            .unwrap()
            .switch_drain_timeout,
        DEFAULT_SWITCH_DRAIN_TIMEOUT
    );
    for (text, seconds) in [("1s", 1), ("45s", 45), ("10m", 600)] {
        server["switching"] = serde_json::json!({"drain_timeout": text});
        assert_eq!(
            ServerConfig::parse(&server.to_string())
                .unwrap()
                .switch_drain_timeout,
            Duration::from_secs(seconds)
        );
        let standalone = serde_json::json!({
            "schema_version": 1, "kind": "standalone", "name": "local",
            "server": {"switching": {"drain_timeout": text}}
        });
        let parsed = parse_strict(ConfigKind::Standalone, &standalone.to_string()).unwrap();
        assert_eq!(
            switch_drain_timeout(&parsed["server"]).unwrap(),
            Duration::from_secs(seconds)
        );
    }
    for text in ["0s", "601s", "later"] {
        server["switching"] = serde_json::json!({"drain_timeout": text});
        let error = ServerConfig::parse(&server.to_string()).unwrap_err();
        assert_eq!(error.path, "switching.drain_timeout", "{text}: {error}");
    }
    server["switching"] = serde_json::json!({"window": "1s"});
    assert!(ServerConfig::parse(&server.to_string()).is_err());
}

// T26 T03 (ADR 0019, discrete GPU design §2): a remote host declares the same
// discrete shape standalone derives: host RAM in a `distinct` system domain
// and each GPU its own `device` domain naming its device. The strict host
// schema accepts it and its policy resolves; the domain rules still refuse a
// device domain that names no device.
#[test]
fn a_remote_host_declares_device_domains() {
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/effective-vllm-golden.json")).unwrap();
    let mut host = golden["input"]["host"].clone();
    host["state_dir"] = serde_json::json!("/home/operator/.local/state/capyctl");
    host["identity_dir"] = serde_json::json!("/home/operator/.local/state/capyctl/identity");
    host["resource_policy"]["domains"] = serde_json::json!({
        "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB",
                   "parked_limit": "12GiB", "host_kv_limit": "4GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "14848MiB",
                 "free_reserve": "1528MiB", "parked_limit": "2GiB"}
    });
    host["resource_policy"]["devices"] =
        serde_json::json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
    let config = HostConfig::parse(&host.to_string()).expect("a discrete host document parses");
    let local = capyctl_config::remote_resources::local_host_document(&config.document).unwrap();
    let policy = capyctl_config::effective::normalize_host_policy(&local).unwrap();
    assert_eq!(policy.domains["gpu0"].device.as_deref(), Some("gpu0"));
    // Scoped for the server's ledger, the domain and its device still match.
    let scoped =
        capyctl_config::remote_resources::scope_host_document("host-a", &config.document).unwrap();
    capyctl_config::effective::normalize_host_policy(&scoped)
        .expect("the scoped device domain still names its own device");

    host["resource_policy"]["domains"]["gpu0"]
        .as_object_mut()
        .unwrap()
        .remove("device");
    let config = HostConfig::parse(&host.to_string()).unwrap();
    let local = capyctl_config::remote_resources::local_host_document(&config.document).unwrap();
    let refused = capyctl_config::effective::normalize_host_policy(&local).unwrap_err();
    assert!(
        refused.detail.starts_with("device_policy_mismatch"),
        "{refused:?}"
    );
}

// T03 T37 (design §9): the server's inference listener accepts any unicast
// address with a port; management stays loopback-only.
#[test]
fn server_inference_bind_is_not_loopback_forced() {
    let yaml = ServerConfig::template(Path::new("/home/operator/state"));
    assert!(yaml.contains("\"0.0.0.0:8443\""), "{yaml}");
    for ok in ["100.64.0.5:8443", "[::]:8443", "127.0.0.1:8443"] {
        let doc = yaml.replace("0.0.0.0:8443", ok);
        assert_eq!(
            ServerConfig::parse(&doc).unwrap().inference,
            ok.parse().unwrap(),
            "{ok}"
        );
    }
    for bad in ["0.0.0.0:0", "224.0.0.1:8443", "0.0.0.0:7443"] {
        assert!(
            ServerConfig::parse(&yaml.replace("0.0.0.0:8443", bad)).is_err(),
            "{bad}"
        );
    }
    assert!(ServerConfig::parse(&yaml.replace("127.0.0.1:7443", "0.0.0.0:7443")).is_err());
}

// T03 (design §9): `--listen` replaces the inference bind for one run and is
// held to the same rules, including no collision with another listener.
#[test]
fn server_inference_override_keeps_the_listener_rules() {
    let yaml = ServerConfig::template(Path::new("/home/operator/state"));
    let server = ServerConfig::parse(&yaml).unwrap();
    let moved = server
        .clone()
        .with_inference("100.64.0.5:9443".parse().unwrap())
        .unwrap();
    assert_eq!(moved.inference.to_string(), "100.64.0.5:9443");
    for bad in [
        "0.0.0.0:7443",
        "127.0.0.1:7444",
        "224.0.0.1:8443",
        "0.0.0.0:0",
    ] {
        assert!(
            server.clone().with_inference(bad.parse().unwrap()).is_err(),
            "{bad}"
        );
    }
}

// T03 T37 (design §9): the server's inference listener accepts
// `authentication: none`; every other listener keeps its fixed mode.
#[test]
fn server_inference_authentication_may_be_none() {
    use capyctl_config::standalone::InferenceAuth;
    let yaml = ServerConfig::template(Path::new("/home/operator/state"));
    assert_eq!(
        ServerConfig::parse(&yaml).unwrap().inference_auth,
        InferenceAuth::ApiKey
    );
    let open = yaml.replace(
        "\"authentication\": \"api_key\"",
        "\"authentication\": \"none\"",
    );
    assert_ne!(open, yaml);
    assert_eq!(
        ServerConfig::parse(&open).unwrap().inference_auth,
        InferenceAuth::None
    );
    for (from, to) in [
        (
            "\"authentication\": \"api_key\"",
            "\"authentication\": \"token\"",
        ),
        (
            "\"authentication\": \"token\"",
            "\"authentication\": \"none\"",
        ),
        (
            "\"authentication\": \"server_tls\"",
            "\"authentication\": \"none\"",
        ),
        (
            "\"authentication\": \"mutual_tls\"",
            "\"authentication\": \"none\"",
        ),
    ] {
        let doc = yaml.replace(from, to);
        assert_ne!(doc, yaml, "{from}");
        assert!(ServerConfig::parse(&doc).is_err(), "{from} -> {to}");
    }
}

// T14 T03 (owner decisions 2026-09-25): an enrolled host behaves as the
// standalone one does. With nothing stated its models live in ~/models,
// Hugging Face and HTTP sources are allowed with the 500 GiB ceiling, and
// downloads go to ~/models/sources (owner ruling: the models directory, so
// earlier copies are reused); the resolved values are in the
// document the host publishes. An explicit `disabled` stays disabled, and a
// flag or variable wins over the document.
#[test]
fn a_server_host_allows_model_sources_by_default() {
    use capyctl_config::model_settings::ModelOverrides;
    use capyctl_config::model_source::SourceSwitch;
    let root = Path::new("/home/operator/.local/state/capyctl");
    let home = Path::new("/home/operator");
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let mut bare = serde_json::json!({
        "schema_version": 1, "kind": "host", "name": "h",
        "state_dir": root, "identity_dir": root.join("identity"),
    });
    for field in [
        "hardware_fingerprint",
        "environment_fingerprint",
        "resource_policy",
        "runtime_profiles",
    ] {
        bare[field] = fixture["host"][field].clone();
    }
    let policy = |document: &serde_json::Value, flags: &ModelOverrides, env: &ModelOverrides| {
        let config = HostConfig::parse(&document.to_string())
            .expect("a host document may omit model_store")
            .with_models(flags, env, Some(home))
            .unwrap();
        let local = capyctl_config::remote_resources::local_host_document(&config.document).unwrap();
        capyctl_config::effective::normalize_host_policy(&local).unwrap()
    };
    let none = ModelOverrides::default();
    let defaults = policy(&bare, &none, &none);
    assert_eq!(defaults.model_store, home.join("models"));
    assert_eq!(defaults.model_sources.huggingface, SourceSwitch::Allowed);
    assert_eq!(defaults.model_sources.http, SourceSwitch::Allowed);
    assert_eq!(defaults.model_sources.max_bytes, Some(500 << 30));
    assert_eq!(
        defaults.model_sources.root(&defaults.model_store),
        home.join("models")
    );
    // A stated store is kept; with the template's layout (`<state_dir>/models`)
    // downloads land where they always did.
    let mut template = bare.clone();
    template["model_store"] = serde_json::json!({"path": root.join("models")});
    let kept = policy(&template, &none, &none);
    assert_eq!(kept.model_store, root.join("models"));
    assert_eq!(
        kept.model_sources.root(&kept.model_store),
        root.join("models")
    );
    // Explicitly disabled stays disabled.
    let mut disabled = bare.clone();
    disabled["model_sources"] = serde_json::json!({"huggingface": "disabled", "http": "denied"});
    let off = policy(&disabled, &none, &none);
    assert_eq!(off.model_sources.huggingface, SourceSwitch::Denied);
    assert_eq!(off.model_sources.http, SourceSwitch::Denied);
    // The environment, then the flag, win over the document.
    let env = ModelOverrides {
        models_root: Some("/srv/env-models".into()),
        sources: Some(SourceSwitch::Allowed),
        sources_max: Some("64GiB".into()),
        ..Default::default()
    };
    let from_env = policy(&disabled, &none, &env);
    assert_eq!(from_env.model_store, Path::new("/srv/env-models"));
    assert_eq!(from_env.model_sources.huggingface, SourceSwitch::Allowed);
    assert_eq!(from_env.model_sources.max_bytes, Some(64 << 30));
    let flags = ModelOverrides {
        models_root: Some("/srv/flag-models".into()),
        sources: Some(SourceSwitch::Denied),
        sources_max: None,
        ..Default::default()
    };
    let from_flag = policy(&disabled, &flags, &env);
    assert_eq!(from_flag.model_store, Path::new("/srv/flag-models"));
    assert_eq!(from_flag.model_sources.http, SourceSwitch::Denied);
    assert_eq!(from_flag.model_sources.max_bytes, Some(64 << 30));
}

// T21 T03 (owner rule 2026-09-25: every setting three ways; standalone is a
// server plus one host): a host's engine settings follow CLI flag >
// environment > YAML > default, and what it publishes is the result: the
// `local_engine` profile, the runtime directory and the engine port range.
#[test]
fn a_host_states_its_engine_settings_three_ways() {
    use capyctl_config::engine_settings::EngineOverrides;
    let root = Path::new("/var/lib/capyctl/host");
    let yaml = serde_json::json!({
        "schema_version": 1, "kind": "host", "name": "h",
        "state_dir": root, "identity_dir": root.join("identity"),
        "runtime_dir": "/yaml/runtime",
        "resource_policy": {"endpoint_port_range": {"start": 9200, "end": 9299}},
        "local_engine": {"vllm": "/yaml/vllm", "deep_park": "on", "installation_drift": "warn"},
    });
    let env = EngineOverrides {
        vllm: Some("/env/vllm".into()),
        runtime_dir: Some("/env/runtime".into()),
        engine_ports: Some((9100, 9199)),
        deep_park: Some(false),
        ..Default::default()
    };
    let flags = EngineOverrides {
        vllm: Some("/flag/vllm".into()),
        engine_ports: Some((9000, 9099)),
        ..Default::default()
    };
    let none = EngineOverrides::default();
    let host = |document: &serde_json::Value, flags: &EngineOverrides, env: &EngineOverrides| {
        HostConfig::parse(&document.to_string())
            .unwrap()
            .with_engines(flags, env, &|path| Ok(format!("probed {}", path.display())))
            .unwrap()
    };
    let published = |config: &HostConfig| {
        (
            config.profiles["local"]["executable"]
                .as_str()
                .unwrap()
                .to_owned(),
            config.runtime_dir.display().to_string(),
            config.document["resource_policy"]["endpoint_port_range"]["start"]
                .as_u64()
                .unwrap(),
            config.profiles["local"]["security"]["deep_park"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
    };
    let flagged = host(&yaml, &flags, &env);
    assert_eq!(
        published(&flagged),
        (
            "/flag/vllm".into(),
            "/env/runtime".into(),
            9000,
            "disabled".into()
        )
    );
    assert!(flagged.document.get("local_engine").is_none());
    assert_eq!(
        flagged.profiles["local"]["build_fingerprint"],
        "probed /flag/vllm"
    );
    assert_eq!(
        published(&host(&yaml, &none, &env)),
        (
            "/env/vllm".into(),
            "/env/runtime".into(),
            9100,
            "disabled".into()
        )
    );
    assert_eq!(
        published(&host(&yaml, &none, &none)),
        (
            "/yaml/vllm".into(),
            "/yaml/runtime".into(),
            9200,
            "enabled".into()
        )
    );
    // Nothing stated: no local profile, the managed runtime, no port range.
    let bare = serde_json::json!({
        "schema_version": 1, "kind": "host", "name": "h",
        "state_dir": root, "identity_dir": root.join("identity"),
    });
    let defaults = host(&bare, &none, &none);
    assert!(defaults.profiles.is_empty());
    assert!(!defaults.runtime_dir_declared);
    assert_eq!(defaults.runtime_dir, root.join("runtime"));
    assert!(defaults.document.get("resource_policy").is_none());
    // A host has no generated deployment to give a KV cache.
    let mut kv = bare.clone();
    kv["local_engine"] = serde_json::json!({"kv_cache": "8GiB"});
    assert!(HostConfig::parse(&kv.to_string())
        .unwrap()
        .with_engines(&none, &none, &|_| Ok("fp".into()))
        .is_err());
}

// T03 (final review I8-bis): a state directory named by `--state-dir` or
// `CAPYCTL_STATE_DIR` replaces the document's for the run, with the identity
// directory and the managed runtime directory it implies.
#[test]
fn a_named_state_directory_moves_identity_and_runtime_with_it() {
    let root = std::path::Path::new("/srv/capyctl/host");
    let host = capyctl_config::remote_roles::HostConfig::parse(
        &capyctl_config::remote_roles::HostConfig::template(root),
    )
    .unwrap();
    let other = std::path::PathBuf::from("/srv/capyctl/other");
    let moved = host.with_state_dir(other.clone());
    assert_eq!(moved.state_dir, other);
    assert_eq!(moved.identity_dir, other.join("identity"));
    assert_eq!(moved.runtime_dir, other.join("runtime"));
    assert_eq!(moved.document["state_dir"], "/srv/capyctl/other");
    let server = capyctl_config::remote_roles::ServerConfig::parse(
        &capyctl_config::remote_roles::ServerConfig::template(root),
    )
    .unwrap()
    .with_state_dir(other.clone());
    assert_eq!(server.identity_dir, other.join("identity"));
}

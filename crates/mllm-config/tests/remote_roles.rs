use mllm_config::remote_roles::{HostConfig, ServerConfig};
use std::path::Path;

// T01, T02, T07: safe role templates require no engine installation.
#[test]
fn role_templates_round_trip_without_engines() {
    let root = Path::new("/home/operator/.local/state/mllm");
    let server = ServerConfig::parse(&ServerConfig::template(root)).unwrap();
    assert!(server.management.ip().is_loopback());
    assert!(server.inference.ip().is_loopback());
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
    document["runtime_dir"] = "/home/operator/mllm/runtime".into();
    document["ingress"] = serde_json::json!({"bind":"100.64.0.10:9443","address":"http://100.64.0.10:9443","transport":"trusted_private_link"});
    let config = HostConfig::parse(&document.to_string()).unwrap();
    assert_eq!(config.runtime_dir, Path::new("/home/operator/mllm/runtime"));
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
        let local = mllm_config::remote_resources::local_host_document(&config.document).unwrap();
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
    use mllm_config::remote_roles::{drain_timeout, DEFAULT_DRAIN_TIMEOUT};
    use mllm_config::{parse_strict, ConfigKind};
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
        let local = mllm_config::remote_resources::local_host_document(&config.document).unwrap();
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
    use mllm_config::remote_roles::HeartbeatTimeouts;
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
    use mllm_config::remote_roles::{switch_drain_timeout, DEFAULT_SWITCH_DRAIN_TIMEOUT};
    use mllm_config::{parse_strict, ConfigKind};
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
    host["state_dir"] = serde_json::json!("/home/operator/.local/state/mllm");
    host["identity_dir"] = serde_json::json!("/home/operator/.local/state/mllm/identity");
    host["resource_policy"]["domains"] = serde_json::json!({
        "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB",
                   "parked_limit": "12GiB", "host_kv_limit": "4GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "14848MiB",
                 "free_reserve": "1528MiB", "parked_limit": "2GiB"}
    });
    host["resource_policy"]["devices"] =
        serde_json::json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
    let config = HostConfig::parse(&host.to_string()).expect("a discrete host document parses");
    let local = mllm_config::remote_resources::local_host_document(&config.document).unwrap();
    let policy = mllm_config::effective::normalize_host_policy(&local).unwrap();
    assert_eq!(policy.domains["gpu0"].device.as_deref(), Some("gpu0"));
    // Scoped for the server's ledger, the domain and its device still match.
    let scoped =
        mllm_config::remote_resources::scope_host_document("host-a", &config.document).unwrap();
    mllm_config::effective::normalize_host_policy(&scoped)
        .expect("the scoped device domain still names its own device");

    host["resource_policy"]["domains"]["gpu0"]
        .as_object_mut()
        .unwrap()
        .remove("device");
    let config = HostConfig::parse(&host.to_string()).unwrap();
    let local = mllm_config::remote_resources::local_host_document(&config.document).unwrap();
    let refused = mllm_config::effective::normalize_host_policy(&local).unwrap_err();
    assert!(
        refused.detail.starts_with("device_policy_mismatch"),
        "{refused:?}"
    );
}

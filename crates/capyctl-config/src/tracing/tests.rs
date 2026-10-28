use super::*;
use crate::remote_roles::{HostConfig, ServerConfig};
use crate::setting_overrides::{defaults, setting_paths, SettingOverrides};
use crate::ConfigKind;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

const LOOPBACK: &str = "http://127.0.0.1:4318/v1/traces";
const REMOTE: &str = "https://collector.example.internal:4318/v1/traces";

fn server_document(tracing: Value) -> Value {
    let mut document: Value =
        serde_json::from_str(&ServerConfig::template(Path::new("/srv/capyctl"))).unwrap();
    document["observability"] = json!({"tracing": tracing});
    document
}

fn host_document(tracing: Value) -> Value {
    let mut document: Value =
        serde_json::from_str(&HostConfig::template(Path::new("/srv/capyctl"))).unwrap();
    document["observability"] = json!({"tracing": tracing});
    document
}

fn standalone_document(server: Value, host: Value) -> Value {
    json!({
        "schema_version": 1, "kind": "standalone", "name": "capyctl",
        "server": {"observability": {"tracing": server}},
        "host": {"observability": {"tracing": host}},
    })
}

fn server_refusal(tracing: Value) -> ConfigError {
    server_tracing(&json!({"observability": {"tracing": tracing}})).unwrap_err()
}

fn host_refusal(tracing: Value) -> ConfigError {
    host_tracing(&json!({"observability": {"tracing": tracing}})).unwrap_err()
}

fn private_file(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

// T48 (ADR 0037 §1, owner decisions 2026-10-27 q1, q3, q4): with nothing
// stated, tracing is off, the ratio 1.0, the caller context `parent`, and
// engine tracing off, on every role.
#[test]
fn tracing_is_off_unless_an_endpoint_is_named() {
    let server = ServerConfig::parse(&ServerConfig::template(Path::new("/srv/capyctl"))).unwrap();
    assert_eq!(server.tracing, ServerTracing::default());
    assert!(server.tracing.export.is_none());
    assert_eq!(server.tracing.sample_ratio, 1.0);
    assert_eq!(server.tracing.caller_context, CallerContext::Parent);
    let host = HostConfig::parse(&HostConfig::template(Path::new("/srv/capyctl"))).unwrap();
    assert_eq!(host.tracing, HostTracing::default());
    assert_eq!(host.tracing.engines, EngineTracingSetting::Off);
    assert_eq!(
        standalone_tracing(&json!({"server": {}, "host": {}})).unwrap(),
        (ServerTracing::default(), HostTracing::default())
    );
    // An empty or null block is off too; so is a ratio or caller context
    // without an endpoint.
    for tracing in [json!({}), Value::Null] {
        assert!(
            server_tracing(&json!({"observability": {"tracing": tracing}}))
                .unwrap()
                .export
                .is_none()
        );
    }
    let unnamed = server_tracing(&json!({"observability": {"tracing": {"sample_ratio": 0.5}}}));
    assert!(unnamed.unwrap().export.is_none());
}

// T48 T03 (ADR 0037 §12): every field, every accepted value.
#[test]
fn every_tracing_field_is_read() {
    let server = ServerConfig::parse(
        &server_document(json!({
            "endpoint": REMOTE,
            "headers_file": "/etc/capyctl/otlp-headers",
            "sample_ratio": 0.25,
            "caller_context": "link",
        }))
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        server.tracing,
        ServerTracing {
            export: Some(TracingExport {
                endpoint: REMOTE.into(),
                headers_file: Some("/etc/capyctl/otlp-headers".into()),
            }),
            sample_ratio: 0.25,
            caller_context: CallerContext::Link,
        }
    );
    for (ratio, expected) in [
        (json!(0), 0.0),
        (json!(1), 1.0),
        (json!(0.0), 0.0),
        (json!(1.0), 1.0),
    ] {
        let tracing = server_tracing(&json!({"observability": {"tracing": {
            "endpoint": REMOTE, "sample_ratio": ratio}}}))
        .unwrap();
        assert_eq!(tracing.sample_ratio, expected);
    }
    for (text, expected) in [
        ("parent", CallerContext::Parent),
        ("link", CallerContext::Link),
        ("ignore", CallerContext::Ignore),
    ] {
        let tracing = server_tracing(&json!({"observability": {"tracing": {
            "endpoint": REMOTE, "caller_context": text}}}))
        .unwrap();
        assert_eq!(tracing.caller_context, expected);
        assert_eq!(expected.as_str(), text);
    }

    let host = HostConfig::parse(
        &host_document(json!({
            "endpoint": LOOPBACK,
            "headers_file": "/etc/capyctl/otlp-headers",
            "engines": "on",
        }))
        .to_string(),
    )
    .unwrap();
    assert_eq!(host.tracing.engines, EngineTracingSetting::On);
    assert_eq!(host.tracing.export.as_ref().unwrap().endpoint, LOOPBACK);
    // A host may export off loopback when its engines do not trace.
    let remote = host_tracing(&json!({"observability": {"tracing": {
        "endpoint": REMOTE, "engines": "off"}}}))
    .unwrap();
    assert_eq!(remote.engines, EngineTracingSetting::Off);
    // ADR 0037 §7: loopback is `localhost`, 127.0.0.0/8 and ::1.
    for endpoint in [
        "http://localhost:4318/v1/traces",
        "http://LOCALHOST:4318",
        "http://127.0.0.1:4318/v1/traces",
        "https://127.8.9.10/v1/traces",
        "http://[::1]:4318/v1/traces",
        "http://[::1]/v1/traces",
    ] {
        let tracing = host_tracing(&json!({"observability": {"tracing": {
            "endpoint": endpoint, "engines": "on"}}}))
        .unwrap_or_else(|error| panic!("{endpoint}: {error}"));
        let export = tracing.export.unwrap();
        assert!(export.on_loopback(), "{endpoint}");
        assert!(!export.plain_http_off_loopback(), "{endpoint}");
    }
    for (endpoint, plain) in [
        ("http://collector.example:4318", true),
        ("http://10.0.0.5:4318/v1/traces", true),
        ("http://[2001:db8::1]:4318", true),
        ("https://collector.example:4318", false),
        ("http://localhost.example:4318", true),
    ] {
        let export = TracingExport {
            endpoint: endpoint.into(),
            headers_file: None,
        };
        assert!(!export.on_loopback(), "{endpoint}");
        assert_eq!(export.plain_http_off_loopback(), plain, "{endpoint}");
    }
}

// T48 T03 (ADR 0037 §12): each invalid setting refuses the start, with the
// field's path, the closest existing code, and never the endpoint's text.
#[test]
fn invalid_tracing_settings_are_refused() {
    use ConfigErrorCode::{UnknownField, UnsupportedCombination};
    let path = |field: &str| format!("observability.tracing.{field}");
    for ratio in [
        json!(-0.1),
        json!(1.1),
        json!("x"),
        json!("0.5"),
        json!(true),
    ] {
        let error = server_refusal(json!({"endpoint": REMOTE, "sample_ratio": ratio}));
        assert_eq!(
            (error.code, error.path.as_str()),
            (UnsupportedCombination, path("sample_ratio").as_str()),
            "{ratio}: {error}"
        );
    }
    for endpoint in [
        json!("ftp://collector.example/v1/traces"),
        json!("collector.example:4318"),
        json!("https://"),
        json!("https:///v1/traces"),
        json!("http://user:hunter2@collector.example:4318"),
        json!("http://collector.example:0"),
        json!("http://collector.example:70000"),
        json!("http://collector.example:port"),
        json!("http://coll ector.example"),
        json!("http://[::1"),
        json!(4318),
    ] {
        for error in [
            server_refusal(json!({"endpoint": endpoint})),
            host_refusal(json!({"endpoint": endpoint})),
        ] {
            assert_eq!(
                (error.code, error.path.as_str()),
                (UnsupportedCombination, path("endpoint").as_str()),
                "{endpoint}: {error}"
            );
            assert!(!error.to_string().contains("hunter2"), "{error}");
            assert!(!error.to_string().contains("collector.example"), "{error}");
        }
    }
    for error in [
        server_refusal(json!({"headers_file": "/etc/capyctl/otlp-headers"})),
        host_refusal(json!({"headers_file": "/etc/capyctl/otlp-headers"})),
        server_refusal(json!({"endpoint": REMOTE, "headers_file": "otlp-headers"})),
        server_refusal(json!({"endpoint": REMOTE, "headers_file": 7})),
    ] {
        assert_eq!(
            (error.code, error.path.as_str()),
            (UnsupportedCombination, path("headers_file").as_str()),
            "{error}"
        );
    }
    for tracing in [
        json!({"endpoint": REMOTE, "engines": "on"}),
        json!({"endpoint": "http://10.0.0.5:4318", "engines": "on"}),
        json!({"endpoint": "http://localhost.example:4318", "engines": "on"}),
        json!({"engines": "on"}),
        json!({"endpoint": LOOPBACK, "engines": "yes"}),
        json!({"endpoint": LOOPBACK, "engines": true}),
    ] {
        let error = host_refusal(tracing.clone());
        assert_eq!(
            (error.code, error.path.as_str()),
            (UnsupportedCombination, path("engines").as_str()),
            "{tracing}: {error}"
        );
    }
    let error = server_refusal(json!({"endpoint": REMOTE, "caller_context": "trust"}));
    assert_eq!(error.path, path("caller_context"));
    // Unknown fields: a typo, and a field of the other role.
    for (error, field) in [
        (
            server_refusal(json!({"endpoint": REMOTE, "endpiont": REMOTE})),
            "endpiont",
        ),
        (
            server_refusal(json!({"endpoint": REMOTE, "engines": "on"})),
            "engines",
        ),
        (
            host_refusal(json!({"endpoint": LOOPBACK, "sample_ratio": 1.0})),
            "sample_ratio",
        ),
        (
            host_refusal(json!({"caller_context": "parent"})),
            "caller_context",
        ),
    ] {
        assert_eq!((error.code, error.path), (UnknownField, path(field)));
    }
    assert_eq!(
        server_tracing(&json!({"observability": {"tracing": "on"}}))
            .unwrap_err()
            .path,
        "observability.tracing"
    );
    // The strict schema refuses the same unknown fields in a role document.
    for document in [
        server_document(json!({"endpoint": REMOTE, "engines": "on"})).to_string(),
        server_document(json!({"endpoint": REMOTE, "other": 1})).to_string(),
    ] {
        assert_eq!(
            ServerConfig::parse(&document).unwrap_err().code,
            UnknownField
        );
    }
    let host = host_document(json!({"endpoint": LOOPBACK, "sample_ratio": 0.5})).to_string();
    assert_eq!(HostConfig::parse(&host).unwrap_err().code, UnknownField);
    // And the role parse carries the value refusals.
    let error = ServerConfig::parse(
        &server_document(json!({"endpoint": REMOTE, "sample_ratio": 1.1})).to_string(),
    )
    .unwrap_err();
    assert_eq!(error.path, path("sample_ratio"));
    let error =
        HostConfig::parse(&host_document(json!({"endpoint": REMOTE, "engines": "on"})).to_string())
            .unwrap_err();
    assert_eq!(error.path, path("engines"));
}

// T48 T03 (ADR 0037 §12): standalone reads `server.observability.tracing` as
// a server's; `host.observability.tracing` takes only `engines`, which
// exports to the server block's endpoint and so needs it on loopback.
#[test]
fn standalone_tracing_is_the_server_block_and_host_engines() {
    let (server, host) = standalone_tracing(&standalone_document(
        json!({"endpoint": LOOPBACK, "sample_ratio": 0.5, "caller_context": "ignore"}),
        json!({"engines": "on"}),
    ))
    .unwrap();
    assert_eq!(server.sample_ratio, 0.5);
    assert_eq!(server.caller_context, CallerContext::Ignore);
    assert_eq!(host.engines, EngineTracingSetting::On);
    assert_eq!(host.export, server.export);

    let error = standalone_tracing(&standalone_document(
        json!({"endpoint": REMOTE}),
        json!({"engines": "on"}),
    ))
    .unwrap_err();
    assert_eq!(error.path, "host.observability.tracing.engines", "{error}");
    assert!(error
        .detail
        .contains("server.observability.tracing.endpoint"));
    let error =
        standalone_tracing(&standalone_document(json!({}), json!({"engines": "on"}))).unwrap_err();
    assert_eq!(error.path, "host.observability.tracing.engines");
    let error = standalone_tracing(&standalone_document(
        json!({"endpoint": REMOTE, "sample_ratio": -0.1}),
        json!({}),
    ))
    .unwrap_err();
    assert_eq!(error.path, "server.observability.tracing.sample_ratio");
    for field in [
        "endpoint",
        "headers_file",
        "sample_ratio",
        "caller_context",
        "other",
    ] {
        let mut host = json!({"engines": "off"});
        host[field] = json!(LOOPBACK);
        let error = standalone_tracing(&standalone_document(json!({"endpoint": LOOPBACK}), host))
            .unwrap_err();
        assert_eq!(
            (error.code, error.path.as_str()),
            (
                ConfigErrorCode::UnknownField,
                format!("host.observability.tracing.{field}").as_str()
            )
        );
        assert_eq!(error.detail, STANDALONE_HOST_TRACING);
    }
    // The strict schema points the host block's fields to the server block,
    // and `validate config` refuses a bad combination through
    // `check_honoured`.
    let mut document = standalone_document(json!({"endpoint": LOOPBACK}), json!({}));
    document["host"]["observability"]["tracing"]["endpoint"] = json!(LOOPBACK);
    let error = crate::validate(&document.to_string(), ConfigKind::Standalone).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnknownField);
    assert_eq!(error.path, "host.observability.tracing.endpoint");
    assert_eq!(error.detail, STANDALONE_HOST_TRACING);
    let good = standalone_document(json!({"endpoint": LOOPBACK}), json!({"engines": "on"}));
    crate::validate(&good.to_string(), ConfigKind::Standalone).unwrap();
    crate::standalone::check_honoured(&good, Path::new("/s/config"), Path::new("/s")).unwrap();
    let bad = standalone_document(json!({"endpoint": REMOTE}), json!({"engines": "on"}));
    let error = crate::standalone::check_honoured(&bad, Path::new("/s/config"), Path::new("/s"))
        .unwrap_err();
    assert_eq!(error.path, "host.observability.tracing.engines");
}

// T48 (ADR 0037 §12, owner decision 2026-09-25): every field is settable by
// path, `--set` over `CAPYCTL_SET__…`, on every role (standalone with the
// `server.` or `host.` prefix), and validated as the YAML would be.
#[test]
fn tracing_settings_resolve_by_path() {
    let server_paths = setting_paths(ConfigKind::Server);
    for field in ["endpoint", "headers_file", "sample_ratio", "caller_context"] {
        let path = format!("observability.tracing.{field}");
        assert!(server_paths.contains(&path), "{path}");
        assert!(setting_paths(ConfigKind::Standalone).contains(&format!("server.{path}")));
    }
    let host_paths = setting_paths(ConfigKind::Host);
    for field in ["endpoint", "headers_file", "engines"] {
        assert!(host_paths.contains(&format!("observability.tracing.{field}")));
    }
    let standalone_paths = setting_paths(ConfigKind::Standalone);
    assert!(standalone_paths.contains(&"host.observability.tracing.engines".to_owned()));
    assert!(!standalone_paths.contains(&"host.observability.tracing.endpoint".to_owned()));

    let overrides = SettingOverrides::parse(
        ConfigKind::Server,
        &[
            format!("observability.tracing.endpoint={REMOTE}"),
            "observability.tracing.sample_ratio=0.75".to_owned(),
        ],
        &[
            (
                "CAPYCTL_SET__OBSERVABILITY__TRACING__ENDPOINT".to_owned(),
                LOOPBACK.to_owned(),
            ),
            (
                "CAPYCTL_SET__OBSERVABILITY__TRACING__CALLER_CONTEXT".to_owned(),
                "link".to_owned(),
            ),
            (
                "CAPYCTL_SET__OBSERVABILITY__TRACING__HEADERS_FILE".to_owned(),
                "/etc/capyctl/otlp-headers".to_owned(),
            ),
        ],
    )
    .unwrap();
    let mut document: Value =
        serde_json::from_str(&ServerConfig::template(Path::new("/srv/capyctl"))).unwrap();
    overrides.apply(&mut document).unwrap();
    let tracing = ServerConfig::parse(&document.to_string()).unwrap().tracing;
    let export = tracing.export.unwrap();
    assert_eq!(export.endpoint, REMOTE, "--set wins over CAPYCTL_SET__");
    assert_eq!(
        export.headers_file,
        Some("/etc/capyctl/otlp-headers".into())
    );
    assert_eq!(tracing.sample_ratio, 0.75);
    assert_eq!(tracing.caller_context, CallerContext::Link);

    let overrides = SettingOverrides::parse(
        ConfigKind::Host,
        &[],
        &[
            (
                "CAPYCTL_SET__OBSERVABILITY__TRACING__ENDPOINT".to_owned(),
                LOOPBACK.to_owned(),
            ),
            (
                "CAPYCTL_SET__OBSERVABILITY__TRACING__ENGINES".to_owned(),
                "on".to_owned(),
            ),
        ],
    )
    .unwrap();
    let mut document: Value =
        serde_json::from_str(&HostConfig::template(Path::new("/srv/capyctl"))).unwrap();
    overrides.apply(&mut document).unwrap();
    let tracing = HostConfig::parse(&document.to_string()).unwrap().tracing;
    assert_eq!(tracing.engines, EngineTracingSetting::On);

    let overrides = SettingOverrides::parse(
        ConfigKind::Standalone,
        &[format!("server.observability.tracing.endpoint={LOOPBACK}")],
        &[(
            "CAPYCTL_SET__HOST__OBSERVABILITY__TRACING__ENGINES".to_owned(),
            "on".to_owned(),
        )],
    )
    .unwrap();
    let document = overrides
        .apply_and_validate(json!({"schema_version": 1, "kind": "standalone", "name": "s"}))
        .unwrap();
    let (server, host) = standalone_tracing(&document).unwrap();
    assert_eq!(server.export.unwrap().endpoint, LOOPBACK);
    assert_eq!(host.engines, EngineTracingSetting::On);

    // A bad value is refused as the YAML would be, naming the override.
    let overrides = SettingOverrides::parse(
        ConfigKind::Server,
        &["observability.tracing.sample_ratio=x".to_owned()],
        &[],
    )
    .unwrap();
    let mut document = server_document(json!({"endpoint": REMOTE}));
    overrides.apply(&mut document).unwrap();
    let error = ServerConfig::parse(&document.to_string()).unwrap_err();
    assert_eq!(error.path, "observability.tracing.sample_ratio");
    // The standalone host block's other fields point to the server block.
    let overrides = SettingOverrides::parse(
        ConfigKind::Standalone,
        &[format!("host.observability.tracing.endpoint={LOOPBACK}")],
        &[],
    )
    .unwrap();
    let error = overrides
        .apply_and_validate(json!({"schema_version": 1, "kind": "standalone", "name": "s"}))
        .unwrap_err();
    assert_eq!(error.path, "host.observability.tracing.endpoint");
    assert!(error.detail.starts_with(STANDALONE_HOST_TRACING), "{error}");
}

// T48 (ADR 0037 §13): `config show` lists the tracing defaults; tracing
// itself is off, so no endpoint is listed until one is stated.
#[test]
fn config_show_lists_the_tracing_defaults() {
    let root = Path::new("/srv/capyctl");
    let has = |kind, path: &str, value: Value| {
        defaults(kind, None, root).contains(&(path.to_owned(), value))
    };
    assert!(has(
        ConfigKind::Server,
        "observability.tracing.sample_ratio",
        json!(1.0)
    ));
    assert!(has(
        ConfigKind::Server,
        "observability.tracing.caller_context",
        json!("parent")
    ));
    assert!(has(
        ConfigKind::Host,
        "observability.tracing.engines",
        json!("off")
    ));
    assert!(has(
        ConfigKind::Standalone,
        "server.observability.tracing.sample_ratio",
        json!(1.0)
    ));
    assert!(has(
        ConfigKind::Standalone,
        "server.observability.tracing.caller_context",
        json!("parent")
    ));
    assert!(has(
        ConfigKind::Standalone,
        "host.observability.tracing.engines",
        json!("off")
    ));
    for kind in [ConfigKind::Server, ConfigKind::Host, ConfigKind::Standalone] {
        let paths: Vec<String> = setting_paths(kind);
        for (path, _) in defaults(kind, None, root) {
            if path.contains("tracing") {
                assert!(paths.contains(&path), "{path}");
            }
        }
        assert!(!defaults(kind, None, root)
            .iter()
            .any(|(path, _)| path.ends_with("tracing.endpoint")
                || path.ends_with("tracing.headers_file")));
    }
}

// T48 T21 (ADR 0037 §8, §12): the settings are role-local. They never reach
// the document deployments are resolved against, no deployment field states
// them, and the standalone host's published settings leave them out.
#[test]
fn tracing_settings_are_role_local() {
    let base = HostConfig::parse(&HostConfig::template(Path::new("/srv/capyctl"))).unwrap();
    let unset = crate::remote_resources::local_host_document(&base.document).unwrap();
    let traced = HostConfig::parse(
        &host_document(json!({
            "endpoint": LOOPBACK, "headers_file": "/etc/capyctl/otlp-headers", "engines": "on"}))
        .to_string(),
    )
    .unwrap();
    let local = crate::remote_resources::local_host_document(&traced.document).unwrap();
    assert!(local.get("observability").is_none());
    assert_eq!(local, unset);
    assert!(!crate::schema::schema(ConfigKind::Deployment)
        .fields
        .iter()
        .any(|(name, _)| *name == "observability"));
    let mut published = json!({"schema_version": 1, "kind": "host"});
    crate::standalone::publish_host_settings(
        &mut published,
        &json!({"observability": {"tracing": {"engines": "on"}}}),
    );
    assert!(published.get("observability").is_none());
}

// T48 T21 (ADR 0037 §8, SPEC §13.3): the headers file is private state: a
// regular file of this user with no group or other bits. Valid lines are
// read; every refusal names the path and line, never a value.
#[test]
fn headers_file_is_private_and_strict() {
    let dir = tempfile::tempdir().unwrap();
    let secret = "Bearer s3cr3t-token-value";
    let good = private_file(
        dir.path(),
        "good",
        &format!(
            "# collector credentials\n\nAuthorization: {secret}\r\nx-scope-orgid:  tenant-1  \n"
        ),
        0o600,
    );
    assert_eq!(
        read_headers_file(&good).unwrap(),
        vec![
            ("Authorization".to_owned(), secret.to_owned()),
            ("x-scope-orgid".to_owned(), "tenant-1".to_owned()),
        ]
    );
    let export = TracingExport {
        endpoint: REMOTE.into(),
        headers_file: Some(good.clone()),
    };
    assert_eq!(export.headers().unwrap().len(), 2);
    let bare = TracingExport {
        endpoint: REMOTE.into(),
        headers_file: None,
    };
    assert!(bare.headers().unwrap().is_empty());
    let empty = private_file(dir.path(), "empty", "# none yet\n", 0o400);
    assert!(read_headers_file(&empty).unwrap().is_empty());

    let text = format!("Authorization: {secret}\n");
    for mode in [0o640, 0o604, 0o660, 0o644, 0o601] {
        let path = private_file(dir.path(), &format!("open-{mode:o}"), &text, mode);
        let error = read_headers_file(&path).unwrap_err();
        assert_eq!(error.code, ConfigErrorCode::Io, "{mode:o}");
        assert_eq!(error.path, path.display().to_string());
        assert!(error.detail.contains("group or other"), "{error}");
        assert!(!error.to_string().contains("s3cr3t"), "{error}");
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    let error = read_headers_file_owned_by(&good, me.wrapping_add(1)).unwrap_err();
    assert!(error.detail.contains("owned by uid"), "{error}");
    assert!(!error.to_string().contains("s3cr3t"), "{error}");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&good, &link).unwrap();
    assert!(read_headers_file(&link)
        .unwrap_err()
        .detail
        .contains("symbolic link"));
    let error = read_headers_file(dir.path()).unwrap_err();
    assert!(error.detail.contains("regular file"), "{error}");
    let error = read_headers_file(&dir.path().join("missing")).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::Io);

    for (body, line, detail) in [
        (
            format!("# ok\n{secret}\n"),
            2,
            "is not a `Name: value` header",
        ),
        (
            format!("\n\nBad Name: {secret}\n"),
            3,
            "not a valid HTTP token",
        ),
        (format!("Auth(x): {secret}\n"), 1, "not a valid HTTP token"),
        (format!(": {secret}\n"), 1, "not a valid HTTP token"),
        ("Authorization:   \n".to_owned(), 1, "has no value"),
        (
            format!("A: ok\nAuthorization: {secret}\u{7}\n"),
            2,
            "control character",
        ),
    ] {
        let path = private_file(dir.path(), "malformed", &body, 0o600);
        let error = read_headers_file(&path).unwrap_err();
        assert_eq!(error.path, path.display().to_string());
        assert!(
            error.detail.starts_with(&format!("line {line}: ")) && error.detail.contains(detail),
            "{body:?}: {error}"
        );
        assert!(!error.to_string().contains("s3cr3t"), "{error}");
    }
    let path = dir.path().join("binary");
    std::fs::write(&path, b"A: ok\nAuthorization: \xff\xfe\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(read_headers_file(&path)
        .unwrap_err()
        .detail
        .starts_with("line 2: "));
    let big = private_file(
        dir.path(),
        "big",
        &"A: b\n".repeat(MAX_HEADERS_FILE_BYTES as usize / 5 + 1),
        0o600,
    );
    assert!(read_headers_file(&big)
        .unwrap_err()
        .detail
        .contains("larger"));
}

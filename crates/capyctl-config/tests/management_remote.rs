//! ADR 0040 §1: the management listener stays on loopback unless its role
//! states `remote: true`; `public_address` is an optional origin. Parsing an
//! address is split from the loopback check, which applies only without
//! `remote`.

use std::path::Path;

use capyctl_config::remote_roles::ServerConfig;
use capyctl_config::setting_overrides::{defaults, leaves, SettingOverrides};
use capyctl_config::standalone::{check_honoured, management_listener};
use capyctl_config::ConfigKind;
use serde_json::{json, Value};

const LOOPBACK_REFUSAL: &str =
    "the management listener binds a loopback address with a non-zero port, e.g. 127.0.0.1:7443";

fn standalone(management: Value) -> Value {
    json!({
        "schema_version": 1, "kind": "standalone", "name": "local",
        "server": {"listeners": {"management": management}}
    })
}

/// The standalone document as the role validates it: the strict schema, then
/// what the role honours.
fn check(document: &Value) -> Result<(), capyctl_config::ConfigError> {
    let document =
        SettingOverrides::none(ConfigKind::Standalone).apply_and_validate(document.clone())?;
    check_honoured(&document, Path::new("/s/config"), Path::new("/s")).map(|_| ())
}

fn server(management: Value) -> String {
    let mut document: Value =
        serde_json::from_str(&ServerConfig::template(Path::new("/srv/capyctl"))).unwrap();
    document["listeners"]["management"] = management;
    document.to_string()
}

// ADR 0040 §1 (AE3): without `remote`, an off-box bind is refused with the
// loopback message, exactly as before.
#[test]
fn an_off_box_bind_without_remote_is_refused_with_the_loopback_message() {
    let document = standalone(json!({"bind": "100.101.102.103:7443"}));
    let error = check(&document).unwrap_err();
    assert_eq!(error.path, "server.listeners.management.bind");
    assert_eq!(error.detail, LOOPBACK_REFUSAL);
    let error = management_listener(&document).unwrap_err();
    assert_eq!(error.detail, LOOPBACK_REFUSAL);
    // `remote: false` is the same as leaving it out.
    let document = standalone(json!({"bind": "100.101.102.103:7443", "remote": false}));
    assert_eq!(check(&document).unwrap_err().detail, LOOPBACK_REFUSAL);
}

// ADR 0040 §1 (AE3): with `remote: true` the same bind is accepted, and the
// effective configuration lists both settings.
#[test]
fn an_off_box_bind_with_remote_is_accepted_and_listed() {
    let document = standalone(json!({
        "bind": "100.101.102.103:7443",
        "remote": true,
        "public_address": "http://box:7443",
    }));
    check(&document).unwrap();
    let listener = management_listener(&document).unwrap();
    assert_eq!(listener.bind.to_string(), "100.101.102.103:7443");
    assert!(listener.remote);
    assert_eq!(listener.public_address.as_deref(), Some("http://box:7443"));
    // Every interface too; port 0 is still refused.
    check(&standalone(json!({"bind": "0.0.0.0:7443", "remote": true}))).unwrap();
    check(&standalone(json!({"bind": "[::]:7443", "remote": true}))).unwrap();
    let error = check(&standalone(json!({"bind": "0.0.0.0:0", "remote": true}))).unwrap_err();
    assert_eq!(error.path, "server.listeners.management.bind");
    // `remote` is a boolean.
    let error = check(&standalone(json!({"remote": "yes"}))).unwrap_err();
    assert_eq!(error.path, "server.listeners.management.remote");

    let stated = leaves(&document);
    for (path, value) in [
        ("server.listeners.management.remote", json!(true)),
        (
            "server.listeners.management.public_address",
            json!("http://box:7443"),
        ),
    ] {
        assert!(
            stated.contains(&(path.to_owned(), value)),
            "{path}: {stated:?}"
        );
    }
    // Unstated, `remote` is listed with its default.
    for (kind, path) in [
        (ConfigKind::Standalone, "server.listeners.management.remote"),
        (ConfigKind::Server, "listeners.management.remote"),
    ] {
        assert!(
            defaults(kind, None, Path::new("/s")).contains(&(path.to_owned(), json!(false))),
            "{path}"
        );
    }
}

// ADR 0040 §1: `public_address` is an origin: `http://` or `https://`, a host
// and an optional port, no path.
#[test]
fn a_public_address_is_an_origin() {
    for good in [
        "https://box.tail1234.ts.net",
        "http://box:7443",
        "http://100.101.102.103:7443",
        "http://[fd7a:115c:a1e0::1]:7443",
    ] {
        let document = standalone(json!({"public_address": good}));
        check(&document).unwrap_or_else(|e| panic!("{good}: {e}"));
        assert_eq!(
            management_listener(&document)
                .unwrap()
                .public_address
                .as_deref(),
            Some(good)
        );
        let parsed = ServerConfig::parse(&server(json!({
            "bind": "127.0.0.1:7443", "authentication": "token", "public_address": good
        })))
        .unwrap();
        assert_eq!(parsed.management_public_address.as_deref(), Some(good));
    }
    for bad in [
        "https://box.tail1234.ts.net/ui",
        "https://box.tail1234.ts.net/",
        "box.tail1234.ts.net",
        "ftp://box",
        "https://",
        "https://box:0",
        "https://user@box",
    ] {
        let error = check(&standalone(json!({"public_address": bad}))).unwrap_err();
        assert_eq!(
            error.path, "server.listeners.management.public_address",
            "{bad}"
        );
        assert!(
            ServerConfig::parse(&server(json!({
                "bind": "127.0.0.1:7443", "authentication": "token", "public_address": bad
            })))
            .is_err(),
            "{bad}"
        );
    }
}

// ADR 0040 §1: a server document states the same settings on
// `listeners.management`; `remote` with a loopback bind changes nothing else.
#[test]
fn a_server_management_listener_takes_remote_and_public_address() {
    let plain = ServerConfig::parse(&server(
        json!({"bind": "127.0.0.1:7443", "authentication": "token"}),
    ))
    .unwrap();
    assert!(!plain.management_remote);
    assert_eq!(plain.management_public_address, None);
    let remote = ServerConfig::parse(&server(
        json!({"bind": "127.0.0.1:7443", "authentication": "token", "remote": true}),
    ))
    .unwrap();
    assert_eq!(remote.management, plain.management);
    assert!(remote.management_remote);
    // Off the box only with `remote`.
    assert!(ServerConfig::parse(&server(
        json!({"bind": "100.101.102.103:7443", "authentication": "token"})
    ))
    .is_err());
    let off_box = ServerConfig::parse(&server(
        json!({"bind": "100.101.102.103:7443", "authentication": "token", "remote": true}),
    ))
    .unwrap();
    assert_eq!(off_box.management.to_string(), "100.101.102.103:7443");
    // A one-run address follows the run's `remote`.
    let address = "0.0.0.0:7543".parse().unwrap();
    assert!(plain.clone().with_management(Some(address), None).is_err());
    let moved = plain.with_management(Some(address), Some(true)).unwrap();
    assert_eq!(moved.management, address);
    assert!(moved.management_remote);
    // `remote: false` for one run takes an off-box document bind back.
    assert!(off_box.with_management(None, Some(false)).is_err());
}

// ADR 0040 §1: `remote` and `public_address` belong to the management
// listener only.
#[test]
fn only_the_management_listener_takes_remote() {
    let mut document = standalone(json!({}));
    document["server"]["listeners"]["inference"] = json!({"remote": true});
    assert_eq!(
        check(&document).unwrap_err().path,
        "server.listeners.inference.remote"
    );
    let mut document: Value =
        serde_json::from_str(&ServerConfig::template(Path::new("/srv/capyctl"))).unwrap();
    document["listeners"]["inference"]["public_address"] = json!("http://box:8443");
    assert!(ServerConfig::parse(&document.to_string()).is_err());
}

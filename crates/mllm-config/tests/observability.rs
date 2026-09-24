//! SPEC §17 (M80): `observability.timing_header` is off unless a server
//! document turns it on, and only a boolean is accepted.
use mllm_config::remote_roles::{timing_header, ServerConfig};

fn server(observability: Option<serde_json::Value>) -> String {
    let mut document: serde_json::Value =
        serde_json::from_str(&ServerConfig::template(std::path::Path::new("/srv/mllm"))).unwrap();
    if let Some(observability) = observability {
        document["observability"] = observability;
    }
    document.to_string()
}

#[test]
fn timing_header_is_opt_in_and_boolean() {
    assert!(!ServerConfig::parse(&server(None)).unwrap().timing_header);
    let on = server(Some(serde_json::json!({"timing_header": true})));
    assert!(ServerConfig::parse(&on).unwrap().timing_header);
    assert!(
        ServerConfig::parse(&server(Some(serde_json::json!({"timing_header": "yes"})))).is_err()
    );
    assert!(ServerConfig::parse(&server(Some(serde_json::json!({"other": true})))).is_err());
    assert!(!timing_header(&serde_json::json!({})).unwrap());
    // An explicit false reads as off.
    let text = server(None).replace(
        "\"schema_version\"",
        "\"observability\":{\"timing_header\":false},\"schema_version\"",
    );
    assert!(!ServerConfig::parse(&text).unwrap().timing_header);
}

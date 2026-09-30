//! SPEC §9.1, T21, owner decision P4: human output of status and inspect
//! warns about every deployment and host installation that exposes vLLM
//! development controls. The JSON result itself is unchanged; warnings are
//! diagnostics on stderr. Not qualification of any native engine recipe.

use capyctl_cli::output::development_controls_notices;
use serde_json::json;

fn exposed() -> serde_json::Value {
    json!({
        "state": "exposed", "engine": "vllm", "deep_park": "enabled",
        "deep_park_source": "default", "enable_sleep_mode": true, "residency": "deep",
        "surface": ["/sleep", "/wake_up", "/is_sleeping", "/collective_rpc"],
        "mitigations": ["loopback_engine_listener", "per_launch_engine_key",
            "engine_key_guard_middleware", "no_ingress_or_router_path"],
        "production_safe": false
    })
}

// T21
#[test]
fn a_status_view_of_an_exposed_deployment_warns_with_mitigations() {
    let view = json!({"id": "d1", "name": "qwen", "development_controls": exposed()});
    let notices = development_controls_notices(&view);
    assert_eq!(notices.len(), 1, "{notices:?}");
    let line = &notices[0];
    assert!(line.contains("deployment qwen"), "{line}");
    assert!(line.contains("vLLM development mode"), "{line}");
    assert!(line.contains("deep_park enabled (default)"), "{line}");
    assert!(line.contains("/collective_rpc"), "{line}");
    assert!(line.contains("loopback_engine_listener"), "{line}");
    assert!(line.contains("not production-safe"), "{line}");
}

// T21: a list and a host inventory are walked; safe items stay silent and
// an underivable item is called out rather than reported safe.
#[test]
fn lists_and_host_views_mark_every_exposed_item() {
    let list = json!([
        {"id": "d1", "name": "a", "development_controls": exposed()},
        {"id": "d2", "name": "b", "development_controls": {"state": "not_exposed", "engine": "sglang"}},
        {"id": "d3", "name": "c", "development_controls": {"state": "unknown"}}
    ]);
    let notices = development_controls_notices(&list);
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices[0].contains("deployment a"));
    assert!(notices[1].contains("deployment c") && notices[1].contains("unknown"));

    let mut installation = exposed();
    installation["profile"] = json!("vllm-default");
    installation.as_object_mut().unwrap().remove("residency");
    installation
        .as_object_mut()
        .unwrap()
        .remove("enable_sleep_mode");
    installation["applies_to"] = json!("parking_deployments");
    let hosts = json!({"api_version": "1", "hosts": [
        {"host_id": "h1", "name": "host-a", "development_controls":
            {"state": "exposed", "installations": [installation,
                {"profile": "vllm-off", "state": "not_exposed"}]}},
        {"host_id": "h2", "name": "host-b", "development_controls":
            {"state": "unknown", "installations": []}}
    ]});
    let notices = development_controls_notices(&hosts);
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices[0].contains("host host-a installation vllm-default"));
    assert!(
        notices[0].contains("for parking deployments"),
        "{}",
        notices[0]
    );
    assert!(notices[1].contains("host host-b") && notices[1].contains("unknown"));

    // A single inspected host is the same object without the wrapper.
    let single = hosts["hosts"][0].clone();
    assert_eq!(development_controls_notices(&single).len(), 1);
}

// T21: views without the mark (older servers, other commands) print nothing.
#[test]
fn views_without_the_mark_are_silent() {
    for view in [json!({"deployment_id": "d1"}), json!([]), json!(null)] {
        assert!(development_controls_notices(&view).is_empty());
    }
}

// T21, owner decision 2026-09-22: an SGLang deployment or installation is
// noted for its unauthenticated loopback `/metrics`, which the owner accepted;
// a view without the mark prints no such note.
#[test]
fn sglang_views_note_the_unauthenticated_loopback_metrics() {
    let surfaces = json!({"surface": ["/metrics"], "listener": "loopback", "access": "read_only"});
    let view = json!({"id": "d2", "name": "sg", "development_controls":
        {"state": "not_exposed", "engine": "sglang", "unauthenticated_local_surfaces": surfaces}});
    let notices = development_controls_notices(&view);
    assert_eq!(notices.len(), 1, "{notices:?}");
    let line = &notices[0];
    assert!(line.contains("deployment sg"), "{line}");
    assert!(line.contains("/metrics"), "{line}");
    assert!(line.contains("without authentication"), "{line}");
    assert!(
        line.contains("loopback") && line.contains("read_only"),
        "{line}"
    );

    let mut installation =
        json!({"profile": "sglang-default", "state": "not_exposed", "engine": "sglang"});
    installation["unauthenticated_local_surfaces"] = surfaces;
    let host = json!({"host_id": "h1", "name": "host-b", "development_controls":
        {"state": "not_exposed", "installations": [installation]}});
    let notices = development_controls_notices(&host);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("host host-b installation sglang-default"),
        "{}",
        notices[0]
    );
}

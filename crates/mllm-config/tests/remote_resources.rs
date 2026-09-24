use mllm_config::remote_resources::*;
use serde_json::json;
// T07, T16: identical local resource names cannot overlap across enrolled hosts.
#[test]
fn resource_names_are_disjoint_and_round_trip_without_inventing_fields() {
    let source = json!({"host":"spark","devices":[{"id":"gpu0"}],"resources":{"ready":{"allocations":[{"domain":"unified","bytes":20}],"devices":[{"id":"gpu0"}]}}});
    let a = scope_deployment_document("host-a", &source).unwrap();
    let b = scope_deployment_document("host-b", &source).unwrap();
    assert_ne!(a["devices"], b["devices"]);
    assert_ne!(a["resources"], b["resources"]);
    assert!(a["resources"].get("cold").is_none());
    let mut expected = source.clone();
    expected.as_object_mut().unwrap().remove("host");
    assert_eq!(local_deployment_document("host-a", &a).unwrap(), expected);
    assert!(local_deployment_document("host-b", &a).is_err());
    assert_eq!(
        scope_deployment_document("host-a", &json!({"name":"empty"})).unwrap(),
        json!({"name":"empty"})
    );
}

use capyctl_agent::{identity_storage::IdentityDirectory, ingress::IngressScope, ingress_identity};
use std::os::unix::fs::PermissionsExt;
// T13, T37: provisioning acknowledgement loss never rotates live credentials.
#[test]
fn private_credentials_survive_reopen_and_refuse_scope_or_gate_replacement() {
    let directory = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let scope = IngressScope {
        host_id: "host".into(),
        deployment_id: "deployment".into(),
        binding_id: "binding".into(),
        incarnation: "incarnation".into(),
        member_id: "head".into(),
        generation: 1,
        revision: 1,
        instance_index: 0,
    };
    let store = IdentityDirectory::open(directory.path()).unwrap();
    let first = ingress_identity::provision(&store, &scope, [9; 32], [1; 32]).unwrap();
    assert_ne!(first.inference, first.admin);
    assert_ne!(first.inference, first.gate);
    drop(store);
    let store = IdentityDirectory::open(directory.path()).unwrap();
    let replay = ingress_identity::provision(&store, &scope, [9; 32], [1; 32]).unwrap();
    assert_eq!(first.inference, replay.inference);
    assert_eq!(first.admin, replay.admin);
    assert!(ingress_identity::provision(&store, &scope, [9; 32], [2; 32]).is_err());
    let mut changed = scope.clone();
    changed.generation = 2;
    assert!(ingress_identity::load(&store, &changed, [9; 32]).is_err());
    assert!(ingress_identity::load(&store, &scope, [8; 32]).is_err());
    assert_eq!(
        ingress_identity::load(&store, &scope, [9; 32])
            .unwrap()
            .gate,
        [1; 32]
    );
}

// T34 (ADR 0013 §5): a credential bundle written before the ingress was keyed
// by instance names no instance. It still loads for the binding it was written
// for, whatever instance that binding realizes, so a retained launch keeps its
// credentials across the upgrade; a bundle that names an instance loads only
// for that instance.
#[test]
fn a_bundle_written_before_instances_were_keyed_still_loads_for_its_binding() {
    let directory = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let scope = IngressScope {
        host_id: "host".into(),
        deployment_id: "deployment".into(),
        binding_id: "binding".into(),
        incarnation: "incarnation".into(),
        member_id: "head".into(),
        generation: 1,
        revision: 1,
        instance_index: 1,
    };
    let store = IdentityDirectory::open(directory.path()).unwrap();
    let provisioned = ingress_identity::provision(&store, &scope, [9; 32], [1; 32]).unwrap();
    // Rewrite the bundle as the previous release wrote it: no instance.
    let file = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ingress-")
        })
        .unwrap();
    let mut bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(bundle["scope"]["instance_index"], 1);
    bundle["scope"]
        .as_object_mut()
        .unwrap()
        .remove("instance_index");
    std::fs::write(&file, serde_json::to_vec(&bundle).unwrap()).unwrap();
    let loaded = ingress_identity::load(&store, &scope, [9; 32]).unwrap();
    assert_eq!(loaded.inference, provisioned.inference);
    // A bundle that names its instance is bound to it.
    bundle["scope"]["instance_index"] = serde_json::json!(2);
    std::fs::write(&file, serde_json::to_vec(&bundle).unwrap()).unwrap();
    assert!(ingress_identity::load(&store, &scope, [9; 32]).is_err());
}

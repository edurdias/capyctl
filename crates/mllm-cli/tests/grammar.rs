use mllm_cli::grammar::{parse, parse_invocation, Command, InitTarget, LifecycleAction, ListResource, Resource, Role};

#[test]
fn action_first_grammar() {
    assert!(matches!(parse(["mllm", "start", "server"]),
        Ok(Command::Start(Role::Server))));
    assert!(matches!(parse(["mllm", "deploy", "model"]),
        Ok(Command::Deploy{activate: false, wait: false, ..})));
    assert!(matches!(parse(["mllm", "stop", "deployment", "dep_x"]),
        Ok(Command::Lifecycle{action: LifecycleAction::Stop, deployment}) if deployment == "dep_x"));
    assert!(parse(["mllm", "server", "run"]).is_err(), "legacy grammar rejected");
    assert!(parse(["mllm", "start", "power-on", "host-1"]).is_err(),
        "start targets roles, not remote machines");
}

#[test]
fn list_and_status_never_activate() {
    // grammar carries no activation side effect; enforced by Command shape
    // (Status/List carry no boolean activation flag at all).
    assert!(matches!(parse(["mllm", "list", "hosts"]),
        Ok(Command::List{resource: ListResource::Hosts})));
    assert!(matches!(parse(["mllm", "list", "deployments"]),
        Ok(Command::List{resource: ListResource::Deployments})));
    assert!(matches!(parse(["mllm", "status", "deployment", "dep_x"]),
        Ok(Command::Status{deployment, watch: false}) if deployment == "dep_x"));
    assert!(matches!(parse(["mllm", "status", "deployment", "dep_x", "--watch"]),
        Ok(Command::Status{watch: true, ..})));
}

#[test]
fn start_roles_with_config() {
    assert!(matches!(parse(["mllm", "start", "host"]),
        Ok(Command::Start(Role::Host))));
    assert!(matches!(parse(["mllm", "start", "standalone"]),
        Ok(Command::Start(Role::Standalone))));
    let inv = parse_invocation(["mllm", "start", "server", "--config", "server.yaml"]).unwrap();
    assert!(matches!(inv.command, Command::Start(Role::Server)));
    assert_eq!(inv.config.as_deref(), Some(std::path::Path::new("server.yaml")));
}

#[test]
fn init_targets() {
    assert!(matches!(parse(["mllm", "init", "server"]), Ok(Command::Init(InitTarget::Server))));
    assert!(matches!(parse(["mllm", "init", "host"]), Ok(Command::Init(InitTarget::Host))));
}

#[test]
fn invite_join_inspect_doctor_qualify() {
    let invite = parse(["mllm", "invite", "host", "--name", "host-a"]).unwrap();
    matches!(invite, Command::Invite{name} if name == "host-a");

    let join = parse(["mllm", "join", "host", "--join-file", "host-a.join"]).unwrap();
    matches!(join, Command::Join{join_file} if join_file == std::path::Path::new("host-a.join"));

    let inspect_host = parse(["mllm", "inspect", "host", "host-a"]).unwrap();
    matches!(inspect_host, Command::Inspect{resource: Resource::Host, id: Some(id), effective: false} if id == "host-a");

    let inspect_dep = parse(["mllm", "inspect", "deployment", "dep_x", "--effective-config"]).unwrap();
    matches!(inspect_dep, Command::Inspect{resource: Resource::Deployment, id: Some(id), effective: true} if id == "dep_x");

    let inspect_cfg = parse(["mllm", "inspect", "config", "--role", "host", "--effective"]).unwrap();
    matches!(inspect_cfg, Command::Inspect{resource: Resource::Config, id: Some(role), effective: true} if role == "host");

    assert!(matches!(parse(["mllm", "doctor", "host", "host-a"]),
        Ok(Command::Doctor{host}) if host == "host-a"));
    assert!(matches!(parse(["mllm", "qualify", "deployment", "dep_example"]),
        Ok(Command::Qualify{deployment}) if deployment == "dep_example"));
}

#[test]
fn deploy_flags() {
    let c = parse(["mllm", "deploy", "model", "--file", "d.yaml", "--activate", "--wait"]).unwrap();
    matches!(c, Command::Deploy{file: Some(f), activate: true, wait: true} if f == std::path::Path::new("d.yaml"));
}

#[test]
fn lifecycle_forms() {
    assert!(matches!(parse(["mllm", "start", "deployment", "dep_x"]),
        Ok(Command::Lifecycle{action: LifecycleAction::Start, deployment}) if deployment == "dep_x"));
    assert!(matches!(parse(["mllm", "park", "deployment", "dep_x"]),
        Ok(Command::Lifecycle{action: LifecycleAction::Park, ..})));
    assert!(matches!(parse(["mllm", "preinitialize", "deployment", "dep_x"]),
        Ok(Command::Lifecycle{action: LifecycleAction::Preinitialize, ..})));
    assert!(matches!(parse(["mllm", "undeploy", "model", "dep_x"]),
        Ok(Command::Lifecycle{action: LifecycleAction::Undeploy, deployment}) if deployment == "dep_x"));
}

#[test]
fn validate_config_file() {
    let c = parse(["mllm", "validate", "config", "--file", "host.yaml"]).unwrap();
    matches!(c, Command::Validate{file} if file == std::path::Path::new("host.yaml"));
}

#[test]
fn machine_mode_output_flag() {
    let inv = parse_invocation(["mllm", "deploy", "model", "--output", "json"]).unwrap();
    assert_eq!(inv.output.as_deref(), Some("json"));
    let inv = parse_invocation(["mllm", "status", "deployment", "dep_x", "--output", "text"]).unwrap();
    assert_eq!(inv.output.as_deref(), Some("text"));
}

#[test]
fn malformed_invocations_rejected() {
    assert!(parse(["mllm", "start"]).is_err(), "start requires a role");
    assert!(parse(["mllm", "stop", "model", "dep_x"]).is_err(), "stop targets deployments");
    assert!(parse(["mllm", "undeploy", "deployment", "dep_x"]).is_err(), "undeploy targets models");
    assert!(parse(["mllm", "deploy", "model", "--activate", "--file"]).is_err(), "--file needs a value");
    assert!(parse(["mllm"]).is_err(), "bare invocation needs an action");
}
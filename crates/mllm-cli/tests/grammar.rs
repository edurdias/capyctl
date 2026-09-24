use mllm_cli::grammar::{
    parse, parse_invocation, Command, InitTarget, LifecycleAction, ListResource, Resource, Role,
};

#[test]
fn action_first_grammar() {
    assert!(matches!(
        parse(["mllm", "start", "server"]),
        Ok(Command::Start(Role::Server))
    ));
    assert!(matches!(
        parse(["mllm", "deploy", "model"]),
        Ok(Command::Deploy {
            activate: false,
            wait: false,
            ..
        })
    ));
    assert!(matches!(parse(["mllm", "stop", "deployment", "dep_x"]),
        Ok(Command::Lifecycle{action: LifecycleAction::Stop, deployment}) if deployment == "dep_x"));
    assert!(
        parse(["mllm", "server", "run"]).is_err(),
        "legacy grammar rejected"
    );
    assert!(
        parse(["mllm", "start", "power-on", "host-1"]).is_err(),
        "start targets roles, not remote machines"
    );
}

#[test]
fn list_and_status_never_activate() {
    // grammar carries no activation side effect; enforced by Command shape
    // (Status/List carry no boolean activation flag at all).
    assert!(matches!(
        parse(["mllm", "list", "hosts"]),
        Ok(Command::List {
            resource: ListResource::Hosts
        })
    ));
    assert!(matches!(
        parse(["mllm", "list", "deployments"]),
        Ok(Command::List {
            resource: ListResource::Deployments
        })
    ));
    assert!(matches!(parse(["mllm", "status", "deployment", "dep_x"]),
        Ok(Command::Status{deployment, watch: false}) if deployment == "dep_x"));
    assert!(matches!(
        parse(["mllm", "status", "deployment", "dep_x", "--watch"]),
        Ok(Command::Status { watch: true, .. })
    ));
}

#[test]
fn start_roles_with_config() {
    assert!(matches!(
        parse(["mllm", "start", "host"]),
        Ok(Command::Start(Role::Host))
    ));
    assert!(matches!(
        parse(["mllm", "start", "standalone"]),
        Ok(Command::Start(Role::Standalone))
    ));
    let inv = parse_invocation(["mllm", "start", "server", "--config", "server.yaml"]).unwrap();
    assert!(matches!(inv.command, Command::Start(Role::Server)));
    assert_eq!(
        inv.config.as_deref(),
        Some(std::path::Path::new("server.yaml"))
    );
}

#[test]
fn init_targets() {
    assert!(matches!(
        parse(["mllm", "init", "server"]),
        Ok(Command::Init(InitTarget::Server))
    ));
    assert!(matches!(
        parse(["mllm", "init", "host"]),
        Ok(Command::Init(InitTarget::Host))
    ));
}

#[test]
fn invite_join_inspect_doctor() {
    let invite = parse(["mllm", "invite", "host", "--name", "host-a"]).unwrap();
    assert!(matches!(invite, Command::Invite{name, recover: false} if name == "host-a"));

    let join = parse(["mllm", "join", "host", "--join-file", "host-a.join"]).unwrap();
    assert!(
        matches!(join, Command::Join{join_file, recover: false} if join_file == std::path::Path::new("host-a.join"))
    );

    // T05 T06 (ADR 0016): recovery is explicit on both sides; the host is
    // named positionally or with --name, never both.
    let recover = parse(["mllm", "invite", "host", "host-a", "--recover"]).unwrap();
    assert!(matches!(recover, Command::Invite{name, recover: true} if name == "host-a"));
    let recover = parse(["mllm", "invite", "host", "--name", "host-a", "--recover"]).unwrap();
    assert!(matches!(recover, Command::Invite{name, recover: true} if name == "host-a"));
    assert!(parse(["mllm", "invite", "host", "host-a", "--name", "host-a"]).is_err());
    assert!(parse(["mllm", "invite", "host", "--recover"]).is_err());
    let join = parse(["mllm", "join", "host", "--join-file", "host-a.join", "--recover"]).unwrap();
    assert!(matches!(join, Command::Join{recover: true, ..}));

    let inspect_host = parse(["mllm", "inspect", "host", "host-a"]).unwrap();
    assert!(
        matches!(inspect_host, Command::Inspect{resource: Resource::Host, id: Some(id), effective: false} if id == "host-a")
    );

    let inspect_dep = parse([
        "mllm",
        "inspect",
        "deployment",
        "dep_x",
        "--effective-config",
    ])
    .unwrap();
    assert!(
        matches!(inspect_dep, Command::Inspect{resource: Resource::Deployment, id: Some(id), effective: true} if id == "dep_x")
    );

    let inspect_cfg =
        parse(["mllm", "inspect", "config", "--role", "host", "--effective"]).unwrap();
    assert!(
        matches!(inspect_cfg, Command::Inspect{resource: Resource::Config, id: Some(role), effective: true} if role == "host")
    );

    assert!(matches!(parse(["mllm", "doctor", "host", "host-a"]),
        Ok(Command::Doctor{host}) if host == "host-a"));
}

#[test]
fn deploy_flags() {
    let c = parse([
        "mllm",
        "deploy",
        "model",
        "--file",
        "d.yaml",
        "--activate",
        "--wait",
    ])
    .unwrap();
    assert!(
        matches!(c, Command::Deploy{file: Some(f), activate: true, wait: true, revision: None} if f == std::path::Path::new("d.yaml"))
    );
}

#[test]
fn lifecycle_forms() {
    assert!(matches!(parse(["mllm", "start", "deployment", "dep_x"]),
        Ok(Command::Lifecycle{action: LifecycleAction::Start, deployment}) if deployment == "dep_x"));
    assert!(matches!(
        parse(["mllm", "park", "deployment", "dep_x"]),
        Ok(Command::Lifecycle {
            action: LifecycleAction::Park,
            ..
        })
    ));
    assert!(matches!(
        parse(["mllm", "preinitialize", "deployment", "dep_x"]),
        Ok(Command::Lifecycle {
            action: LifecycleAction::Preinitialize,
            ..
        })
    ));
}

// T09 T10, SPEC §6.3 (owner decision 2026-09-23): `delete deployment` names
// the object removed; `--stop` stops every instance first. `undeploy` is gone.
#[test]
fn delete_deployment_forms() {
    assert!(matches!(parse(["mllm", "delete", "deployment", "dep_x"]),
        Ok(Command::Delete{deployment, stop: false}) if deployment == "dep_x"));
    assert!(matches!(parse(["mllm", "delete", "deployment", "dep_x", "--stop"]),
        Ok(Command::Delete{deployment, stop: true}) if deployment == "dep_x"));
    let request = ulid::Ulid::new().to_string();
    let inv = parse_invocation(["mllm", "delete", "deployment", "dep_x", "--stop", "--request-id", &request]).unwrap();
    assert_eq!(inv.request_id.as_deref(), Some(request.as_str()));
    assert_eq!(inv.command.label(), "delete deployment dep_x --stop");
    assert!(parse(["mllm", "undeploy", "model", "dep_x"]).is_err(), "undeploy was dropped");
    assert!(parse(["mllm", "delete", "model", "dep_x"]).is_err(), "delete targets deployments");
    assert!(parse(["mllm", "delete", "deployment"]).is_err(), "delete needs a deployment");
}

// T09 T13, SPEC §6.4: a request identity is one ULID however it is spelled.
// A lowercase (or otherwise non-canonical) retry of the printed identity must
// recover the same journal and idempotency key, not open a second request.
#[test]
fn request_identity_is_canonicalized() {
    let request = ulid::Ulid::new().to_string();
    let lower = request.to_ascii_lowercase();
    let inv = parse_invocation(["mllm", "start", "deployment", "dep_x", "--request-id", &lower]).unwrap();
    assert_eq!(inv.request_id.as_deref(), Some(request.as_str()));
}

#[test]
fn validate_config_file() {
    let c = parse(["mllm", "validate", "config", "--file", "host.yaml"]).unwrap();
    assert!(matches!(c, Command::Validate{file, host: None} if file == std::path::Path::new("host.yaml")));
    let c = parse(["mllm", "validate", "config", "--file", "d.yaml", "--host", "h.yaml"]).unwrap();
    assert!(matches!(c, Command::Validate{file, host: Some(host)}
        if file == std::path::Path::new("d.yaml") && host == std::path::Path::new("h.yaml")));
}

#[test]
fn machine_mode_output_flag() {
    let inv = parse_invocation(["mllm", "deploy", "model", "--output", "json"]).unwrap();
    assert_eq!(inv.output.as_deref(), Some("json"));
    let inv =
        parse_invocation(["mllm", "status", "deployment", "dep_x", "--output", "text"]).unwrap();
    assert_eq!(inv.output.as_deref(), Some("text"));
}

#[test]
fn malformed_invocations_rejected() {
    assert!(parse(["mllm", "start"]).is_err(), "start requires a role");
    assert!(
        parse(["mllm", "stop", "model", "dep_x"]).is_err(),
        "stop targets deployments"
    );
    assert!(
        parse(["mllm", "undeploy", "deployment", "dep_x"]).is_err(),
        "undeploy is not a verb"
    );
    assert!(
        parse(["mllm", "deploy", "model", "--activate", "--file"]).is_err(),
        "--file needs a value"
    );
    assert!(parse(["mllm"]).is_err(), "bare invocation needs an action");
}

// T22: full native logs require an explicit standalone-only operator flag.
#[test]
fn debug_engine_logs_are_explicit_and_scoped_to_standalone() {
    assert!(!parse_invocation(["mllm", "start", "standalone"]).unwrap().debug_engine_logs);
    assert!(parse_invocation(["mllm", "start", "standalone", "--debug-engine-logs"]).unwrap().debug_engine_logs);
    assert!(parse_invocation(["mllm", "start", "deployment", "d", "--debug-engine-logs"]).is_err());
    assert!(parse_invocation(["mllm", "status", "deployment", "d", "--debug-engine-logs"]).is_err());
}

// SPEC §14 (deploy model): updating an existing deployment requires an
// explicit revision-aware operation. `--revision` names the revision the
// update replaces; it cannot be combined with activation (a count change
// keeps running instances, any other change restarts them, ADR 0013 §7).
// Found live 2026-09-23: the matrix drove count changes through the raw
// management API because the CLI had no way to revise a deployment.
// T08 T09
#[test]
fn deploy_model_accepts_an_explicit_expected_revision() {
    let revise = |extra: &[&'static str]| {
        let mut argv = vec!["mllm", "deploy", "model", "--file", "d.yaml"];
        argv.extend_from_slice(extra);
        parse(argv)
    };
    assert!(matches!(
        revise(&["--revision", "3"]),
        Ok(Command::Deploy {
            revision: Some(3),
            activate: false,
            ..
        })
    ));
    assert!(matches!(
        revise(&[]),
        Ok(Command::Deploy { revision: None, .. })
    ));
    assert!(revise(&["--revision", "3", "--activate"]).is_err());
    assert!(revise(&["--revision", "0"]).is_err());
}

/// SPEC §6.4: `start deployment` and `start instance` take `--wait`, which
/// observes the start's operation to its end; no other command takes it that
/// way, and without it a start returns at acceptance.
// T08
#[test]
fn start_takes_wait() {
    assert!(parse_invocation(["mllm", "start", "deployment", "d", "--wait"]).unwrap().wait);
    assert!(parse_invocation(["mllm", "start", "instance", "d/0", "--wait", "--evict"]).unwrap().wait);
    assert!(!parse_invocation(["mllm", "start", "deployment", "d"]).unwrap().wait);
    assert!(!parse_invocation(["mllm", "deploy", "model", "--file", "d.yaml", "--activate", "--wait"]).unwrap().wait);
    assert!(parse_invocation(["mllm", "stop", "deployment", "d", "--wait"]).is_err());
}

/// SPEC §§4.1, 13.3, 14: `revoke host <name|id>` is an action-first verb that
/// takes a request identity like every other mutation.
// T01 T06
#[test]
fn revoke_host_parses_with_a_request_identity() {
    assert_eq!(
        parse(["mllm", "revoke", "host", "host-a"]).unwrap(),
        Command::Revoke { host: "host-a".into() }
    );
    let id = ulid::Ulid::new().to_string();
    let invocation =
        parse_invocation(["mllm", "revoke", "host", "host-a", "--request-id", &id]).unwrap();
    assert_eq!(invocation.request_id.as_deref(), Some(id.as_str()));
    assert_eq!(invocation.command.label(), "revoke host host-a");
    assert!(parse(["mllm", "revoke", "host"]).is_err());
    assert!(parse(["mllm", "revoke", "deployment", "d"]).is_err());
}

/// SPEC §6.3, ADR 0008: `prune sources` is explicit: it names the host
/// document whose store it prunes and only lists unless `--apply` is given.
// T01
#[test]
fn prune_sources_parses_and_lists_by_default() {
    assert_eq!(
        parse(["mllm", "prune", "sources", "--host-config", "host.yaml"]).unwrap(),
        Command::PruneSources {
            host_config: "host.yaml".into(),
            apply: false,
            referenced_file: None,
        }
    );
    let applied = parse(["mllm", "prune", "sources", "--host-config", "h.yaml", "--apply",
        "--referenced-file", "refs.json"]).unwrap();
    assert_eq!(applied.label(), "prune sources --apply");
    assert!(matches!(applied, Command::PruneSources { apply: true, referenced_file: Some(_), .. }));
    assert!(parse(["mllm", "prune", "sources"]).is_err(), "the store is named explicitly");
    assert!(parse(["mllm", "prune", "deployment", "d"]).is_err());
}

/// SPEC §6.4: `--wait` observes the accepted target operation. A deploy
/// without `--activate` has no operation beyond its durable acceptance, which
/// it already returns after, so `--wait` alone is refused with the reason
/// instead of being silently ignored.
// T08
#[test]
fn deploy_wait_without_activate_is_refused_with_its_reason() {
    let refused = parse_invocation(["mllm", "deploy", "model", "--file", "d.yaml", "--wait"])
        .expect_err("deploy --wait without --activate was accepted");
    let message = refused.to_string();
    assert!(message.contains("--wait requires --activate"), "{message}");
    assert!(parse_invocation(["mllm", "deploy", "model", "--file", "d.yaml", "--activate", "--wait"]).is_ok());
    assert!(parse_invocation(["mllm", "deploy", "model", "--file", "d.yaml"]).is_ok());
    assert!(parse_invocation(["mllm", "deploy", "model", "--file", "d.yaml", "--revision", "2", "--wait"]).is_err());
}

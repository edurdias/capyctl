use capyctl_cli::grammar::{
    parse, parse_invocation, Command, InitTarget, LifecycleAction, ListResource, Resource, Role,
};

#[test]
fn action_first_grammar() {
    assert!(matches!(
        parse(["capyctl", "start", "server"]),
        Ok(Command::Start(Role::Server))
    ));
    assert!(matches!(
        parse(["capyctl", "deploy", "model"]),
        Ok(Command::Deploy {
            activate: false,
            wait: false,
            ..
        })
    ));
    assert!(matches!(parse(["capyctl", "stop", "deployment", "dep_x"]),
        Ok(Command::Lifecycle{action: LifecycleAction::Stop, deployment}) if deployment == "dep_x"));
    assert!(
        parse(["capyctl", "server", "run"]).is_err(),
        "legacy grammar rejected"
    );
    assert!(
        parse(["capyctl", "start", "power-on", "host-1"]).is_err(),
        "start targets roles, not remote machines"
    );
}

#[test]
fn list_and_status_never_activate() {
    // grammar carries no activation side effect; enforced by Command shape
    // (Status/List carry no boolean activation flag at all).
    assert!(matches!(
        parse(["capyctl", "list", "hosts"]),
        Ok(Command::List {
            resource: ListResource::Hosts
        })
    ));
    assert!(matches!(
        parse(["capyctl", "list", "deployments"]),
        Ok(Command::List {
            resource: ListResource::Deployments
        })
    ));
    assert!(
        matches!(parse(["capyctl", "status", "deployment", "dep_x"]),
        Ok(Command::Status{deployment, watch: false}) if deployment == "dep_x")
    );
    assert!(matches!(
        parse(["capyctl", "status", "deployment", "dep_x", "--watch"]),
        Ok(Command::Status { watch: true, .. })
    ));
}

#[test]
fn start_roles_with_config() {
    assert!(matches!(
        parse(["capyctl", "start", "host"]),
        Ok(Command::Start(Role::Host))
    ));
    assert!(matches!(
        parse(["capyctl", "start", "standalone"]),
        Ok(Command::Start(Role::Standalone))
    ));
    let inv = parse_invocation(["capyctl", "start", "server", "--config", "server.yaml"]).unwrap();
    assert!(matches!(inv.command, Command::Start(Role::Server)));
    assert_eq!(
        inv.config.as_deref(),
        Some(std::path::Path::new("server.yaml"))
    );
}

#[test]
fn init_targets() {
    assert!(matches!(
        parse(["capyctl", "init", "server"]),
        Ok(Command::Init(InitTarget::Server))
    ));
    assert!(matches!(
        parse(["capyctl", "init", "host"]),
        Ok(Command::Init(InitTarget::Host))
    ));
}

#[test]
fn invite_join_inspect_doctor() {
    let invite = parse(["capyctl", "invite", "host", "--name", "host-a"]).unwrap();
    assert!(matches!(invite, Command::Invite{name, recover: false} if name == "host-a"));

    let join = parse(["capyctl", "join", "host", "--join-file", "host-a.join"]).unwrap();
    assert!(
        matches!(join, Command::Join{join_file, recover: false} if join_file == std::path::Path::new("host-a.join"))
    );

    // T05 T06 (ADR 0016): recovery is explicit on both sides; the host is
    // named positionally or with --name, never both.
    let recover = parse(["capyctl", "invite", "host", "host-a", "--recover"]).unwrap();
    assert!(matches!(recover, Command::Invite{name, recover: true} if name == "host-a"));
    let recover = parse(["capyctl", "invite", "host", "--name", "host-a", "--recover"]).unwrap();
    assert!(matches!(recover, Command::Invite{name, recover: true} if name == "host-a"));
    assert!(parse(["capyctl", "invite", "host", "host-a", "--name", "host-a"]).is_err());
    assert!(parse(["capyctl", "invite", "host", "--recover"]).is_err());
    let join = parse([
        "capyctl",
        "join",
        "host",
        "--join-file",
        "host-a.join",
        "--recover",
    ])
    .unwrap();
    assert!(matches!(join, Command::Join { recover: true, .. }));

    let inspect_host = parse(["capyctl", "inspect", "host", "host-a"]).unwrap();
    assert!(
        matches!(inspect_host, Command::Inspect{resource: Resource::Host, id: Some(id), effective: false} if id == "host-a")
    );

    let inspect_dep = parse([
        "capyctl",
        "inspect",
        "deployment",
        "dep_x",
        "--effective-config",
    ])
    .unwrap();
    assert!(
        matches!(inspect_dep, Command::Inspect{resource: Resource::Deployment, id: Some(id), effective: true} if id == "dep_x")
    );

    let inspect_cfg = parse([
        "capyctl",
        "inspect",
        "config",
        "--role",
        "host",
        "--effective",
    ])
    .unwrap();
    assert!(
        matches!(inspect_cfg, Command::Inspect{resource: Resource::Config, id: Some(role), effective: true} if role == "host")
    );

    assert!(matches!(parse(["capyctl", "doctor", "host", "host-a"]),
        Ok(Command::Doctor{host}) if host == "host-a"));
}

#[test]
fn deploy_flags() {
    let c = parse([
        "capyctl",
        "deploy",
        "model",
        "--file",
        "d.yaml",
        "--activate",
        "--wait",
    ])
    .unwrap();
    assert!(
        matches!(c, Command::Deploy{file: Some(f), activate: true, wait: true, revision: None, hf_endpoint: None} if f == std::path::Path::new("d.yaml"))
    );
}

#[test]
fn lifecycle_forms() {
    assert!(matches!(parse(["capyctl", "start", "deployment", "dep_x"]),
        Ok(Command::Lifecycle{action: LifecycleAction::Start, deployment}) if deployment == "dep_x"));
    assert!(matches!(
        parse(["capyctl", "park", "deployment", "dep_x"]),
        Ok(Command::Lifecycle {
            action: LifecycleAction::Park,
            ..
        })
    ));
    assert!(matches!(
        parse(["capyctl", "preinitialize", "deployment", "dep_x"]),
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
    assert!(
        matches!(parse(["capyctl", "delete", "deployment", "dep_x"]),
        Ok(Command::Delete{deployment, stop: false}) if deployment == "dep_x")
    );
    assert!(
        matches!(parse(["capyctl", "delete", "deployment", "dep_x", "--stop"]),
        Ok(Command::Delete{deployment, stop: true}) if deployment == "dep_x")
    );
    let request = ulid::Ulid::new().to_string();
    let inv = parse_invocation([
        "capyctl",
        "delete",
        "deployment",
        "dep_x",
        "--stop",
        "--request-id",
        &request,
    ])
    .unwrap();
    assert_eq!(inv.request_id.as_deref(), Some(request.as_str()));
    assert_eq!(inv.command.label(), "delete deployment dep_x --stop");
    assert!(
        parse(["capyctl", "undeploy", "model", "dep_x"]).is_err(),
        "undeploy was dropped"
    );
    assert!(
        parse(["capyctl", "delete", "model", "dep_x"]).is_err(),
        "delete targets deployments"
    );
    assert!(
        parse(["capyctl", "delete", "deployment"]).is_err(),
        "delete needs a deployment"
    );
}

// T09 T13, SPEC §6.4: a request identity is one ULID however it is spelled.
// A lowercase (or otherwise non-canonical) retry of the printed identity must
// recover the same journal and idempotency key, not open a second request.
#[test]
fn request_identity_is_canonicalized() {
    let request = ulid::Ulid::new().to_string();
    let lower = request.to_ascii_lowercase();
    let inv = parse_invocation([
        "capyctl",
        "start",
        "deployment",
        "dep_x",
        "--request-id",
        &lower,
    ])
    .unwrap();
    assert_eq!(inv.request_id.as_deref(), Some(request.as_str()));
}

#[test]
fn validate_config_file() {
    let c = parse(["capyctl", "validate", "config", "--file", "host.yaml"]).unwrap();
    assert!(
        matches!(c, Command::Validate{file, host: None, ..} if file == std::path::Path::new("host.yaml"))
    );
    let c = parse([
        "capyctl", "validate", "config", "--file", "d.yaml", "--host", "h.yaml",
    ])
    .unwrap();
    assert!(matches!(c, Command::Validate{file, host: Some(host), ..}
        if file == std::path::Path::new("d.yaml") && host == std::path::Path::new("h.yaml")));
}

#[test]
fn machine_mode_output_flag() {
    let inv = parse_invocation(["capyctl", "deploy", "model", "--output", "json"]).unwrap();
    assert_eq!(inv.output.as_deref(), Some("json"));
    let inv = parse_invocation([
        "capyctl",
        "status",
        "deployment",
        "dep_x",
        "--output",
        "text",
    ])
    .unwrap();
    assert_eq!(inv.output.as_deref(), Some("text"));
}

// Owner decision 2026-09-25: `--format table|json`, `--json` for short;
// anything else, or both at once, is a usage error.
#[test]
fn format_flag_selects_table_or_json() {
    let format = |args: &[&str]| parse_invocation(args).map(|inv| inv.format);
    assert_eq!(format(&["capyctl", "list", "hosts"]).unwrap(), None);
    assert_eq!(
        format(&["capyctl", "list", "hosts", "--format", "json"])
            .unwrap()
            .as_deref(),
        Some("json")
    );
    assert_eq!(
        format(&["capyctl", "--format", "table", "list", "deployments"])
            .unwrap()
            .as_deref(),
        Some("table")
    );
    assert_eq!(
        format(&["capyctl", "engine", "list", "--json"])
            .unwrap()
            .as_deref(),
        Some("json")
    );
    assert!(format(&["capyctl", "list", "hosts", "--format", "yaml"]).is_err());
    assert!(format(&["capyctl", "list", "hosts", "--format", "table", "--json"]).is_err());
}

#[test]
fn malformed_invocations_rejected() {
    assert!(
        parse(["capyctl", "start"]).is_err(),
        "start requires a role"
    );
    assert!(
        parse(["capyctl", "stop", "model", "dep_x"]).is_err(),
        "stop targets deployments"
    );
    assert!(
        parse(["capyctl", "undeploy", "deployment", "dep_x"]).is_err(),
        "undeploy is not a verb"
    );
    assert!(
        parse(["capyctl", "deploy", "model", "--activate", "--file"]).is_err(),
        "--file needs a value"
    );
    assert!(
        parse(["capyctl"]).is_err(),
        "bare invocation needs an action"
    );
}

// T22: full native logs require an explicit standalone-only operator flag.
#[test]
fn debug_engine_logs_are_explicit_and_scoped_to_standalone() {
    assert!(
        !parse_invocation(["capyctl", "start", "standalone"])
            .unwrap()
            .debug_engine_logs
    );
    assert!(
        parse_invocation(["capyctl", "start", "standalone", "--debug-engine-logs"])
            .unwrap()
            .debug_engine_logs
    );
    assert!(
        parse_invocation(["capyctl", "start", "deployment", "d", "--debug-engine-logs"]).is_err()
    );
    assert!(parse_invocation([
        "capyctl",
        "status",
        "deployment",
        "d",
        "--debug-engine-logs"
    ])
    .is_err());
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
        let mut argv = vec!["capyctl", "deploy", "model", "--file", "d.yaml"];
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
    assert!(
        parse_invocation(["capyctl", "start", "deployment", "d", "--wait"])
            .unwrap()
            .wait
    );
    assert!(
        parse_invocation(["capyctl", "start", "instance", "d/0", "--wait", "--evict"])
            .unwrap()
            .wait
    );
    assert!(
        !parse_invocation(["capyctl", "start", "deployment", "d"])
            .unwrap()
            .wait
    );
    assert!(
        !parse_invocation([
            "capyctl",
            "deploy",
            "model",
            "--file",
            "d.yaml",
            "--activate",
            "--wait"
        ])
        .unwrap()
        .wait
    );
    assert!(parse_invocation(["capyctl", "stop", "deployment", "d", "--wait"]).is_err());
}

/// SPEC §§4.1, 13.3, 14: `revoke host <name|id>` is an action-first verb that
/// takes a request identity like every other mutation.
// T01 T06
#[test]
fn revoke_host_parses_with_a_request_identity() {
    assert_eq!(
        parse(["capyctl", "revoke", "host", "host-a"]).unwrap(),
        Command::Revoke {
            host: "host-a".into()
        }
    );
    let id = ulid::Ulid::new().to_string();
    let invocation =
        parse_invocation(["capyctl", "revoke", "host", "host-a", "--request-id", &id]).unwrap();
    assert_eq!(invocation.request_id.as_deref(), Some(id.as_str()));
    assert_eq!(invocation.command.label(), "revoke host host-a");
    assert!(parse(["capyctl", "revoke", "host"]).is_err());
    assert!(parse(["capyctl", "revoke", "deployment", "d"]).is_err());
}

/// SPEC §6.3, ADR 0008: `prune sources` is explicit: it names the host
/// document whose store it prunes and only lists unless `--apply` is given.
// T01
#[test]
fn prune_sources_parses_and_lists_by_default() {
    assert_eq!(
        parse(["capyctl", "prune", "sources", "--host-config", "host.yaml"]).unwrap(),
        Command::PruneSources {
            host_config: "host.yaml".into(),
            apply: false,
            referenced_file: None,
        }
    );
    let applied = parse([
        "capyctl",
        "prune",
        "sources",
        "--host-config",
        "h.yaml",
        "--apply",
        "--referenced-file",
        "refs.json",
    ])
    .unwrap();
    assert_eq!(applied.label(), "prune sources --apply");
    assert!(matches!(
        applied,
        Command::PruneSources {
            apply: true,
            referenced_file: Some(_),
            ..
        }
    ));
    assert!(
        parse(["capyctl", "prune", "sources"]).is_err(),
        "the store is named explicitly"
    );
    assert!(parse(["capyctl", "prune", "deployment", "d"]).is_err());
}

/// SPEC §6.4: `--wait` observes the accepted target operation. A deploy
/// without `--activate` has no operation beyond its durable acceptance, which
/// it already returns after, so `--wait` alone is refused with the reason
/// instead of being silently ignored.
// T08
#[test]
fn deploy_wait_without_activate_is_refused_with_its_reason() {
    let refused = parse_invocation(["capyctl", "deploy", "model", "--file", "d.yaml", "--wait"])
        .expect_err("deploy --wait without --activate was accepted");
    let message = refused.to_string();
    assert!(message.contains("--wait requires --activate"), "{message}");
    assert!(parse_invocation([
        "capyctl",
        "deploy",
        "model",
        "--file",
        "d.yaml",
        "--activate",
        "--wait"
    ])
    .is_ok());
    assert!(parse_invocation(["capyctl", "deploy", "model", "--file", "d.yaml"]).is_ok());
    assert!(parse_invocation([
        "capyctl",
        "deploy",
        "model",
        "--file",
        "d.yaml",
        "--revision",
        "2",
        "--wait"
    ])
    .is_err());
}

use capyctl_cli::grammar::{DeepParkChoice, DriftChoice};

// T01 (ADR 0018 §1): the engine commands and `list engines` parse strictly.
#[test]
fn engine_commands_parse() {
    assert_eq!(
        parse([
            "capyctl",
            "engine",
            "add",
            "/v",
            "--name",
            "vllm-patched",
            "--deep-park",
            "disabled",
            "--drift",
            "refuse",
            "--arg",
            "--max-num-seqs",
            "--arg",
            "8",
            "--approve-option",
            "--speculative-config",
            "--approve-path",
            "/srv/drafters",
            "--approve-path",
            "/srv/other"
        ])
        .unwrap(),
        Command::EngineAdd {
            path: Some("/v".into()),
            name: Some("vllm-patched".into()),
            deep_park: Some(DeepParkChoice::Disabled),
            drift: DriftChoice::Refuse,
            args: vec!["--max-num-seqs".into(), "8".into()],
            approved_options: vec!["--speculative-config".into()],
            approved_paths: vec!["/srv/drafters".into(), "/srv/other".into()],
        }
    );
    assert_eq!(
        parse(["capyctl", "engine", "add"]).unwrap(),
        Command::EngineAdd {
            path: None,
            name: None,
            deep_park: None,
            drift: DriftChoice::Warn,
            args: vec![],
            approved_options: vec![],
            approved_paths: vec![]
        }
    );
    assert_eq!(
        parse(["capyctl", "engine", "detect", "--path", "/a", "--path", "/b"]).unwrap(),
        Command::EngineDetect {
            paths: vec!["/a".into(), "/b".into()]
        }
    );
    assert_eq!(
        parse(["capyctl", "engine", "list"]).unwrap(),
        Command::EngineList
    );
    assert_eq!(
        parse(["capyctl", "engine", "remove", "vllm", "--drain"]).unwrap(),
        Command::EngineRemove {
            name: "vllm".into(),
            drain: true
        }
    );
    assert_eq!(
        parse(["capyctl", "list", "engines"]).unwrap(),
        Command::List {
            resource: ListResource::Engines
        }
    );
    assert!(parse(["capyctl", "engine", "add", "--deep-park", "maybe"]).is_err());
    assert!(parse(["capyctl", "engine", "remove"]).is_err());
    assert_eq!(
        parse(["capyctl", "engine", "remove", "vllm"])
            .unwrap()
            .label(),
        "engine remove vllm"
    );
    assert_eq!(
        parse(["capyctl", "list", "engines"]).unwrap().label(),
        "list engines"
    );
}

// T01 (design §9): `--listen <addr:port>` narrows the inference bind of
// `start standalone` and `start server`, and of no other command.
#[test]
fn listen_is_parsed_on_start_standalone_and_server() {
    let i = parse_invocation([
        "capyctl",
        "start",
        "standalone",
        "--listen",
        "100.64.0.5:8443",
    ])
    .unwrap();
    assert!(matches!(i.command, Command::Start(Role::Standalone)));
    assert_eq!(i.listen, Some("100.64.0.5:8443".parse().unwrap()));
    let i = parse_invocation(["capyctl", "start", "server", "--listen", "[::]:9443"]).unwrap();
    assert!(matches!(i.command, Command::Start(Role::Server)));
    assert_eq!(i.listen, Some("[::]:9443".parse().unwrap()));
    assert_eq!(
        parse_invocation(["capyctl", "start", "standalone"])
            .unwrap()
            .listen,
        None
    );
    for bad in ["bad", "0.0.0.0:0", "224.0.0.1:8443", "0.0.0.0"] {
        assert!(
            parse_invocation(["capyctl", "start", "standalone", "--listen", bad]).is_err(),
            "{bad}"
        );
    }
    assert!(parse_invocation(["capyctl", "start", "host", "--listen", "0.0.0.0:1"]).is_err());
    assert!(parse_invocation(["capyctl", "status", "--listen", "0.0.0.0:1"]).is_err());
}

// T01 T37 (design §9): `--no-inference-auth` turns the inference key off for
// one run of `start standalone` and `start server`, and of no other command.
#[test]
fn no_inference_auth_is_parsed_on_start_standalone_and_server() {
    let i = parse_invocation(["capyctl", "start", "standalone", "--no-inference-auth"]).unwrap();
    assert!(matches!(i.command, Command::Start(Role::Standalone)));
    assert!(i.no_inference_auth);
    let i = parse_invocation(["capyctl", "start", "server", "--no-inference-auth"]).unwrap();
    assert!(matches!(i.command, Command::Start(Role::Server)));
    assert!(i.no_inference_auth);
    assert!(
        !parse_invocation(["capyctl", "start", "standalone"])
            .unwrap()
            .no_inference_auth
    );
    assert!(parse_invocation(["capyctl", "start", "host", "--no-inference-auth"]).is_err());
    assert!(parse_invocation(["capyctl", "status", "--no-inference-auth"]).is_err());
}

// T01 T03 (owner decision 2026-09-25): `--models-root`, `--model-sources` and
// `--model-sources-max` on `start standalone` and `start host`, the roles
// that hold a model store; malformed values are refused at parse time.
#[test]
fn model_flags_are_parsed_on_start_standalone_and_host() {
    use capyctl_config::model_source::SourceSwitch;
    for role in ["standalone", "host"] {
        let i = parse_invocation([
            "capyctl",
            "start",
            role,
            "--models-root",
            "/data/models",
            "--model-sources",
            "disabled",
            "--model-sources-max",
            "100GiB",
        ])
        .unwrap();
        assert_eq!(
            i.model_overrides.models_root.as_deref(),
            Some(std::path::Path::new("/data/models"))
        );
        assert_eq!(i.model_overrides.sources, Some(SourceSwitch::Denied));
        assert_eq!(i.model_overrides.sources_max.as_deref(), Some("100GiB"));
        let i = parse_invocation(["capyctl", "start", role, "--model-sources", "allowed"]).unwrap();
        assert_eq!(i.model_overrides.sources, Some(SourceSwitch::Allowed));
        assert!(parse_invocation(["capyctl", "start", role, "--model-sources", "maybe"]).is_err());
        assert!(
            parse_invocation(["capyctl", "start", role, "--model-sources-max", "lots"]).is_err()
        );
        // A relative directory is made absolute against the working directory.
        let i = parse_invocation(["capyctl", "start", role, "--models-root", "m"]).unwrap();
        assert!(i.model_overrides.models_root.unwrap().is_absolute());
    }
    assert_eq!(
        parse_invocation(["capyctl", "start", "standalone"])
            .unwrap()
            .model_overrides,
        Default::default()
    );
    assert!(parse_invocation(["capyctl", "start", "server", "--models-root", "/m"]).is_err());
}

// T03 (owner rule 2026-09-25: every setting three ways): the engine and
// download flags parse on `start standalone` and `start host`, `--kv-cache`
// on standalone only, `--state-dir` on every command, and `--hf-endpoint` on
// `deploy model`; a malformed value is refused by the parser.
#[test]
fn engine_flags_are_parsed_on_start_standalone_and_host() {
    use capyctl_config::effective::InstallationDrift;
    use capyctl_config::engine_settings::EngineOverrides;
    for role in ["standalone", "host"] {
        let i = parse_invocation([
            "capyctl",
            "start",
            role,
            "--vllm-bin",
            "/opt/vllm/bin/vllm",
            "--sglang-bin",
            "/opt/sglang/bin/python3",
            "--tensorfold-bin",
            "/opt/tensorfold/bin/tensorfold",
            "--engine-fingerprint",
            "vllm 0.29.0",
            "--engine-args",
            "--enforce-eager --max-num-seqs 4",
            "--deep-park",
            "off",
            "--trust-remote-code",
            "true",
            "--installation-drift",
            "refuse",
            "--runtime-dir",
            "/opt/capyctl/runtime",
            "--engine-ports",
            "9000-9099",
            "--cuda-home",
            "/usr/local/cuda-13.0",
            "--model-sources-path",
            "/data/downloads",
            "--hf-endpoint",
            "https://mirror.example",
        ])
        .unwrap();
        assert_eq!(
            i.engine_overrides,
            EngineOverrides {
                vllm: Some("/opt/vllm/bin/vllm".into()),
                sglang: Some("/opt/sglang/bin/python3".into()),
                tensorfold: Some("/opt/tensorfold/bin/tensorfold".into()),
                build_fingerprint: Some("vllm 0.29.0".into()),
                args: Some(vec![
                    "--enforce-eager".into(),
                    "--max-num-seqs".into(),
                    "4".into()
                ]),
                kv_cache: None,
                deep_park: Some(false),
                trust_remote_code: Some(true),
                installation_drift: Some(InstallationDrift::Refuse),
                runtime_dir: Some("/opt/capyctl/runtime".into()),
                engine_ports: Some((9000, 9099)),
                cuda_home: Some("/usr/local/cuda-13.0".into()),
            }
        );
        assert_eq!(
            i.model_overrides.sources_path.as_deref(),
            Some(std::path::Path::new("/data/downloads"))
        );
        assert_eq!(
            i.model_overrides.hf_endpoint.as_deref(),
            Some("https://mirror.example")
        );
        for (flag, bad) in [
            ("--deep-park", "disabled"),
            ("--trust-remote-code", "yes"),
            ("--installation-drift", "ignore"),
            ("--engine-ports", "80-90"),
            ("--hf-endpoint", "http://mirror.example"),
        ] {
            assert!(
                parse_invocation(["capyctl", "start", role, flag, bad]).is_err(),
                "{flag} {bad}"
            );
        }
    }
    let i = parse_invocation(["capyctl", "start", "standalone", "--kv-cache", "8GiB"]).unwrap();
    assert_eq!(i.engine_overrides.kv_cache.as_deref(), Some("8GiB"));
    assert!(parse_invocation(["capyctl", "start", "host", "--kv-cache", "8GiB"]).is_err());
    assert!(parse_invocation(["capyctl", "start", "standalone", "--kv-cache", "lots"]).is_err());
    assert!(parse_invocation(["capyctl", "start", "server", "--vllm-bin", "/v"]).is_err());
    // `--state-dir` on any command.
    let i = parse_invocation(["capyctl", "--state-dir", "/srv/capyctl", "list", "hosts"]).unwrap();
    assert_eq!(
        i.state_dir.as_deref(),
        Some(std::path::Path::new("/srv/capyctl"))
    );
    let i = parse_invocation(["capyctl", "start", "standalone", "--state-dir", "/srv/s"]).unwrap();
    assert_eq!(i.state_dir.as_deref(), Some(std::path::Path::new("/srv/s")));
    // `--hf-endpoint` on `deploy model`.
    assert!(matches!(
        parse([
            "capyctl",
            "deploy",
            "model",
            "--file",
            "d.yaml",
            "--hf-endpoint",
            "http://127.0.0.1:9"
        ]),
        Ok(Command::Deploy { hf_endpoint: Some(endpoint), .. }) if endpoint == "http://127.0.0.1:9"
    ));
}

// T03 (owner decision 2026-09-25): `--set path=value` (repeatable) on every
// role start, `validate config` and `config show`; `--management-listen` on
// `start standalone` (loopback only).
#[test]
fn generic_overrides_and_config_show_parse() {
    for role in ["server", "host", "standalone"] {
        let i = parse_invocation([
            "capyctl",
            "start",
            role,
            "--set",
            "shutdown.drain_timeout=45s",
            "--set",
            "a.b=c=d",
        ])
        .unwrap();
        assert_eq!(
            i.sets,
            vec!["shutdown.drain_timeout=45s", "a.b=c=d"],
            "{role}"
        );
        for bad in ["novalue", "=x", "a.b="] {
            assert!(
                parse_invocation(["capyctl", "start", role, "--set", bad]).is_err(),
                "{role} {bad}"
            );
        }
    }
    let i = parse_invocation([
        "capyctl", "validate", "config", "--file", "s.yaml", "--set", "name=x",
    ])
    .unwrap();
    assert!(matches!(&i.command, Command::Validate { sets, .. } if sets == &["name=x"]));
    assert_eq!(i.sets, vec!["name=x"]);
    let i = parse_invocation([
        "capyctl",
        "config",
        "show",
        "--role",
        "host",
        "--set",
        "load_report_interval=2s",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        &i.command,
        Command::ConfigShow { role: Some(Role::Host), sets } if sets == &["load_report_interval=2s"]
    ));
    assert!(parse_invocation(["capyctl", "config", "show", "--role", "engine"]).is_err());
    assert!(parse_invocation(["capyctl", "list", "hosts", "--set", "a=b"]).is_err());
    let i = parse_invocation([
        "capyctl",
        "start",
        "standalone",
        "--management-listen",
        "127.0.0.1:7543",
    ])
    .unwrap();
    assert_eq!(i.management_listen, Some("127.0.0.1:7543".parse().unwrap()));
    for bad in ["0.0.0.0:7543", "127.0.0.1:0", "localhost"] {
        assert!(
            parse_invocation(["capyctl", "start", "standalone", "--management-listen", bad])
                .is_err(),
            "{bad}"
        );
    }
    // Final review I8: the server takes `--management-listen` too, with the
    // same loopback rule.
    let i = parse_invocation([
        "capyctl",
        "start",
        "server",
        "--management-listen",
        "127.0.0.1:7543",
    ])
    .unwrap();
    assert_eq!(i.management_listen, Some("127.0.0.1:7543".parse().unwrap()));
    assert!(parse_invocation([
        "capyctl",
        "start",
        "server",
        "--management-listen",
        "0.0.0.0:7543"
    ])
    .is_err());
    assert!(parse_invocation([
        "capyctl",
        "start",
        "host",
        "--management-listen",
        "127.0.0.1:7543"
    ])
    .is_err());
    // `join host --set` applies the host document's overrides as `start host`.
    let i = parse_invocation([
        "capyctl",
        "join",
        "host",
        "--join-file",
        "/tmp/j",
        "--set",
        "load_report_interval=2s",
    ])
    .unwrap();
    assert_eq!(i.sets, ["load_report_interval=2s"]);
}

// T03: `--output` is described at the top level and on the commands that
// write a file with it, not on every subcommand's help; it still parses
// anywhere on the line.
#[test]
fn output_help_is_shown_only_where_it_means_something() {
    let texts = capyctl_cli::grammar::help_texts();
    let with_output: Vec<&str> = texts
        .iter()
        .filter(|(_, text)| text.contains("--output"))
        .map(|(path, _)| path.as_str())
        .collect();
    assert!(with_output.contains(&"capyctl"), "{with_output:?}");
    assert!(
        with_output.contains(&"capyctl init host"),
        "{with_output:?}"
    );
    assert!(with_output.contains(&"capyctl invite"), "{with_output:?}");
    assert!(
        with_output.iter().all(|path| *path == "capyctl"
            || path.starts_with("capyctl init")
            || *path == "capyctl invite"),
        "{with_output:?}"
    );
    let inv =
        parse_invocation(["capyctl", "status", "deployment", "d", "--output", "json"]).unwrap();
    assert_eq!(inv.output.as_deref(), Some("json"));
    let inv = parse_invocation(["capyctl", "--output", "json", "list", "hosts"]).unwrap();
    assert_eq!(inv.output.as_deref(), Some("json"));
    let inv = parse_invocation(["capyctl", "init", "host", "--output", "h.yaml"]).unwrap();
    assert_eq!(inv.output.as_deref(), Some("h.yaml"));
    let inv = parse_invocation(["capyctl", "invite", "host", "a", "--output", "i.json"]).unwrap();
    assert_eq!(inv.output.as_deref(), Some("i.json"));
}

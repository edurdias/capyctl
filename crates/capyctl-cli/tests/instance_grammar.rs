//! Owner decision Q7 (ADR 0013 as amended): action-first per-instance verbs.
use capyctl_cli::grammar::{parse, parse_invocation, Command, LifecycleAction};

// T10
#[test]
fn start_and_stop_instance_name_deployment_and_index() {
    for (action, want) in [
        ("start", LifecycleAction::Start),
        ("stop", LifecycleAction::Stop),
    ] {
        let command = parse(["capyctl", action, "instance", "qwen3-4b/1"]).unwrap();
        assert_eq!(
            command,
            Command::InstanceLifecycle {
                action: want,
                deployment: "qwen3-4b".into(),
                instance: 1,
            }
        );
        assert_eq!(command.label(), format!("{action} instance qwen3-4b/1"));
    }
    // The deployment part may itself contain a slash; the index is the last part.
    assert!(matches!(
        parse(["capyctl", "stop", "instance", "team/model/0"]),
        Ok(Command::InstanceLifecycle { deployment, instance: 0, .. }) if deployment == "team/model"
    ));
    // Deployment verbs are unchanged.
    assert!(matches!(
        parse(["capyctl", "stop", "deployment", "dep_x"]),
        Ok(Command::Lifecycle { action: LifecycleAction::Stop, deployment }) if deployment == "dep_x"
    ));
    // A per-instance command is a recoverable mutation like its deployment form.
    let id = ulid::Ulid::new().to_string();
    let recoverable = parse_invocation([
        "capyctl",
        "stop",
        "instance",
        "dep_x/0",
        "--request-id",
        id.as_str(),
    ])
    .unwrap();
    assert_eq!(recoverable.request_id.as_deref(), Some(id.as_str()));
}

// T03
#[test]
fn malformed_instance_references_are_refused() {
    for reference in [
        "dep_x",
        "dep_x/",
        "/1",
        "dep_x/01",
        "dep_x/-1",
        "dep_x/64",
        "dep_x/one",
    ] {
        assert!(
            parse(["capyctl", "stop", "instance", reference]).is_err(),
            "{reference}"
        );
    }
    assert!(parse(["capyctl", "park", "instance", "dep_x/0"]).is_err());
}

/// W14 per-instance marking (ADR 0013 §6): an instance whose host resolves a
/// different development-control mark than the deployment gets its own notice;
/// an instance with the deployment's mark adds nothing.
// T21
#[test]
fn an_instance_with_its_own_exposure_gets_its_own_notice() {
    let exposed = serde_json::json!({"state":"exposed","deep_park":"enabled","surface":["/sleep"],"mitigations":["per_launch_engine_key"],"production_safe":false});
    let clear = serde_json::json!({"state":"not_exposed"});
    let view = serde_json::json!({"deployments":[{"name":"d","development_controls":clear,
        "instances":[{"index":0,"development_controls":clear},{"index":1,"development_controls":exposed}]}]});
    let notices = capyctl_cli::output::development_controls_notices(&view);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("deployment d instance 1"),
        "{notices:?}"
    );
}

//! SPEC §6.5, §10: a host whose activation policy is explicit
//! (`host.lifecycle.activation: explicit`, here through `--set`) starts, parks,
//! wakes and stops its deployments only on an operator's action. An inference
//! request for one that is not serving is refused at once with HTTP 409
//! `deployment_inactive` and changes nothing; the operator's own commands work
//! as always, and an operator's stop keeps its own answer.
//!
//! Fake engine only: these are capyctl's own decisions, not qualification of a
//! native engine recipe (SPEC §18).

mod support;

use capyctl_cli::roles::{App, SettingOverrides};
use capyctl_config::effective::ModelSource;
use capyctl_config::ConfigKind;
use capyctl_controller::LifecyclePort as _;
use capyctl_domain::{LifecycleAction, LifecycleState};
use support::{boot_deep_parking_with_overrides, safe_state_dir};

async fn settle(app: &App, id: &str, action: LifecycleAction, want: LifecycleState) {
    let handle = app
        .controller
        .request_transition(id, action)
        .await
        .unwrap_or_else(|error| panic!("{action:?} was refused: {error:?}"));
    let state = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        app.controller.wait_terminal(&handle),
    )
    .await
    .unwrap_or_else(|_| panic!("{action:?} did not settle within 30 s"))
    .unwrap_or_else(|error| panic!("{action:?} did not settle: {error:?}"));
    assert_eq!(state, want, "after {action:?}");
}

/// One chat request through the inference listener: its status and body.
async fn chat(app: &App, model: &str) -> (u16, serde_json::Value) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reqwest::Client::new()
            .post(format!("http://{addr}/v1/chat/completions"))
            .header("Authorization", format!("Bearer {}", app.api_key()))
            .json(&serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send(),
    )
    .await
    .expect("refused promptly, never queued")
    .unwrap();
    let status = response.status().as_u16();
    let body = response.json().await.unwrap();
    server.abort();
    (status, body)
}

// T10 T16 (SPEC §6.5, §10): the request never starts or wakes the
// deployment; the operator's start, park, wake and stop all work.
#[tokio::test]
async fn an_explicit_host_moves_its_deployments_only_on_an_operators_action() {
    // Two processes this test owns stand for the engine group, so the
    // coordinator's park and wake checks read real processes.
    struct Group(Vec<std::process::Child>);
    impl Drop for Group {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let group = Group(
        (0..2)
            .map(|_| {
                std::process::Command::new("sleep")
                    .arg("600")
                    .spawn()
                    .expect("a stand-in engine process")
            })
            .collect(),
    );
    let members = vec![
        capyctl_testkit::live_identity("api", group.0[0].id()),
        capyctl_testkit::live_identity("worker-0", group.0[1].id()),
    ];
    let dir = safe_state_dir();
    let overrides = SettingOverrides::parse(
        ConfigKind::Standalone,
        &["host.lifecycle.activation=explicit".to_owned()],
        &[],
    )
    .unwrap();
    let app = boot_deep_parking_with_overrides(dir.path(), members, &overrides).await;
    let id = app
        .deploy(
            "explicit-m",
            ModelSource::Local {
                path: "/models/explicit-m".into(),
            },
        )
        .expect("deployed");
    let state = |app: &App| {
        app.store
            .get_deployment(&id)
            .unwrap()
            .unwrap()
            .observed_state
    };
    let inactive = |(status, body): (u16, serde_json::Value)| {
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["code"], "deployment_inactive", "{body}");
        let message = body["message"].as_str().unwrap();
        assert!(message.contains("activation is explicit"), "{message}");
        assert!(
            message.contains(&format!("capyctl start deployment {id}")),
            "{message}"
        );
    };

    // Deployed and never started: the request starts nothing.
    inactive(chat(&app, "explicit-m").await);
    assert_eq!(state(&app), LifecycleState::Stopped);

    settle(&app, &id, LifecycleAction::Start, LifecycleState::Ready).await;
    settle(&app, &id, LifecycleAction::Park, LifecycleState::Parked).await;
    // Parked by the operator: the request wakes nothing.
    inactive(chat(&app, "explicit-m").await);
    assert_eq!(state(&app), LifecycleState::Parked);
    // The operator's start wakes it.
    settle(&app, &id, LifecycleAction::Start, LifecycleState::Ready).await;

    // An operator's stop keeps its own, more specific answer. The stand-in
    // engine processes end first, so the stop's cleanup proves them gone.
    drop(group);
    settle(&app, &id, LifecycleAction::Stop, LifecycleState::Stopped).await;
    let (status, body) = chat(&app, "explicit-m").await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "deployment_stopped", "{body}");
    assert_eq!(state(&app), LifecycleState::Stopped);
}

// T19 (SPEC §6.5, §10): with no waiting allowed
// (`max_pending_per_deployment: 0`), a request for an explicit deployment
// that is not serving is still 409 `deployment_inactive`, never a retryable
// 429 `queue_full`: no activation would start, so a retry would not help.
// An operator's stop keeps `deployment_stopped`.
#[tokio::test]
async fn with_no_waiting_an_inactive_explicit_deployment_is_refused_409() {
    let dir = safe_state_dir();
    let overrides = SettingOverrides::parse(
        ConfigKind::Standalone,
        &["host.lifecycle.activation=explicit".to_owned()],
        &[],
    )
    .unwrap();
    let app = boot_deep_parking_with_overrides(dir.path(), Vec::new(), &overrides).await;
    // The bound a host's `resource_policy.queue.max_pending_per_deployment: 0`
    // gives the router (`standalone_queue_of_zero_is_set_three_ways` covers
    // the setting itself).
    let mut limits = app.deps().inflight.waiting.limits();
    limits.max_pending_per_deployment = 0;
    app.deps().inflight.waiting.set_limits(limits);
    let id = app
        .deploy(
            "explicit-q0",
            ModelSource::Local {
                path: "/models/explicit-q0".into(),
            },
        )
        .expect("deployed");
    let (status, body) = chat(&app, "explicit-q0").await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "deployment_inactive", "{body}");
    assert_eq!(
        app.store
            .get_deployment(&id)
            .unwrap()
            .unwrap()
            .observed_state,
        LifecycleState::Stopped
    );

    // An operator's stop of the never-started deployment.
    settle(&app, &id, LifecycleAction::Stop, LifecycleState::Stopped).await;
    let (status, body) = chat(&app, "explicit-q0").await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "deployment_stopped", "{body}");
}

// T03 (SPEC §15.3): a value the embedded host would not honour is refused
// before anything starts, naming the setting.
#[tokio::test]
async fn an_unknown_activation_policy_refuses_the_start() {
    let dir = safe_state_dir();
    let overrides = SettingOverrides::parse(
        ConfigKind::Standalone,
        &["host.lifecycle.activation=manual".to_owned()],
        &[],
    )
    .unwrap();
    let refused = support::boot_with_overrides(dir.path(), None, &overrides)
        .await
        .err()
        .expect("an unknown activation policy is refused");
    assert!(
        refused.to_string().contains("host.lifecycle.activation"),
        "{refused}"
    );
}

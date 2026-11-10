//! T19 T47 (SPEC §10, ADR 0014 §2, ADR 0036 §3): the deployment's declared
//! concurrency bounds both router admission and durable request leases. These
//! exercise the standalone role's real configuration, coordinator and router;
//! CPU and Fake-engine tests are not qualification of a native recipe.

mod support;

use std::sync::Arc;
use std::time::Duration;

use capyctl_cli::roles::App;
use capyctl_config::effective::{DeploymentApi, ModelSource};
use capyctl_config::engine_policy::Engine;
use capyctl_controller::{LeaseEnd, LifecyclePort as _};
use capyctl_domain::LifecycleAction;
use capyctl_router::balance::Plan;
use capyctl_router::endpoint::Endpoint;
use capyctl_router::request_id::RequestIds;
use capyctl_router::timing::RequestTiming;
use serde_json::json;

async fn configured(app: &App, declared: Option<u32>, api: DeploymentApi) -> String {
    let mut document = capyctl_cli::standalone_config::deployment_document(
        "bounded",
        "bounded",
        &ModelSource::Local {
            path: "/models/bounded".into(),
        },
        Engine::Vllm,
        &capyctl_cli::standalone_config::TemplateMemory::Unified {
            capacity_bytes: support::TEST_CAPACITY_BYTES,
        },
        capyctl_cli::standalone_config::DEFAULT_REQUEST_DEADLINE,
        false,
        "local",
    )
    .unwrap();
    if let Some(declared) = declared {
        document["engine_config"]["max_concurrent_requests"] = json!(declared);
    }
    if api == DeploymentApi::Embeddings {
        document["api"] = json!("embeddings");
    }
    let id = app
        .controller
        .create_configuration(
            "test",
            "bounded",
            &json!({"config": document}).to_string(),
            &app.host_document(),
        )
        .unwrap()
        .deployment_id;
    let operation = app
        .controller
        .request_transition(&id, LifecycleAction::Start)
        .await
        .unwrap();
    app.controller.wait_terminal(&operation).await.unwrap();
    id
}

async fn holds_exactly(declared: Option<u32>, api: DeploymentApi, expected: usize) {
    let state = support::safe_state_dir();
    let app = support::boot(state.path()).await;
    let id = configured(&app, declared, api).await;
    let mut waiting = app.deps().inflight.waiting.limits();
    waiting.max_pending_per_deployment = 0;
    let capacity = app.store.capacity(Some(&id)).unwrap();
    let report = capyctl_router::capacity::capacity_report(
        &capacity,
        &app.deps().inflight,
        32,
        None,
        capyctl_protocol::now_unix_ms(),
    );
    assert_eq!(report["deployments"][0]["router"]["in_flight_limit"], expected);
    app.deps().inflight.waiting.set_limits(waiting);
    let endpoint = Endpoint::serving(api);
    let mut held = Vec::new();
    for slot in 0..expected {
        let ids = Arc::new(RequestIds::untracked());
        let received = tokio::time::Instant::now();
        let admitted = capyctl_router::chat::admit_timed(
            app.deps(),
            endpoint,
            "bounded",
            1,
            received,
            &mut RequestTiming::untracked(),
            &ids,
        )
        .await
        .unwrap_or_else(|refusal| panic!("slot {slot} below {expected} refused: {refusal:?}"));
        let (deployment, guard, view) = admitted;
        assert_eq!(deployment, id);
        let mut plan = Plan::new(
            app.deps(),
            &id,
            view,
            ids,
            received + Duration::from_secs(5),
        )
        .unwrap();
        let attempt = plan
            .next()
            .await
            .unwrap_or_else(|refusal| panic!("lease {slot} below {expected} refused: {refusal:?}"));
        held.push((guard, attempt));
    }
    assert_eq!(app.deps().inflight.current(&id), expected);
    let refused = capyctl_router::chat::admit_timed(
        app.deps(),
        endpoint,
        "bounded",
        1,
        tokio::time::Instant::now(),
        &mut RequestTiming::untracked(),
        &RequestIds::untracked(),
    )
    .await
    .err()
    .expect("the next request exceeds the deployment's configured bound");
    assert_eq!(refused.0, axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.1 .0["code"], "queue_full");
    for (guard, attempt) in held {
        assert!(attempt.settle(LeaseEnd::NotAccepted).await);
        drop(guard);
    }
    let _ = app.shutdown().await;
}

// T19 T47: #240's 48 slots must be usable, not capped at the default 32.
#[tokio::test]
async fn chat_and_embeddings_use_all_declared_slots_above_32() {
    for api in [DeploymentApi::Chat, DeploymentApi::Embeddings] {
        holds_exactly(Some(48), api, 48).await;
    }
}

// T19 T47: the setting is a bound, not merely permission to exceed 32.
#[tokio::test]
async fn chat_and_embeddings_obey_a_declared_bound_below_32() {
    for api in [DeploymentApi::Chat, DeploymentApi::Embeddings] {
        holds_exactly(Some(2), api, 2).await;
    }
}

// T19 T47: undeclared engine parallelism (including engine-specific defaults)
// does not change the router's default per-deployment bound.
#[tokio::test]
async fn chat_and_embeddings_keep_32_slots_when_concurrency_is_undeclared() {
    for api in [DeploymentApi::Chat, DeploymentApi::Embeddings] {
        holds_exactly(None, api, 32).await;
    }
}

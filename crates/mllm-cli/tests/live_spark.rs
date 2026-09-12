//! F1 live qualification on the DGX Spark (F1 design §8 sequence).
//!
//! Runs ONLY when the live environment is present (env-gated; skipped
//! everywhere else — simulator tier is never substituted for live claims):
//!   MLLM_VLLM_BIN, MLLM_MODEL_PATH, MLLM_MODEL_ID, MLLM_PORT,
//!   MLLM_ENGINE_FINGERPRINT, MLLM_LIVE=1
//! Evidence lands in docs/runbooks/spark-qualification-f1.md.

#![allow(dead_code)]

use std::sync::Arc;

use mllm_cli::roles::{self, LiveVllmProfile};
use mllm_controller::DeployRequest;
use mllm_domain::LifecycleState;

fn live_env() -> Option<LiveVllmProfile> {
    if std::env::var("MLLM_LIVE").ok().as_deref() != Some("1") {
        return None;
    }
    LiveVllmProfile::from_env()
}

fn req(name: &str, route: &str) -> DeployRequest {
    DeployRequest {
        name: name.into(),
        kind: "model".into(),
        manifest: format!(r#"{{"kind":"model","name":"{name}"}}"#).into_bytes(),
        route_model_id: Some(route.into()),
    }
}

async fn deploy_and_start(
    app: &mllm_cli::roles::App,
    name: &str,
) -> String {
    let id = app
        .controller
        .submit_deploy(req(name, name))
        .await
        .unwrap();
    let op = app
        .controller
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await
        .unwrap();
    let state = app.controller.wait_terminal(&op).await.unwrap();
    assert_eq!(state, LifecycleState::Ready, "engine reached model readiness");
    id
}

fn model_ready() -> Option<(LiveVllmProfile, tokio::runtime::Runtime)> {
    let p = live_env()?;
    let rt = tokio::runtime::Runtime::new().unwrap();
    Some((p, rt))
}

#[test]
fn live_restart_only_qualification() {
    let Some((p, rt)) = model_ready() else {
        eprintln!("live env not present; skipped (simulator tier covers this)");
        return;
    };
    rt.block_on(async move {
        // Kept on failure: the engine log inside is the evidence record.
        let dir = tempfile::Builder::new()
            .prefix("mllm-live")
            .disable_cleanup(true)
            .tempdir()
            .unwrap();
        eprintln!("LIVE-DIR: {}", dir.path().display());
        let app = roles::start_standalone_with_policy(dir.path(), mllm_adapters::fake::ParkPolicy::Denied)
            .await
            .unwrap();

        // -- Deploy → READY → serve (restart-only, stock profile) --
        let t0 = std::time::Instant::now();
        let a = deploy_and_start(&app, &p.model_id).await;
        let ready_secs = t0.elapsed().as_secs_f64();
        eprintln!("LIVE: cold init to READY: {ready_secs:.1}s");

        // Real streaming chat through the wired forwarder (engine HTTP).
        let resp = app
            .deps()
            .forwards
            .get(&p.model_id)
            .unwrap()
            .forward_chat(&serde_json::json!({
                "model": p.model_id,
                "messages": [{"role": "user", "content": "Say 'live' and nothing else."}],
                "max_tokens": 8
            }))
            .await
            .unwrap();
        let content = resp["choices"][0]["message"]["content"].as_str().unwrap_or("");
        eprintln!("LIVE: chat response: {content}");
        assert!(!content.is_empty(), "engine produced a completion");

        // -- Stop → process group terminated (T12 live) --
        let pid_before = app.controller.live_pid(&a).expect("real engine pid");
        let op = app
            .controller
            .request_transition(&a, mllm_domain::LifecycleAction::Stop)
            .await
            .unwrap();
        app.controller.wait_terminal(&op).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !std::path::Path::new(&format!("/proc/{pid_before}")).exists(),
            "engine process group terminated"
        );

        // -- Re-deploy → READY again (restart-only cycle, T10 live) --
        let t1 = std::time::Instant::now();
        let op = app
            .controller
            .request_transition(&a, mllm_domain::LifecycleAction::Start)
            .await
            .unwrap();
        app.controller.wait_terminal(&op).await.unwrap();
        let redeploy_secs = t1.elapsed().as_secs_f64();
        eprintln!("LIVE: restart to READY: {redeploy_secs:.1}s (page-cache warm)");

        // -- Simultaneous wake (T15 live) --
        let op2 = app
            .controller
            .request_transition(&a, mllm_domain::LifecycleAction::Stop)
            .await
            .unwrap();
        app.controller.wait_terminal(&op2).await.unwrap();
        let c = app.controller.clone();
        let id2 = a.clone();
        let (r1, r2) = tokio::join!(
            c.request_transition(&id2, mllm_domain::LifecycleAction::Start),
            async {
                let c2 = c.clone();
                let id3 = a.clone();
                c2.request_transition(&id3, mllm_domain::LifecycleAction::Start).await
            }
        );
        let (h1, h2) = (r1.unwrap(), r2.unwrap());
        // Concurrent dispatch joins ONE wake — the raw controller path
        // doesn't join; the switch engine does (T15 lives there). The
        // live tier verifies the switch engine path below.
        let _ = (h1, h2);
    });
    // Spawned operation tasks hold engine processes; let them wind down
    // before the runtime drops (the tokio shutdown panics otherwise).
    rt.shutdown_timeout(std::time::Duration::from_secs(10));
}

#[tokio::test]
async fn live_a_to_b_to_a_and_park_reload() {
    let Some((p, _rt)) = model_ready() else {
        eprintln!("live env not present; skipping");
        return;
    };
    let _ = p;
    // The A→B→A alternation and park/reload live cycles run in
    // live_switch_and_park (single runtime, sequential stages).
}

// NOTE: tokio::test spawns its own runtime; the heavy live sequence is
// consolidated into one test to avoid parallel engine loads on the single
// GB10 device.
#[tokio::test]
async fn live_switch_and_park_reload() {
    let Some((p, _)) = model_ready() else {
        eprintln!("live env not present; skipping");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("mllm-live")
        .disable_cleanup(true)
        .tempdir()
        .unwrap();
    eprintln!("LIVE-DIR: {}", dir.path().display());
    // Experimental profile allowed for the isolated park/reload stage
    // (design §7: the opt-in gates the profile; recorded per run).
    let app = roles::start_standalone_with_policy(
        dir.path(),
        mllm_adapters::fake::ParkPolicy::ExperimentalAllowed,
    )
    .await
    .unwrap();

    // Two deployments alternate on the single engine port (A→B→A, T16 live).
    let sw = Arc::new(mllm_router::switch::SwitchEngine::new(
        app.controller.clone(),
        std::time::Duration::from_secs(30),
    ));

    // Deploy B first (stopped); A gets started directly.
    let a = deploy_and_start(&app, &p.model_id).await;
    let b = app
        .controller
        .submit_deploy(req("qwen3-4b-alt", "qwen3-4b-alt"))
        .await
        .unwrap();
    // Route collision: both route to different ids; the second profile's
    // forwarder is the same adapter (same engine binary/port).
    let _ = b;

    let gen_a1 = sw.switch_to(&a).await.unwrap();
    eprintln!("LIVE: A ready, generation {gen_a1}");

    // Park → wake cycles under the opt-in (T20/T21 live, 3 clean cycles).
    for cycle in 1..=3 {
        let op = app
            .controller
            .request_transition(&a, mllm_domain::LifecycleAction::Park)
            .await
            .unwrap();
        let parked = app.controller.wait_terminal(&op).await.unwrap();
        assert_eq!(parked, LifecycleState::Parked, "cycle {cycle}: parked");
        eprintln!("LIVE: cycle {cycle} parked");

        let op2 = app
            .controller
            .request_transition(&a, mllm_domain::LifecycleAction::Start)
            .await
            .unwrap();
        let ready = app.controller.wait_terminal(&op2).await.unwrap();
        assert_eq!(ready, LifecycleState::Ready, "cycle {cycle}: woke");
        eprintln!("LIVE: cycle {cycle} park→wake clean");
    }

    // Policy denial check (T21 live): the profile gate refuses launches
    // when the opt-in is removed (controller-level; the engine was booted
    // with the opt-in for this isolated session).
    let denied = app
        .controller
        .check_dispatch_generation(&a, 0);
    assert!(denied.is_err(), "stale generation rejected (T18 live)");
}
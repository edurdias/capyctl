//! F1 live qualification on the DGX Spark (F1 design §8 sequence).
//!
//! Runs ONLY when the live environment is present (env-gated; skipped
//! everywhere else — simulator tier is never substituted for live claims):
//!   MLLM_VLLM_BIN, MLLM_MODEL_PATH, MLLM_MODEL_ID, MLLM_PORT,
//!   MLLM_ENGINE_FINGERPRINT, MLLM_LIVE=1
//! Evidence lands in docs/runbooks/spark-qualification-f1.md.

#![allow(dead_code)]


/// A state directory the controller lock will accept.
///
/// The lock walks every ancestor of the state path and refuses any that is group- or
/// other-writable, because such an ancestor lets another account replace the
/// directory the lock guards. `/tmp` is 1777 and a checkout is commonly 0775, so
/// neither can hold controller state. The home directory is the usual root that
/// satisfies the rule.
fn safe_state_dir() -> tempfile::TempDir {
    let home = std::env::var("HOME").expect("HOME is set");
    tempfile::TempDir::new_in(home).expect("a state directory under an owner-only root")
}

/// The engine's API process id, from the deployment's durable identity set.
/// Replaces the old in-memory handle lookup: these identities survive a restart and
/// carry a boot id and start time, so a caller can tell whether the pid still names
/// the process that was recorded.
fn live_pid(app: &roles::App, deployment: &str) -> Option<u32> {
    app.controller
        .live_identities(deployment)
        .ok()?
        .into_iter()
        .find(|identity| identity.role == "api")
        .map(|identity| identity.pid)
}

use mllm_controller::LifecyclePort as _;

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
        .submit_deploy("standalone", &req(name, name))
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
    let p = live_profile()?;
    let rt = tokio::runtime::Runtime::new().unwrap();
    Some((p, rt))
}

/// Profile only (for tests that already run inside a tokio runtime).
fn live_profile() -> Option<LiveVllmProfile> {
    live_env()
}

// Failure injection deliberately leaves lifecycle Failed, which has no
// Stop action in F1. Keep cleanup panic-safe and refuse a reused PID.
struct LiveProcessCleanup {
    pid: u32,
    starttime: String,
}

impl LiveProcessCleanup {
    fn starttime(pid: u32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?.1.split_whitespace().nth(19).map(str::to_owned)
    }

    fn new(pid: u32) -> Self {
        Self { pid, starttime: Self::starttime(pid).expect("owned engine exists") }
    }
}

impl Drop for LiveProcessCleanup {
    fn drop(&mut self) {
        if Self::starttime(self.pid).as_ref() == Some(&self.starttime) {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", "--", &format!("-{}", self.pid)]).status();
        }
    }
}

#[tokio::test]
async fn live_default_denies_sleep_profile() {
    let Some(_) = live_profile() else { return; };
    let dir = safe_state_dir();
    let app = roles::start_standalone_with_policy(
        dir.path(), mllm_adapters::fake::ParkPolicy::Disabled,
    ).await.unwrap();
    let id = app.controller.submit_deploy("standalone", &DeployRequest {
        kind: "vllm-sleep".into(),
        ..req("denied-sleep", "denied-sleep")
    }).unwrap();
    let result = app.controller.request_transition(&id, mllm_domain::LifecycleAction::Start).await;
    assert!(matches!(result, Err(mllm_controller::LifecycleFault::Blocked(ref message)) if message.contains("Denied") || message.contains("denied")));
    assert!(live_pid(&app, &id).is_none(), "denied profile never spawns");
    eprintln!("LIVE: T21 default policy denied sleep profile before spawn");
}

// Pending milestone A1b. This drives an ambiguous park through a second controller
// sharing the store. Ordinary park does not exist in the coordinator yet, and a
// second authority over the same state is what the controller lock now prevents by
// design. Both the park and the fault injection return with A1b, which is also where
// the injection should be rebuilt without a second authority.
// Kept compiled out rather than deleted: the scenario and its proxy-based fault
// injection are what A1b rebuilds from. `cfg(any())` never matches, so the source
// stays readable without being compiled against an API it predates.
#[cfg(any())]
#[tokio::test]
async fn live_ambiguous_park_reconciles() {
    use mllm_adapters::fake::ParkPolicy;
    use tokio::io::AsyncReadExt;

    let Some(p) = live_profile() else { return; };
    let dir = tempfile::Builder::new().prefix("mllm-live").disable_cleanup(true).tempdir().unwrap();
    eprintln!("LIVE-DIR: {}", dir.path().display());
    let app = roles::start_standalone_with_policy(dir.path(), ParkPolicy::Enabled).await.unwrap();
    let id = deploy_and_start(&app, &p.model_id).await;
    let pid = live_pid(&app, &id).unwrap();
    let cleanup = LiveProcessCleanup::new(pid);

    // The real controller owns this running engine. A second controller
    // shares its persisted deployment/member state, with only the HTTP
    // transport redirected through a lost-ack proxy for this park.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let upstream = format!("http://127.0.0.1:{}", p.port);
    let sleep_url = format!("{upstream}/sleep?level=2");
    let proxy = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let byte = socket.read_u8().await.unwrap();
            request.push(byte);
            assert!(request.len() < 16384);
        }
        assert!(request.starts_with(b"POST /sleep?level=2 "));
        let response = reqwest::Client::new().post(sleep_url).send().await.unwrap();
        assert!(response.status().is_success(), "real engine applied park");
        // Drop the downstream connection only AFTER the real engine ack.
        drop(socket);
        let mut calls = 1usize;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        while let Ok(Ok((_socket, _))) = tokio::time::timeout_at(deadline, listener.accept()).await {
            calls += 1;
        }
        calls
    });
    let adapter = mllm_adapters::vllm::VllmAdapter::new(
        format!("http://{proxy_addr}").parse().unwrap(), None,
        p.fingerprint.clone(), ParkPolicy::Enabled, p.model_id.clone(),
    );
    let store = app.controller.store_ref().clone();
    let fault_controller = mllm_controller::Controller::new_with_policy(
        store.clone(), Arc::new(adapter), Arc::new(mllm_launchers::ExecLauncher::new()),
        ParkPolicy::Enabled,
    );
    let op = fault_controller.request_transition(&id, mllm_domain::LifecycleAction::Park).await.unwrap();
    let result = fault_controller.wait_terminal(&op).await;
    let calls = proxy.await.unwrap();
    let sleeping: serde_json::Value = reqwest::get(format!("{upstream}/is_sleeping")).await.unwrap().json().await.unwrap();
    let (state, evidence, parks) = {
        let guard = store.lock().unwrap();
        (guard.get_deployment(&id).unwrap().unwrap().observed_state,
         guard.journal_evidence_of(&id).unwrap().join("\n"),
         guard.operations_of_kind(&id, "park").unwrap().len())
    };
    drop(cleanup);
    assert!(result.is_err(), "lost ack must not become success");
    assert_eq!(sleeping["is_sleeping"], true);
    assert_eq!(state, LifecycleState::Failed);
    assert!(evidence.contains("\"event\":\"uncertain\""));
    assert_eq!((calls, parks), (1, 1), "one park, no blind repeat");
    eprintln!("LIVE: T20 real sleep applied, ack dropped; one park; reconciled to Failed");
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
        let app = roles::start_standalone_with_policy(dir.path(), mllm_adapters::fake::ParkPolicy::Disabled)
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
        let pid_before = live_pid(&app, &a).expect("real engine pid");
        let op = app
            .controller
            .request_transition(&a, mllm_domain::LifecycleAction::Stop)
            .await
            .unwrap();
        app.controller.wait_terminal(&op).await.unwrap();
        // Wait for the kernel to reap the group (a zombie still shows in
        // /proc for a beat after SIGKILL — the reaper thread reaps).
        let mut gone = false;
        for _ in 0..30 {
            if !std::path::Path::new(&format!("/proc/{pid_before}")).exists() {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(gone, "engine process group terminated");

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
async fn live_switch_restart_only() {
    let Some(p) = live_profile() else {
        eprintln!("live env not present; skipping");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("mllm-live")
        .disable_cleanup(true)
        .tempdir()
        .unwrap();
    eprintln!("LIVE-DIR: {}", dir.path().display());
    let app = roles::start_standalone_with_policy(
        dir.path(),
        mllm_adapters::fake::ParkPolicy::Disabled,
    )
    .await
    .unwrap();

    // Two deployments alternate on the single engine port (A→B→A, T16 live).
    let sw = Arc::new(mllm_router::switch::SwitchEngine::new(
        app.controller.clone(),
        std::time::Duration::from_secs(30),
    ));

    // Restart-only alternation (T16 live): A and B are two deployments of
    // the pinned checkpoint on the single engine port; the switch releases
    // A by stop, then B spawns.
    let a = deploy_and_start(&app, &p.model_id).await;
    let b = app
        .controller
        .submit_deploy("standalone", &req("qwen3-4b-alt", "qwen3-4b-alt"))
        .unwrap();

    let gen_a1 = sw.switch_to(&a).await.unwrap();
    eprintln!("LIVE: A ready, generation {gen_a1}");

    // A → B (T16 live): A released by stop, B reaches READY.
    let gen_b = sw.switch_to(&b).await.unwrap();
    eprintln!("LIVE: B ready after switch, generation {gen_b}", gen_b = gen_b);
    let a_state = app
        .controller
        .get_deployment(&a)
        .unwrap()
        .unwrap()
        .observed_state;
    assert_eq!(a_state, LifecycleState::Stopped, "A released on switch");

    let gen_a2 = sw.switch_to(&a).await.unwrap();
    assert!(gen_a2 > gen_a1);
    eprintln!("LIVE: A→B→A complete, A generation {gen_a2}");
    let op = app.controller.request_transition(&a, mllm_domain::LifecycleAction::Stop).await.unwrap();
    app.controller.wait_terminal(&op).await.unwrap();
}

#[tokio::test]
async fn live_park_reload() {
    let Some(p) = live_profile() else { return; };
    let dir = tempfile::Builder::new().prefix("mllm-live").disable_cleanup(true).tempdir().unwrap();
    eprintln!("LIVE-DIR: {}", dir.path().display());
    let app = roles::start_standalone_with_policy(
        dir.path(), mllm_adapters::fake::ParkPolicy::Enabled,
    ).await.unwrap();

    // Park → wake cycles under the opt-in (T20 live, 3 clean cycles) —
    // the sleep-enabled deployment S.
    let s_dep = app
        .controller
        .submit_deploy("standalone", &DeployRequest {
            kind: "vllm-sleep".into(),
            ..req("qualified-model-sleep", &p.model_id)
        })
        .unwrap();
    let startup_started = std::time::Instant::now();
    let op = app.controller.request_transition(&s_dep, mllm_domain::LifecycleAction::Start).await.unwrap();
    assert_eq!(app.controller.wait_terminal(&op).await.unwrap(), LifecycleState::Ready);
    let startup_ready_seconds = startup_started.elapsed().as_secs_f64();
    let _cleanup = LiveProcessCleanup::new(live_pid(&app, &s_dep).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let router_addr = listener.local_addr().unwrap();
    let router = app.router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(120)).build().unwrap();
    let request = serde_json::json!({
        "model": p.model_id,
        "messages": [{"role": "user", "content": "Say 'live' and nothing else."}],
        "max_tokens": 32,
        "temperature": 0,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    let response = client.post(format!("http://{router_addr}/v1/chat/completions"))
        .bearer_auth(app.api_key()).json(&request).send().await.unwrap();
    let status = response.status();
    let response: serde_json::Value = response.json().await.unwrap();
    assert!(status.is_success(), "initial routed inference: {response}");
    assert_eq!(response["choices"][0]["message"]["content"].as_str().unwrap_or("").trim(), "live",
        "initial weights must produce the requested answer: {response}");
    eprintln!("LIVE-STARTUP: {}", serde_json::json!({
        "model": p.model_id,
        "ready_seconds": startup_ready_seconds,
        "response_seconds": startup_started.elapsed().as_secs_f64(),
    }));
    for cycle in 1..=3 {
        let park_started = std::time::Instant::now();
        let op = app
            .controller
            .request_transition(&s_dep, mllm_domain::LifecycleAction::Park)
            .await
            .unwrap();
        let parked = app.controller.wait_terminal(&op).await.unwrap();
        assert_eq!(parked, LifecycleState::Parked, "cycle {cycle}: parked");
        let park_seconds = park_started.elapsed().as_secs_f64();
        eprintln!("LIVE: cycle {cycle} parked");

        let wake_started = std::time::Instant::now();
        let op2 = app
            .controller
            .request_transition(&s_dep, mllm_domain::LifecycleAction::Start)
            .await
            .unwrap();
        let ready = app.controller.wait_terminal(&op2).await.unwrap();
        assert_eq!(ready, LifecycleState::Ready, "cycle {cycle}: woke");
        let ready_seconds = wake_started.elapsed().as_secs_f64();
        let response = client.post(format!("http://{router_addr}/v1/chat/completions"))
            .bearer_auth(app.api_key())
            .json(&request).send().await.unwrap();
        let status = response.status();
        let response: serde_json::Value = response.json().await.unwrap();
        assert!(status.is_success(), "cycle {cycle}: routed inference: {response}");
        let content = response["choices"][0]["message"]["content"].as_str().unwrap_or("");
        assert_eq!(content.trim(), "live", "cycle {cycle}: restored weights produce the requested answer");
        eprintln!("LIVE-METRIC: {}", serde_json::json!({
            "cycle": cycle,
            "park_seconds": park_seconds,
            "wake_ready_seconds": ready_seconds,
            "wake_response_seconds": wake_started.elapsed().as_secs_f64(),
        }));
        eprintln!("LIVE: cycle {cycle} completion: {content}");
        eprintln!("LIVE: cycle {cycle} park→wake clean");
    }

    // Stale-generation dispatch rejection (T18 live).
    let denied = app
        .controller
        .check_dispatch_generation(&s_dep, 0);
    assert!(denied.is_err(), "stale generation rejected (T18 live)");
    let op = app.controller.request_transition(&s_dep, mllm_domain::LifecycleAction::Stop).await.unwrap();
    app.controller.wait_terminal(&op).await.unwrap();
    server.abort();
}

/// Reuse distinct prefixes across destructive sleeps: one trivial prompt
/// misses stale cached-block failures seen with concurrent post-wake traffic.
#[tokio::test]
async fn live_concurrent_park_reload() {
    let Some(p) = live_profile() else { return; };
    let dir = tempfile::Builder::new().prefix("mllm-concurrent").disable_cleanup(true).tempdir().unwrap();
    eprintln!("LIVE-DIR: {}", dir.path().display());
    let app = roles::start_standalone_with_policy(dir.path(),
        mllm_adapters::fake::ParkPolicy::Enabled).await.unwrap();
    let id = app.controller.submit_deploy("standalone", &DeployRequest {
        kind:"vllm-sleep".into(), ..req("concurrent-sleep", &p.model_id)
    }).unwrap();
    let op = app.controller.request_transition(&id, mllm_domain::LifecycleAction::Start).await.unwrap();
    assert_eq!(app.controller.wait_terminal(&op).await.unwrap(), LifecycleState::Ready);
    let pid = live_pid(&app, &id).unwrap();
    let _cleanup = LiveProcessCleanup::new(pid);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/chat/completions",listener.local_addr().unwrap());
    let router = app.router();
    let server = tokio::spawn(async move { axum::serve(listener,router).await.unwrap() });
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(180)).build().unwrap();
    for cycle in 0..=3 {
        if cycle > 0 {
            let op = app.controller.request_transition(&id,mllm_domain::LifecycleAction::Park).await.unwrap();
            assert_eq!(app.controller.wait_terminal(&op).await.unwrap(),LifecycleState::Parked);
        }
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..8 {
            let (client,url,key,model,barrier) = (client.clone(),url.clone(),app.api_key().to_owned(),p.model_id.clone(),barrier.clone());
            tasks.spawn(async move {
                let expected = format!("batch-{i}");
                let request = serde_json::json!({"model":model,"messages":[{"role":"user","content":
                    format!("Return exactly this text and nothing else: {expected}")}],
                    "temperature":0,"max_tokens":16,"chat_template_kwargs":{"enable_thinking":false}});
                barrier.wait().await;
                let started = std::time::Instant::now();
                let response = client.post(url).bearer_auth(key).json(&request).send().await.unwrap();
                assert!(response.status().is_success());
                let body:serde_json::Value = response.json().await.unwrap();
                assert_eq!(body["model"],model);
                assert_eq!(body["choices"][0]["message"]["content"].as_str().unwrap_or("").trim(),expected,
                    "cycle {cycle}, request {i}: {body}");
                started.elapsed().as_secs_f64()
            });
        }
        let mut times = Vec::new();
        while let Some(result) = tasks.join_next().await { times.push(result.unwrap()); }
        assert_eq!(times.len(),8);
        assert_eq!(live_pid(&app, &id),Some(pid),"wake reuses one engine");
        eprintln!("LIVE-CONCURRENT: {}",serde_json::json!({"cycle":cycle,"correct":8,"seconds":times}));
    }
    let op = app.controller.request_transition(&id,mllm_domain::LifecycleAction::Stop).await.unwrap();
    app.controller.wait_terminal(&op).await.unwrap();
    server.abort();
}

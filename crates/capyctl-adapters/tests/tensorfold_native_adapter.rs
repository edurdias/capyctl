//! ADR 0039 §9, §10 (T41): the TensorFold adapter on the native line,
//! against the testkit's native fake (`capyctl_testkit::tensorfold_native`):
//! readiness through a refused connection and 1.1.5's 503 bodies, the
//! `max_batch_size` check, the Initialize step without a build, and the
//! `/metrics` idle read. Nothing here qualifies a native engine recipe.
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use capyctl_adapters::llamacpp::EFFECTIVE_ARGS_MISMATCH;
use capyctl_adapters::tensorfold::{
    render_command, wait_idle, PlanInputTensorfold, TensorfoldAdapter,
};
use capyctl_adapters::traits::{
    EngineAdapter, EngineWork, MemberRef, Readiness, RuntimeAction, RuntimeCommand,
};
use capyctl_config::tensorfold_native::TensorfoldLine;
use capyctl_domain::completion::{
    ExecutionIdentities, Milestone, ProcessIdentity, StepExecutionContext, TransitionToken,
};
use capyctl_testkit::{FakeTensorfoldNative, NativeHealthMode, NativeRelease, ScriptedTool};

const SERVED: &str = "nemotron";

fn url(port: u16) -> reqwest::Url {
    format!("http://127.0.0.1:{port}").parse().unwrap()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn member() -> MemberRef {
    MemberRef {
        deployment_id: "d-1".into(),
        member_id: "b-1".into(),
    }
}

fn api_identity() -> ProcessIdentity {
    ProcessIdentity {
        role: "api".into(),
        pid: 4242,
        boot_id: "boot".into(),
        start_ticks: 99,
    }
}

fn reader(fake: &FakeTensorfoldNative, served: &str) -> TensorfoldAdapter {
    TensorfoldAdapter::new(url(fake.port()), "1.1.5".into(), served.into())
        .with_line(TensorfoldLine::Native)
}

fn native_plan(port: u16, warm_startup_ms: i64) -> PlanInputTensorfold {
    PlanInputTensorfold {
        line: TensorfoldLine::Native,
        engine_bin: "/home/u/tensorfold-1.0.0/bin/tensorfold-native".into(),
        model_path: "/srv/models/nemotron".into(),
        served_model_name: SERVED.into(),
        port,
        context_length: 32768,
        parallel: Some(8),
        warm_startup_ms,
        ..PlanInputTensorfold::default()
    }
}

fn initialize_command(deadline_in_ms: i64) -> RuntimeCommand {
    RuntimeCommand {
        action: RuntimeAction::Initialize,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: "d-1".into(),
                revision: 1,
                generation: 1,
                operation_id: "o-1".into(),
                step_id: "s-1".into(),
            },
            binding_id: "b-1".into(),
            incarnation: "i-1".into(),
            issued_at_ms: now_ms(),
            deadline_ms: now_ms() + deadline_in_ms,
            identities: ExecutionIdentities::OwnedLaunch,
            completion_target: None,
            grant_id: Some("g-1".into()),
            launch_settings: Some(capyctl_testkit::tensorfold_launch_settings()),
        },
    }
}

fn launching(plan: PlanInputTensorfold, port: u16) -> (TensorfoldAdapter, Arc<ScriptedTool>) {
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![]));
    let adapter = TensorfoldAdapter::new(url(port), "1.0.0".into(), SERVED.into())
        .with_launch(plan)
        .with_tools(tool.clone());
    (adapter, tool)
}

// T41 (ADR 0039 §9): a refused connection is still loading; 1.1.5's 503
// bodies and a key-configured bare `{"status": "ok"}` are not ready; ready
// needs `status: ok` with the served name in `model_ids` and `/v1/models`.
#[tokio::test]
async fn readiness_waits_through_refused_connections_and_503_bodies() {
    let fake = FakeTensorfoldNative::new(NativeRelease::V1_1_5, SERVED, 8);
    let adapter = reader(&fake, SERVED);
    let one = member();
    let ready = || adapter.check_readiness(&one);
    assert_eq!(ready().await.unwrap(), Readiness::Initializing, "refused");
    fake.set_health(NativeHealthMode::Unavailable);
    fake.load().await;
    assert_eq!(
        ready().await.unwrap(),
        Readiness::Initializing,
        "503 unavailable"
    );
    fake.set_health(NativeHealthMode::Draining);
    assert_eq!(
        ready().await.unwrap(),
        Readiness::Initializing,
        "503 draining"
    );
    fake.set_health(NativeHealthMode::KeyConfigured);
    assert_eq!(
        ready().await.unwrap(),
        Readiness::Initializing,
        "no model_ids"
    );
    fake.set_health(NativeHealthMode::Serving);
    assert_eq!(ready().await.unwrap(), Readiness::Ready);
    assert_eq!(
        reader(&fake, "another")
            .check_readiness(&member())
            .await
            .unwrap(),
        Readiness::Initializing,
        "the served name must be listed"
    );
    // The 0.6 read of the same engine never reads ready: the line decides.
    let python = TensorfoldAdapter::new(url(fake.port()), "1.1.5".into(), SERVED.into());
    assert!(!matches!(
        python.check_readiness(&member()).await,
        Ok(Readiness::Ready)
    ));
}

// T41 (ADR 0039 §9, §2): Initialize spawns the rendered native command with
// §5's environment, waits through the refused connection while the engine
// loads, probes once and reports one process; nothing is built.
#[tokio::test]
async fn initialize_waits_for_the_listener_and_reports_one_process() {
    let port = FakeTensorfoldNative::new(NativeRelease::V1_0_0, SERVED, 8).port();
    let plan = native_plan(port, 20_000);
    let argv = render_command(&plan).unwrap().argv;
    let fake = FakeTensorfoldNative::from_argv(NativeRelease::V1_0_0, &argv).unwrap();
    assert_eq!(fake.port(), port);
    fake.load_after(Duration::from_millis(1_200));
    let (adapter, tool) = launching(plan, port);
    let started = Instant::now();
    let observation = adapter
        .execute_persisted(&initialize_command(30_000))
        .await
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(1_200));
    assert_eq!(observation.identities, vec![api_identity()]);
    assert!(observation.facts.contains(&Milestone::ModelUsable));
    assert!(observation.kernel_builds.is_empty());
    assert_eq!(fake.chats(), 1, "one probe");
    let spawned = tool.spawned();
    assert_eq!(spawned[0].argv, argv);
    let env = &spawned[0].env;
    assert_eq!(env["TENSORFOLD_MEMORY_RESERVE_GIB"], "2");
    for absent in [
        "TORCH_EXTENSIONS_DIR",
        "MAX_JOBS",
        "CUDA_HOME",
        "TRANSFORMERS_OFFLINE",
    ] {
        assert!(!env.contains_key(absent), "{absent}");
    }
    assert_eq!(env["PATH"], capyctl_adapters::engine_env::SYSTEM_PATH);
}

// T41 (ADR 0039 §9): a 503 that lasts to the start bound fails the launch
// with its body in the failure.
#[tokio::test]
async fn a_503_to_the_start_bound_fails_with_its_body() {
    let fake = FakeTensorfoldNative::new(NativeRelease::V1_1_5, SERVED, 8);
    fake.set_health(NativeHealthMode::Draining);
    fake.set_work(2, 0);
    fake.load().await;
    let (adapter, _) = launching(native_plan(fake.port(), 1_500), fake.port());
    let started = Instant::now();
    let error = adapter
        .execute_persisted(&initialize_command(20_000))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "the ordinary bound"
    );
    assert!(
        error.contains("503")
            && error.contains(r#"{"status": "draining", "requests_in_progress": 2}"#),
        "{error}"
    );
    assert_eq!(fake.chats(), 0);
}

// T41 (SPEC §8.2, ADR 0039 §9): `/health` `max_batch_size` other than the
// rendered `--parallel` fails Initialize `effective_args_mismatch` before
// the probe; the arguments' own count is what is compared when they carry
// one.
#[tokio::test]
async fn a_different_max_batch_size_is_effective_args_mismatch() {
    let fake = FakeTensorfoldNative::new(NativeRelease::V1_0_0, SERVED, 8);
    fake.set_max_batch_size(4);
    fake.load().await;
    let (adapter, _) = launching(native_plan(fake.port(), 10_000), fake.port());
    let error = adapter
        .execute_persisted(&initialize_command(20_000))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(EFFECTIVE_ARGS_MISMATCH)
            && error.contains("max_batch_size 4")
            && error.contains("--parallel 8"),
        "{error}"
    );
    assert_eq!(fake.chats(), 0, "no probe");

    let port = FakeTensorfoldNative::new(NativeRelease::V1_0_0, SERVED, 8).port();
    let mut own = native_plan(port, 10_000);
    own.parallel = None;
    own.extra_args = vec!["--parallel".into(), "4".into()];
    let argv = render_command(&own).unwrap().argv;
    let fake = FakeTensorfoldNative::from_argv(NativeRelease::V1_0_0, &argv).unwrap();
    fake.load().await;
    let (adapter, _) = launching(own, port);
    adapter
        .execute_persisted(&initialize_command(20_000))
        .await
        .unwrap();
}

// T41 (ADR 0039 §10): the idle read is one `/metrics` scrape: both gauges 0
// signal at once; work in either holds the signal back at the bound.
#[tokio::test]
async fn the_idle_read_signals_on_zero_and_zero_and_holds_back_on_work() {
    let fake = FakeTensorfoldNative::new(NativeRelease::V1_0_0, SERVED, 8);
    fake.load().await;
    let adapter = reader(&fake, "");
    assert_eq!(
        adapter.idle_before_signal(&member()).await,
        Some(EngineWork::Idle)
    );
    let started = Instant::now();
    assert!(
        wait_idle(
            &adapter,
            &member(),
            Duration::from_secs(10),
            std::future::pending()
        )
        .await
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(adapter.engine_quiescent(&member(), 0).await);
    for (running, waiting) in [(1, 0), (0, 1)] {
        fake.set_work(running, waiting);
        assert_eq!(
            adapter.idle_before_signal(&member()).await,
            Some(EngineWork::Busy),
            "{running} {waiting}"
        );
    }
    let started = Instant::now();
    assert!(
        !wait_idle(
            &adapter,
            &member(),
            Duration::from_secs(1),
            std::future::pending()
        )
        .await
    );
    assert!(started.elapsed() >= Duration::from_secs(1));
}

// T41 (ADR 0039 §10): a refused connection is not listening and signals at
// once; a `/metrics` that does not answer signals when the bound ends.
#[tokio::test]
async fn a_refused_connection_signals_at_once_and_unanswered_metrics_at_the_bound() {
    let fake = FakeTensorfoldNative::new(NativeRelease::V1_1_5, SERVED, 8);
    let adapter = reader(&fake, "");
    assert_eq!(
        adapter.idle_before_signal(&member()).await,
        Some(EngineWork::NotListening)
    );
    let started = Instant::now();
    assert!(
        wait_idle(
            &adapter,
            &member(),
            Duration::from_secs(10),
            std::future::pending()
        )
        .await
    );
    assert!(started.elapsed() < Duration::from_secs(1));

    fake.load().await;
    fake.hold_metrics(true);
    assert_eq!(
        adapter.idle_before_signal(&member()).await,
        Some(EngineWork::Unanswered)
    );
    let started = Instant::now();
    assert!(
        wait_idle(
            &adapter,
            &member(),
            Duration::from_secs(1),
            std::future::pending()
        )
        .await
    );
    assert!(started.elapsed() >= Duration::from_secs(1));
}

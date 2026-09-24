//! ADR 0008 (owner decision 2026-09-23): the standalone role registers its
//! engine installation's fingerprint at boot, shows it (and any drift) in
//! status, journals a new drift, and refuses a drifted launch only when its
//! host policy says `installation_drift: refuse`.
//!
//! The installation is a synthetic package tree and the engine is the
//! testkit's Fake: CPU and Fake-engine evidence, never qualification.

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use mllm_config::effective::{InstallationDrift, ModelSource};
use mllm_controller::coordinator::{EngineBindings, ServiceClock, ToolsFactory};
use mllm_controller::{EngineInstallation, EngineProvider, LifecyclePort as _, ProviderError};
use mllm_domain::{LifecycleAction, LifecycleState};

use support::safe_state_dir;

/// A virtual environment holding a vLLM package; its `bin/python3` is the
/// installation's executable.
fn venv() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("bin")).unwrap();
    std::fs::write(dir.path().join("bin/python3"), "").unwrap();
    let site = dir.path().join("lib/python3.12/site-packages");
    std::fs::create_dir_all(site.join("vllm")).unwrap();
    std::fs::write(site.join("vllm/__init__.py"), "# build\n").unwrap();
    std::fs::create_dir_all(site.join("vllm-0.29.0.dist-info")).unwrap();
    std::fs::write(
        site.join("vllm-0.29.0.dist-info/METADATA"),
        "Name: vllm\nVersion: 0.29.0\n",
    )
    .unwrap();
    dir
}

fn patch(venv: &tempfile::TempDir) {
    std::fs::write(
        venv.path()
            .join("lib/python3.12/site-packages/vllm/__init__.py"),
        "# patched after boot\n",
    )
    .unwrap();
}

/// The testkit's Fake installation, started through the synthetic venv.
struct Provider {
    executable: PathBuf,
    policy: InstallationDrift,
}

impl EngineProvider for Provider {
    fn installation(&self) -> Result<EngineInstallation, ProviderError> {
        let mut installation = mllm_testkit::fake_installation();
        installation.executable = self.executable.clone();
        installation.installation_drift = self.policy;
        Ok(installation)
    }

    fn bindings(
        &self,
        clock: ServiceClock,
        log_dir: PathBuf,
        runtime_dir: PathBuf,
    ) -> Arc<dyn EngineBindings> {
        mllm_testkit::fake_bindings(clock, log_dir, runtime_dir)
    }

    fn tools_factory(&self) -> ToolsFactory {
        mllm_testkit::fake_tools_factory()
    }
}

async fn boot(
    state_dir: &std::path::Path,
    venv: &tempfile::TempDir,
    policy: InstallationDrift,
) -> mllm_cli::roles::App {
    mllm_cli::roles::start_standalone_with_memory(
        state_dir,
        Arc::new(Provider {
            executable: venv.path().join("bin/python3"),
            policy,
        }),
        support::test_memory(),
    )
    .await
    .expect("standalone boots")
}

/// Every journaled event of `kind`.
fn events(app: &mllm_cli::roles::App, kind: &str) -> Vec<serde_json::Value> {
    let mut found = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = app.store.events_after(after.as_deref(), 100).unwrap();
        if page.events.is_empty() {
            return found;
        }
        for event in &page.events {
            if event.kind == kind {
                found.push(serde_json::from_str(&event.payload_json).unwrap());
            }
        }
        after = Some(page.events.last().unwrap().cursor.to_string());
    }
}

async fn start(app: &mllm_cli::roles::App, deployment: &str) -> LifecycleState {
    let operation = app
        .controller
        .request_transition(deployment, LifecycleAction::Start)
        .await
        .expect("the start is accepted");
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        app.controller.wait_terminal(&operation),
    )
    .await
    .expect("the start settles rather than hanging");
    app.store
        .get_deployment(deployment)
        .unwrap()
        .unwrap()
        .observed_state
}

fn local(name: &str) -> ModelSource {
    ModelSource::Local {
        path: format!("/models/{name}"),
    }
}

// T21 T22: registered at boot and shown; a drift under the default `warn`
// policy is shown and journaled once, and the launch goes ahead.
#[tokio::test]
async fn standalone_shows_its_installation_and_warns_on_drift() {
    let dir = safe_state_dir();
    let venv = venv();
    let app = boot(dir.path(), &venv, InstallationDrift::Warn).await;
    let view = app.installation_view();
    assert_eq!(view["profile"], "local");
    assert_eq!(view["version"], "0.29.0");
    assert_eq!(view["state"], "measured");
    let registered = view["digest"].as_str().unwrap().to_owned();
    assert!(registered.starts_with("sha256:"), "{view}");

    let deployment = app.deploy("drift-warn", local("drift-warn")).unwrap();
    patch(&venv);
    assert_eq!(start(&app, &deployment).await, LifecycleState::Ready);
    let view = app.installation_view();
    assert_eq!(view["state"], "drifted");
    assert_eq!(view["digest"], registered.as_str());
    let observed = view["observed_digest"].as_str().unwrap().to_owned();
    assert_ne!(observed, registered);
    let flagged = events(&app, "installation_drift_flagged");
    assert_eq!(flagged.len(), 1, "{flagged:?}");
    assert_eq!(flagged[0]["installation"], "local");
    assert_eq!(flagged[0]["registered_digest"], registered.as_str());
    assert_eq!(flagged[0]["observed_digest"], observed.as_str());
}

// T21 T22: `installation_drift: refuse` refuses the drifted launch before any
// effect; the drift is still shown and journaled.
#[tokio::test]
async fn standalone_refuses_a_drifted_launch_under_refuse() {
    let dir = safe_state_dir();
    let venv = venv();
    let app = boot(dir.path(), &venv, InstallationDrift::Refuse).await;
    let deployment = app.deploy("drift-refuse", local("drift-refuse")).unwrap();
    patch(&venv);
    assert_ne!(start(&app, &deployment).await, LifecycleState::Ready);
    assert_eq!(app.installation_view()["state"], "drifted");
    assert_eq!(events(&app, "installation_drift_flagged").len(), 1);
}

// T21 T22: without drift a `refuse` host launches as before.
#[tokio::test]
async fn standalone_launches_an_unchanged_installation_under_refuse() {
    let dir = safe_state_dir();
    let venv = venv();
    let app = boot(dir.path(), &venv, InstallationDrift::Refuse).await;
    let deployment = app.deploy("unchanged", local("unchanged")).unwrap();
    assert_eq!(start(&app, &deployment).await, LifecycleState::Ready);
    assert_eq!(app.installation_view()["state"], "measured");
    assert!(events(&app, "installation_drift_flagged").is_empty());
}

// T21 T22: `status deployment` and `inspect deployment` on standalone carry
// the embedded installation, read over authenticated management.
#[tokio::test]
async fn standalone_status_and_inspect_show_the_installation() {
    let dir = safe_state_dir();
    let venv = venv();
    let app = boot(dir.path(), &venv, InstallationDrift::Warn).await;
    app.deploy("shown", local("shown")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app.management_router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    // Only this test in this binary reads the management address.
    std::env::set_var(mllm_cli::roles::MANAGEMENT_ADDR_ENV, address.to_string());

    // Unauthenticated reads are refused like every management read.
    let refused = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{address}/management/v1/installation"))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::UNAUTHORIZED);

    use mllm_cli::grammar::{Command, Resource};
    for command in [
        Command::Status {
            deployment: "shown".into(),
            watch: false,
        },
        Command::Inspect {
            resource: Resource::Deployment,
            id: Some("shown".into()),
            effective: false,
        },
    ] {
        let view = mllm_cli::client::execute(&command, dir.path())
            .await
            .unwrap_or_else(|failure| panic!("{}: {}", failure.code, failure.message));
        assert_eq!(view["name"], "shown");
        assert_eq!(view["installation"]["profile"], "local");
        assert_eq!(view["installation"]["version"], "0.29.0");
        assert_eq!(view["installation"]["state"], "measured");
    }
    // T14, SPEC §8.2: `inspect deployment --effective-config` serves the
    // resolved configuration with provenance, secrets redacted.
    let view = mllm_cli::client::execute(
        &Command::Inspect {
            resource: Resource::Deployment,
            id: Some("shown".into()),
            effective: true,
        },
        dir.path(),
    )
    .await
    .unwrap_or_else(|failure| panic!("{}: {}", failure.code, failure.message));
    assert_eq!(view["effective"]["name"], "shown", "{view}");
    assert_eq!(view["revision"], 1, "{view}");
    assert!(view["effective"]["engine_config"]["provenance"].is_object(), "{view}");
    assert!(!view.to_string().contains("secret://"), "{view}");
    server.abort();
}

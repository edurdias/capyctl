//! Role wiring for the F0 exit gate: `start standalone` boots the
//! embedded server+host graph in-process (no enrollment, no listeners —
//! F3 wires real transports) and returns an [`App`] handle over the
//! controller and the durable store. Every other parsed action still
//! reports a structured not-yet-implemented diagnostic.

use std::convert::Infallible;
use std::path::Path;
use std::rc::Rc;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mllm_agent::Host;
use mllm_config::defaults::resolve_startup;
use mllm_config::schema::ConfigKind;
use mllm_controller::Controller;
use mllm_store::Store;

use crate::grammar::Command;
use crate::output::{ExitCode, StructuredError};

pub const NOT_IMPLEMENTED_EXIT: ExitCode = ExitCode::UNSUPPORTED;

/// A booted standalone deployment graph: the controller operation engine
/// plus the durable store it runs against. Tests and the CLI drive
/// lifecycle through `controller`; the store is a separate connection to
/// the same WAL-backed file for direct observation. It is an `Rc`
/// (not `Arc`) because `Store` is not `Sync` and F0 observes it from one
/// thread; concurrent sharing comes with the F3 task architecture.
pub struct App {
    pub controller: Arc<Controller>,
    pub store: Rc<Store>,
    /// Servable router (F1: the standalone role's inference surface).
    router: axum::Router,
    deps: mllm_router::RouterDeps,
    api_key: String,
}

impl App {
    pub fn router(&self) -> axum::Router {
        self.router.clone()
    }

    pub fn deps(&self) -> &mllm_router::RouterDeps {
        &self.deps
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("config: {0}")]
    Config(#[from] mllm_config::error::ConfigError),
    #[error("store: {0}")]
    Store(#[from] mllm_store::StoreError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<StartError> for StructuredError {
    fn from(err: StartError) -> Self {
        let code = match err {
            StartError::Config(_) => "invalid_config",
            _ => "internal",
        };
        StructuredError {
            code,
            message: err.to_string(),
        }
    }
}

/// Boot the embedded standalone graph (SPEC §15.2 no-config matrix):
/// resolve the startup config (generating the standalone default on
/// first start), open the server store, and run the embedded host
/// (fake engine + fake launcher, deep-park policy denied by default)
/// against a controller bound to the same state directory.
pub async fn start_standalone(state_dir: &Path) -> Result<App, StartError> {
    start_standalone_inner(state_dir, mllm_adapters::fake::ParkPolicy::Denied).await
}

/// Boot with an explicit host deep-park policy (the Spark qualification
/// flow opts in for the isolated experimental profile).
pub async fn start_standalone_with_policy(
    state_dir: &Path,
    policy: mllm_adapters::fake::ParkPolicy,
) -> Result<App, StartError> {
    start_standalone_inner(state_dir, policy).await
}

async fn start_standalone_inner(
    state_dir: &Path,
    policy: mllm_adapters::fake::ParkPolicy,
) -> Result<App, StartError> {
    resolve_startup(ConfigKind::Standalone, None, state_dir)?;
    let db_path = state_dir.join("server").join("srv.sqlite3");
    let store = Rc::new(Store::open(&db_path)?);
    // The controller gets its own connection: rusqlite connections are
    // Send but not Sync, and spawned operation tasks need exclusive,
    // lock-guarded access.
    let controller_store = Arc::new(Mutex::new(Store::open(&db_path)?));
    let host = Host::new();
    let fake = Arc::new(mllm_adapters::fake::FakeEngine::new());
    let controller = Arc::new(
        Controller::new_with_policy(controller_store.clone(), host.adapter(), host.launcher(), policy)
            .with_embedded_fake(fake.clone()),
    );
    // The inference listener authenticates with the generated API key
    // (SPEC §15.2: local-only listeners with authentication).
    let api_key = read_api_key(state_dir).unwrap_or_else(|| "mllm-local".to_string());
    let deps = mllm_router::RouterDeps {
        store: controller_store.clone(),
        controller: controller.clone(),
        forwards: HashMap::from([
            ("model".to_string(), fake.clone() as Arc<dyn mllm_adapters::traits::ChatForward>),
            (
                "attached".to_string(),
                Arc::new(mllm_router::chat::NoForward) as Arc<dyn mllm_adapters::traits::ChatForward>,
            ),
        ]),
        limits: mllm_router::QueueLimits {
            max_requests_per_deployment: 32,
            max_buffered_bytes_total: 64 * 1024 * 1024,
        },
        api_key: Some(api_key.clone()),
        inflight: Arc::new(mllm_router::admission::InFlight::default()),
    };
    let router = mllm_router::serve_router(deps.clone(), "127.0.0.1:0".parse().unwrap());
    Ok(App { controller, store, router, deps, api_key })
}

/// Read the generated API key from the protected credentials file (F0's
/// fail-closed generation; the key is printed never, only used).
fn read_api_key(state_dir: &Path) -> Option<String> {
    let creds = std::fs::read_to_string(state_dir.join("identity").join("credentials")).ok()?;
    creds
        .lines()
        .find_map(|l| l.strip_prefix("api_key: ").map(str::to_string))
}

pub fn dispatch(command: &Command) -> Result<Infallible, StructuredError> {
    Err(StructuredError::not_yet_implemented(&command.label()))
}

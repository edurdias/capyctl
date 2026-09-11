//! Role wiring for the F0 exit gate: `start standalone` boots the
//! embedded server+host graph in-process (no enrollment, no listeners —
//! F3 wires real transports) and returns an [`App`] handle over the
//! controller and the durable store. Every other parsed action still
//! reports a structured not-yet-implemented diagnostic.

use std::convert::Infallible;
use std::path::Path;
use std::rc::Rc;
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
    pub controller: Controller,
    pub store: Rc<Store>,
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
    resolve_startup(ConfigKind::Standalone, None, state_dir)?;
    let db_path = state_dir.join("server").join("srv.sqlite3");
    let store = Rc::new(Store::open(&db_path)?);
    // The controller gets its own connection: rusqlite connections are
    // Send but not Sync, and spawned operation tasks need exclusive,
    // lock-guarded access.
    let controller_store = Arc::new(Mutex::new(Store::open(&db_path)?));
    let host = Host::new();
    let controller = Controller::new(controller_store, host.adapter(), host.launcher());
    Ok(App { controller, store })
}

pub fn dispatch(command: &Command) -> Result<Infallible, StructuredError> {
    Err(StructuredError::not_yet_implemented(&command.label()))
}

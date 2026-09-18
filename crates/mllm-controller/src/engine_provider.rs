//! Where a host's one engine installation comes from.
//!
//! The lifecycle needs three things that are properties of the installation rather
//! than of any deployment: what the host may publish about the engine it has, how a
//! frozen binding becomes an adapter spec, and which process tools a launch is given.
//! A provider supplies all three together, so a caller cannot publish a policy for
//! one engine and then drive a different one.
//!
//! The trait lives here rather than in the CLI because the test double and the
//! environment-driven implementation have no reason to share a crate, and because
//! nothing that implements it should have to depend on a command-line binary.

use std::path::PathBuf;
use std::sync::Arc;

use mllm_config::engine_policy::Engine;

use crate::coordinator::{EngineBindings, ServiceClock, ToolsFactory};

/// One engine a host actually has, described in the terms the host policy publishes.
///
/// Spec §7: everything the published table needs and nothing a deployment decides.
/// The launch settings arrive as JSON because they are a tagged family block that
/// the configuration layer validates; building a typed value here would duplicate
/// that validation in a second place where it could drift.
#[derive(Debug, Clone)]
pub struct EngineInstallation {
    /// The engine family this installation is.
    pub engine: Engine,
    /// The program that starts it.
    pub executable: PathBuf,
    /// What the host says this build is. It pins the recipe, so it must identify
    /// the installed engine rather than the host that happens to run it.
    pub build_fingerprint: String,
    /// The `launch_settings` block for this family, as the host policy carries it.
    pub launch_settings: serde_json::Value,
    /// Whether the deep-park controls may be called on this engine (SPEC §9.1, T21).
    pub deep_park: bool,
    /// Whether this installation may run an engine flag that executes Python
    /// shipped inside a checkpoint (Spec §3).
    pub trust_remote_code: bool,
    /// The directory the host keeps model weights under. Spec §7 resolves a
    /// relative model path against it.
    pub models_root: PathBuf,
    /// Where mllm's own guard middleware lives. It is not part of the frozen
    /// effective configuration: it is a property of this installation.
    pub runtime_dir: PathBuf,
    /// Startup flags the profile passes to the engine, beyond the ones mllm owns.
    /// They belong to the profile rather than to `launch_settings`, which is a
    /// closed per-family block.
    pub args: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// Nothing on this host names an engine that could be started. The message
    /// says what was expected, because a bare refusal leaves an operator guessing.
    #[error("{0}")]
    NoEngineInstallation(String),
}

/// Supplies the one engine installation a host offers, and the two seams the
/// coordinator needs in order to drive it.
pub trait EngineProvider: Send + Sync {
    /// What this host has, or a refusal naming what it expected to find.
    fn installation(&self) -> Result<EngineInstallation, ProviderError>;

    /// How a frozen binding becomes an adapter spec. `log_dir` is where each
    /// engine's own output is written and `runtime_dir` holds the guard middleware.
    fn bindings(
        &self,
        clock: ServiceClock,
        log_dir: PathBuf,
        runtime_dir: PathBuf,
    ) -> Arc<dyn EngineBindings>;

    /// The process tools a launch is given, built per launch around the
    /// association that records its API identity (Spec §3).
    fn tools_factory(&self) -> ToolsFactory;
}

//! TensorFold adapter module (ADR 0023). Engine-specific endpoints and launch
//! parameters for TensorFold live here and only here (SPEC §9).
pub mod adapter;
pub mod args;
mod frozen;
pub mod http;
mod idle;
mod initialize;

pub use adapter::TensorfoldAdapter;
pub use args::{
    engine_environment, render_command, PlanInputTensorfold, TensorfoldArgsError,
    ENGINE_ENV_ALLOWLIST,
};
pub use frozen::{plan_from_effective, TensorfoldPlanError};
pub use http::HealthReport;
pub use idle::wait_idle;

/// ADR 0023 §6: the longest the process owner waits for TensorFold's own
/// counters to read idle before a stop signal.
pub const ENGINE_IDLE_BOUND: std::time::Duration = std::time::Duration::from_secs(30);

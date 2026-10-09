//! llama.cpp adapter module (ADR 0029). Engine-specific endpoints and launch
//! parameters for `llama-server` live here and only here (SPEC §9): the
//! command and environment rendering, the shared launch builder, the bounded
//! reads of `/health`, `/v1/models`, `/props` and `/metrics`, and the
//! restart-only adapter. The wait before a stop signal is the shared
//! [`crate::tensorfold::wait_idle`] over this adapter's `/metrics` gauges.
pub mod adapter;
pub mod args;
mod frozen;
pub mod http;
mod initialize;

pub use adapter::LlamacppAdapter;
pub use args::{
    engine_environment, recheck_rendered, render_command, LlamacppArgsError, PlanInputLlamacpp,
    ENGINE_ENV_ALLOWLIST,
};
pub use frozen::{plan_from_effective, LlamacppDirs, LlamacppPlanError};
pub use http::{WorkGauges, READ_TIMEOUT as HEALTH_READ_TIMEOUT};
pub use initialize::{EFFECTIVE_ARGS_MISMATCH, ENGINE_CONFIG_FILE};

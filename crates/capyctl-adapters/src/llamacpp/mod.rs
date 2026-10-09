//! llama.cpp adapter module (ADR 0029). Engine-specific launch parameters for
//! `llama-server` live here and only here (SPEC §9). This slice holds the
//! command and environment rendering and the shared launch builder; the
//! readiness reads, the idle gate and the adapter itself follow (plan slice L3).
pub mod args;
mod frozen;

pub use args::{
    engine_environment, recheck_rendered, render_command, LlamacppArgsError, PlanInputLlamacpp,
    ENGINE_ENV_ALLOWLIST,
};
pub use frozen::{plan_from_effective, LlamacppDirs, LlamacppPlanError};

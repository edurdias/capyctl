//! TensorRT-LLM adapter module (ADR 0038; the Rust module spelling of the
//! kind `tensorrt-llm`). Engine-specific endpoints and launch parameters for
//! `trtllm-serve` live here and only here (SPEC §9): the command, the
//! configuration file and the launch directory, the closed environment with
//! the MPI pins, the shared launch builder, the bounded reads of `/health`,
//! `/version`, `/v1/models` and `/metrics`, the ownership checks run at
//! readiness and on every re-proof, and the restart-only adapter with no
//! idle signal.
pub mod adapter;
pub mod args;
mod frozen;
pub mod http;
mod initialize;
pub mod ownership;

pub use adapter::TensorrtLlmAdapter;
pub use args::{
    engine_environment, prepare_launch_dir, recheck_rendered, render_command, render_llm_api_yaml,
    PlanInputTensorrtLlm, TensorrtLlmArgsError, ENGINE_ENV_ALLOWLIST,
};
pub use frozen::{plan_from_effective, TensorrtLlmPlanError};
pub use http::{
    newest_load, parse_iteration_stats, IterationLoad, IterationStats, KvCacheStats,
    READ_TIMEOUT as HEALTH_READ_TIMEOUT,
};
pub use initialize::{EFFECTIVE_ARGS_MISMATCH, INSTALLATION_DRIFT, KV_CACHE_SHORT};
pub use ownership::{check_ownership, OwnershipFailure};

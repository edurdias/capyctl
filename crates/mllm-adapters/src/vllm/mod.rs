//! vLLM adapter module. Engine-specific endpoints and launch parameters for
//! vLLM live here and only here (SPEC §9).
pub mod adapter;
pub mod args;
mod frozen;
pub mod http;
mod initialize;
mod residency;

pub use frozen::{
    park_policy, plan_from_effective, sleep_flags, VllmPlanError, GPU_UTILIZATION_GATE_PCT,
};

pub use adapter::VllmAdapter;
pub use args::{
    fingerprint_of, interpreter_for, redact_text, render_command, ArgsError, GrantedBudget,
    PlanInputVllm, EXTRA_ARGS_MARKER, RESERVED_FLAGS, USER_ARGS_MARKER, VLLM_ENTRY,
};
pub use http::{EngineHttp, HttpError, SleepOutcome, StreamChunk, StreamEnd, WakeOutcome, WakeTag};
pub use initialize::ENGINE_ENV_ALLOWLIST;

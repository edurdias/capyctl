//! vLLM adapter module. Engine-specific endpoints and launch parameters for
//! vLLM live here and only here (SPEC §9).
pub mod adapter;
pub mod args;
pub mod http;
mod initialize;

pub use adapter::VllmAdapter;
pub use args::{
    fingerprint_of, redact_text, render_command, ArgsError, GrantedBudget, PlanInputVllm,
    RESERVED_FLAGS,
};
pub use http::{EngineHttp, HttpError, SleepOutcome, StreamChunk, StreamEnd, WakeOutcome};

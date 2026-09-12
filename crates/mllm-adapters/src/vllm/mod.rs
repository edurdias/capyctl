//! vLLM adapter module. Engine-specific endpoints and launch parameters for
//! vLLM live here and only here (SPEC §9).
pub mod adapter;
pub mod args;
pub mod http;

pub use adapter::VllmAdapter;
pub use args::{fingerprint_of, render_command, ArgsError, GrantedBudget, PlanInputVllm, RESERVED_FLAGS};
pub use http::{EngineHttp, HttpError, SleepOutcome, StreamChunk, StreamEnd, WakeOutcome};
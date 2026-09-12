//! vLLM adapter module. Engine-specific endpoints and launch parameters for
//! vLLM live here and only here (SPEC §9).
pub mod adapter;
pub mod http;

pub use adapter::VllmAdapter;
pub use http::{EngineHttp, HttpError, SleepOutcome, StreamChunk, StreamEnd, WakeOutcome};
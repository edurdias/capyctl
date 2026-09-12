//! vLLM adapter module. Engine-specific endpoints and launch parameters for
//! vLLM live here and only here (SPEC §9).
pub mod http;

pub use http::{EngineHttp, HttpError, SleepOutcome, StreamChunk, StreamEnd, WakeOutcome};
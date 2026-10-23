//! Pure rendering for the pinned native recipe, the owned launch this
//! family performs, and evidence observations for persisted controls. Rendering
//! grants no send authority.

mod adapter;
mod args;
mod forward;
mod http;
mod initialize;
// SPEC §9.2, §10: the observation key and engine gauges a host reads.
pub mod observation;
pub mod pinned;

pub use adapter::{
    ObservationAccess, SglangAdapter, SglangLaunchHandle, SglangRuntimeObservation,
    SglangRuntimeObserver,
};
pub use args::{ProtectedDescriptorFds, SglangLaunch};

mod frozen;
pub use frozen::frozen_from_effective;

/// ADR 0032 §1 (ADR 0017 gating): whether an SGLang launch can carry CapyCTL's
/// request ID as `rid` and have that request aborted with proof. Read from the
/// source of exactly 0.5.22: a later version gains it only with its own source
/// reading and live check. With more than one tokenizer worker an abort is
/// forwarded without the worker's local state, so it is not offered.
pub fn request_abort_supported(engine_version: &str, tokenizer_workers: u32) -> bool {
    engine_version.trim() == "0.5.22" && tokenizer_workers == 1
}

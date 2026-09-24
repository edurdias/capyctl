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

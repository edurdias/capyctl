//! Pure rendering for the pinned native recipe, the owned launch this
//! family performs, and evidence observations for persisted controls. Rendering
//! grants no send authority.

mod adapter;
mod args;
mod forward;
mod http;
mod initialize;

pub use adapter::{
    SglangAdapter, SglangLaunchHandle, SglangRuntimeObservation, SglangRuntimeObserver,
};
pub use args::{ProtectedDescriptorFds, SglangLaunch};

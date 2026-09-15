//! Pure rendering for the pinned candidate recipe. Rendering grants no send authority.

mod adapter;
mod args;
mod http;
mod forward;

pub use adapter::{SglangAdapter, SglangRuntimeObservation, SglangRuntimeObserver};
pub use args::{ProtectedDescriptorFds, SglangLaunch};

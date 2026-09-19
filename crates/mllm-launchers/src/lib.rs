//! Process launchers: owned-handle execution semantics.
pub mod exec;
mod durable;
mod ownership;
pub mod group_observation;
pub mod native_observation;
pub mod owned_launch;
pub mod process_absence;
pub use exec::ExecLauncher;
pub use durable::{
    AssociationError, DurableSpawn, DurableSpawnError, DurableSpawnOutcome, LaunchAssociation,
};
// The descriptor type lives in `mllm-adapters` next to the process tools that
// consume it; the launchers' public name for it is kept here.
pub use mllm_adapters::protected::ProtectedLaunchDescriptors;
pub use owned_launch::DurableProcessLaunch;
pub use ownership::ControllerLock;

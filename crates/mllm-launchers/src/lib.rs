//! Process launchers: owned-handle execution semantics.
pub mod exec;
mod durable;
mod ownership;
pub mod group_observation;
pub use exec::ExecLauncher;
pub use durable::{
    AssociationError, DurableSpawn, DurableSpawnError, DurableSpawnOutcome, LaunchAssociation,
    ProtectedLaunchDescriptors,
};
pub use ownership::ControllerLock;

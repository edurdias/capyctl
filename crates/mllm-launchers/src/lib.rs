//! Process launchers: owned-handle execution semantics.
pub mod exec;
mod durable;
mod ownership;
pub use exec::ExecLauncher;
pub use durable::{AssociationError, DurableSpawn, DurableSpawnError, DurableSpawnOutcome, LaunchAssociation};
pub use ownership::ControllerLock;

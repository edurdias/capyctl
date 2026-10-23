//! Process launchers: owned-handle execution semantics.
mod durable;
// SPEC §13.3 / T21: the redacting, rotating writer between an engine and its log.
pub mod engine_log_relay;
pub mod exec;
pub mod group_observation;
pub mod native_observation;
pub mod owned_launch;
mod ownership;
// SPEC §13.2, ADR 0028 §11: a launch's leader is reaped only once its group ends.
mod pinned_leader;
pub mod process_absence;
// SPEC §13.2 (W13): how a launched engine ended, recorded by its reaper.
pub mod reaped;
// ADR 0033 §10: per-launch spawn policy (core dumps, memory-only logs).
pub mod spawn_policy;
// SPEC §13.2 / T12: a role reaps the orphans of its engines.
pub mod subreaper;
pub use durable::{
    AssociationError, DurableSpawn, DurableSpawnError, DurableSpawnOutcome, LaunchAssociation,
};
pub use exec::ExecLauncher;
pub use spawn_policy::SpawnPolicy;
// The descriptor type lives in `capyctl-adapters` next to the process tools that
// consume it; the launchers' public name for it is kept here.
pub use capyctl_adapters::protected::ProtectedLaunchDescriptors;
pub use owned_launch::DurableProcessLaunch;
pub use ownership::ControllerLock;

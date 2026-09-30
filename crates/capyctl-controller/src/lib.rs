//! The controller operation engine: transactional deployment submission,
//! lifecycle transitions executed against engine/launcher participants,
//! and evidence-only journaling (design §5).

#[cfg(test)]
extern crate self as capyctl_controller;

// ADR 0014 §7 (WE3): checkpoint digests measured and recorded.
pub mod checkpoint_digests;
pub mod coordinator;
pub mod coordinator_port;
pub mod engine_bindings;
// SPEC §13.2 (W13): an owned engine that exited is closed and settled.
pub mod engine_exit;
pub mod enrollment;
pub mod host_publication;
pub mod profile_retirement;
// ADR 0008: the embedded host's installation fingerprint and drift.
pub mod engine_provider;
pub mod fault;
pub mod installation_gate;
pub mod local_readiness;
// ADR 0008: declared remote model sources materialized on their hosts.
pub mod model_sources;
pub mod native_launch;
pub mod operations;
pub mod ownership;
pub mod port;
pub mod request_leases;
pub mod runtime;
pub mod sequence;
pub mod sglang_observer;
// Review finding 15: supervisor child tasks end with their supervisor.
mod supervised;
// SPEC §10, ADR 0013 §8 (W10): request-driven switching.
pub mod switching;

pub use coordinator_port::{CoordinatorLifecycle, RoutingSignals};
pub use engine_bindings::ProfileBindings;
pub use engine_provider::{EngineInstallation, EngineProvider, ProviderError};
pub use fault::LifecycleFault;
pub use capyctl_adapters::traits::{RuntimeAction, RuntimeCommand, RuntimeError};
pub use capyctl_domain::completion;
pub use capyctl_domain::LifecycleAction;
pub use operations::{AttachRequest, Controller, ControllerError, DeployRequest, OperationHandle};
pub use ownership::{OwnedCoordinatorState, OwnedStateError};
pub use port::{LifecyclePort, RuntimeEndpoint, ServingInstance};
pub use request_leases::{LeaseEnd, LeaseRefused, RequestLease};
pub use runtime::{DurableRuntimeSupervisor, RuntimeBinding, RuntimeBindings, RuntimeOwnership};

pub mod agent_sessions;

pub mod latency_table;
pub mod load_table;

pub mod remote_execution;

pub mod remote_readiness;

//! The controller operation engine: transactional deployment submission,
//! lifecycle transitions executed against engine/launcher participants,
//! and evidence-only journaling (design §5).

#[cfg(test)]
extern crate self as capyctl_controller;

// SPEC §§6.2, 9.1: the embedded host's residency capability refusals.
pub mod capability_gate;
// ADR 0014 §7 (WE3): checkpoint digests measured and recorded.
pub mod checkpoint_digests;
pub mod coordinator;
// SPEC §§10, 17 (D9): the standalone role's engine load, sampled in process.
pub mod coordinator_port;
pub mod embedded_load;
pub mod engine_bindings;
// SPEC §13.2 (W13): an owned engine that exited is closed and settled.
pub mod engine_exit;
// SPEC §13.3 / T21: an instance's bounded, redacted engine log tail.
pub mod engine_logs;
pub mod enrollment;
pub mod host_publication;
pub mod profile_retirement;
// ADR 0028 §5–§9: reserve, prepare, launch and prove a multi-node group.
pub mod group_activation;
// ADR 0028 §12: group park and wake through the head, per-member evidence.
pub mod group_residency;
// ADR 0028 §11: whole-group stop, per-host settlement, uncertain members.
pub mod group_settlement;
// Owner decision 2026-10-09: a single launch's wake canary.
pub mod wake_canary;
// ADR 0028 §6: a group's weights on every host and cross-host digest agreement.
pub mod group_sources;
// ADR 0028 §11 (decided 2026-10-06): a stalled group request probes the head once.
pub mod group_stall;
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

pub use capyctl_adapters::traits::{RuntimeAction, RuntimeCommand, RuntimeError};
pub use capyctl_domain::completion;
pub use capyctl_domain::LifecycleAction;
pub use coordinator_port::{CoordinatorLifecycle, RoutingSignals};
pub use engine_bindings::ProfileBindings;
pub use engine_provider::{EngineInstallation, EngineProvider, ProviderError};
pub use fault::LifecycleFault;
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

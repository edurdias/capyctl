//! The controller operation engine: transactional deployment submission,
//! lifecycle transitions executed against engine/launcher participants,
//! and evidence-only journaling (design §5).

#[cfg(test)]
extern crate self as mllm_controller;

pub mod coordinator;
pub mod coordinator_port;
pub mod engine_bindings;
pub mod fault;
pub mod operations;
pub mod ownership;
pub mod port;
pub mod runtime;
pub mod sequence;
pub mod sglang_observer;

pub use mllm_domain::LifecycleAction;
pub use mllm_domain::completion;
pub use operations::{AttachRequest, Controller, ControllerError, DeployRequest, OperationHandle};
pub use ownership::{OwnedCoordinatorState, OwnedStateError};
pub use fault::LifecycleFault;
pub use coordinator_port::CoordinatorLifecycle;
pub use engine_bindings::ProfileBindings;
pub use port::LifecyclePort;
pub use runtime::{DurableRuntimeSupervisor, RuntimeBinding, RuntimeBindings, RuntimeOwnership};
pub use mllm_adapters::traits::{RuntimeAction, RuntimeCommand, RuntimeError};

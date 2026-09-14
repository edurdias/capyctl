//! The controller operation engine: transactional deployment submission,
//! lifecycle transitions executed against engine/launcher participants,
//! and evidence-only journaling (design §5).

pub mod operations;
pub mod runtime;
pub mod sequence;
pub mod qualification;

pub use mllm_domain::LifecycleAction;
pub use mllm_domain::completion;
pub use operations::{AttachRequest, Controller, ControllerError, DeployRequest, OperationHandle};
pub use runtime::{DurableRuntimeSupervisor, RuntimeBinding, RuntimeBindings, RuntimeOwnership};
pub use mllm_adapters::traits::{RuntimeAction, RuntimeCommand, RuntimeError};

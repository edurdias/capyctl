//! The controller operation engine: transactional deployment submission,
//! lifecycle transitions executed against engine/launcher participants,
//! and evidence-only journaling (design §5).

pub mod operations;

pub use mllm_domain::LifecycleAction;
pub use operations::{AttachRequest, Controller, ControllerError, DeployRequest, OperationHandle};

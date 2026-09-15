//! Owned, qualified Fake initialization. Production consumer cutover is pending.

mod worker;
pub use worker::*;

use mllm_store::candidate_creation::initialize::ArmResult;

/// Inspect the result returned by the current persisted arm attempt.
/// Reading an execution context or observing a recorded intent never permits send.
/// This predicate is not a capability and cannot replace the Store arm transaction.
pub fn permits_send(result: &ArmResult) -> bool {
    matches!(result, ArmResult::New { .. })
}

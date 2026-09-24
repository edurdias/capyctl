use crate::LifecycleState;

/// An illegal lifecycle transition was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("illegal lifecycle transition: {from:?} -> {to:?}")]
pub enum TransitionError {
    Illegal {
        from: LifecycleState,
        to: LifecycleState,
    },
}

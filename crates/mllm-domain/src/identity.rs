use std::sync::atomic::{AtomicU64, Ordering};

/// A globally unique deployment identifier (ULID).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeploymentId(pub ulid::Ulid);

impl DeploymentId {
    pub fn new() -> Self {
        Self(ulid::Ulid::new())
    }
}

impl Default for DeploymentId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for DeploymentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Monotonic generation counter value; never resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(pub u64);

/// Identifier for a long-running control-plane operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OperationId(pub String);

/// Account that owns a deployment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OwnerAccountId(pub String);

/// Returned when an observed generation is older than the expected current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stale generation: observed generation is older than the current one")]
pub struct StaleGenerationError;

/// Tracks the current generation of a deployment and rejects stale observations.
pub struct GenerationMonitor {
    current: AtomicU64,
}

impl GenerationMonitor {
    pub fn new() -> Self {
        Self {
            current: AtomicU64::new(0),
        }
    }

    /// Bump the generation and return the new value.
    pub fn advance(&self) -> Generation {
        Generation(self.current.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// The current generation.
    pub fn current(&self) -> Generation {
        Generation(self.current.load(Ordering::SeqCst))
    }

    /// Ok if `observed` is not older than the current generation.
    pub fn check(&self, observed: Generation) -> Result<Generation, StaleGenerationError> {
        if observed.0 >= self.current().0 {
            Ok(observed)
        } else {
            Err(StaleGenerationError)
        }
    }
}

impl Default for GenerationMonitor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_ids_are_sortable_and_stable() {
        let a = DeploymentId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = DeploymentId::new();
        assert!(a.0 < b.0, "ULIDs encode a millisecond timestamp");
        assert_eq!(a.to_string().len(), 26);
        assert_eq!(a, a.clone());
    }

    #[test]
    fn stale_generation_rejected() {
        let g = GenerationMonitor::new();
        let g1 = g.advance(); // observe generation 1
        assert!(g.check(g1).is_ok());
        let _ = g.advance();
        g.advance(); // two more transitions
        assert!(matches!(g.check(g1), Err(StaleGenerationError)));
    }

    #[test]
    fn generation_never_resets() {
        let g = GenerationMonitor::new();
        let before = g.current();
        g.advance();
        g.advance();
        assert!(g.current().0 > before.0);
    }
}

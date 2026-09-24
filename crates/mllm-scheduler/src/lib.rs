pub mod admission;
pub mod auto;
pub mod ledger;
pub mod placement;
pub mod residency;
pub mod sequence;
// ADR 0013 §8 (W10): victim choice for request-driven switching.
pub mod switching;

pub use admission::{admit, BlockReason, Candidate};
pub use auto::{resolve_auto, Diagnostic, ResolvedAuto, AUTO_POLICY_VERSION, OBSERVATION_TTL_SECS};
pub use ledger::{Category, Domain, DomainKind, HostLimits, Phase, Reservation};

pub mod admission;
pub mod auto;
// ADR 0019 (discrete GPU design §7): the GPU an instance runs on.
pub mod device_choice;
pub mod ledger;
pub mod placement;
pub mod residency;
pub mod sequence;
// ADR 0013 §8 (W10): victim choice for request-driven switching.
pub mod switching;

pub use admission::{admit, BlockReason, Candidate};
pub use auto::{resolve_auto, Diagnostic, ResolvedAuto, AUTO_POLICY_VERSION, OBSERVATION_TTL_SECS};
pub use ledger::{Category, Domain, DomainKind, HostLimits, Phase, Reservation};

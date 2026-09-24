//! The deep-park security gate: a product-level policy, not a property of any
//! single engine adapter.

/// Whether the deep-park controls may be called on this engine. Default permits
/// them; a host opts out (spec §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParkPolicy {
    /// Host permits the deep-park paths: level-2 park and weight reload.
    #[default]
    Enabled,
    /// Level-2 park and weight reload are deterministically refused.
    Disabled,
}

//! Numerical F2C phase-margin calculation, not qualification or attribution.
//!
//! The caller must establish attributable peaks over every required repetition,
//! complete ownership, retained allocations and conservative overhead. A global
//! free-memory delta is not an attributable peak. Missing attribution keeps the
//! existing conservative grant; it must never be represented by a zero sample.
//! This helper performs no I/O, changes no reservation and supplies no authority.

/// Add max(2 GiB, ceil(peak / 4)), rejecting an unrepresentable result.
/// Zero is a numerical input, not proof that a runtime retains no memory.
pub fn phase_bytes_with_margin(attributable_peak: u64) -> Option<u64> {
    let quarter = attributable_peak.div_ceil(4);
    attributable_peak.checked_add((2 << 30).max(quarter))
}

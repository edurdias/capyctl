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

/// Find the smallest whole-GiB ceiling admitting every planned intermediate
/// total while denying the direct wake total. Every input is a complete charged
/// demand, not a per-owner footprint or inferred free-memory delta. The caller
/// must derive these totals from qualified bounds and the actual durable ledger.
///
/// Reject absent/zero demands, more than 4096 scenario steps, and a safe ceiling
/// above F2C's 96-GiB maximum. None means this supplied scenario cannot establish
/// Q6; it never permits shrinking a grant or raising the safe ceiling.
pub fn pressure_case_ceiling(
    required_steps: &[u64],
    direct_wake: u64,
    safe_ceiling: u64,
) -> Option<u64> {
    const GIB: u64 = 1 << 30;
    if required_steps.is_empty()
        || required_steps.len() > 4096
        || safe_ceiling == 0
        || safe_ceiling > 96 * GIB
        || required_steps.contains(&0)
    {
        return None;
    }
    let required = *required_steps.iter().max()?;
    let ceiling = required.div_ceil(GIB).checked_mul(GIB)?;
    (ceiling <= safe_ceiling && ceiling < direct_wake).then_some(ceiling)
}

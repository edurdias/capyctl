pub const AUTO_POLICY_VERSION: u32 = 1;
pub const OBSERVATION_TTL_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAuto {
    pub managed_limit: i64,
    pub free_reserve: i64,
    pub policy_version: u32,
    pub observed_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: String,
    pub detail: String,
}

pub fn resolve_auto(observed: i64) -> Result<ResolvedAuto, Diagnostic> {
    #[allow(non_upper_case_globals)]
    const GiB: i64 = 1024 * 1024 * 1024;

    // Degenerate host: managed_limit = min(75% * observed, observed - 8 GiB)
    // falls to <= 8 GiB while free_reserve stays >= 8 GiB — permanently
    // un-admittable. Fail closed with a named diagnostic.
    if observed < 16 * GiB {
        return Err(Diagnostic {
            code: "no_safe_estimate".to_string(),
            detail: format!(
                "observed System memory {observed} bytes is below the 16 GiB \
                 minimum for a safe auto-resolution estimate"
            ),
        });
    }

    // managed_limit = min(75% of observed, observed - 8 GiB), integer math.
    // 75% is computed as observed/4*3 (not observed*3/4) to avoid i64 overflow
    // for large observations; this floors the 75% component to a quarter of a
    // byte of extra precision loss (no practical effect at GiB scale).
    let managed_limit = (observed / 4 * 3).min(observed - 8 * GiB);

    // free_reserve = max(8 GiB, floor(10% of observed)), where the 10%
    // component is computed in bytes and then floored to a whole-GiB
    // boundary: observed/10 gives the byte-floor of 10%, then dividing by
    // GiB and multiplying back truncates to the whole-GiB boundary below it.
    // E.g. 128 GiB observed -> 10% = 13,743,895,347 bytes (12.8 GiB) -> 12 GiB.
    let reserve_ten_percent = observed / 10 / GiB * GiB;
    let free_reserve = (8 * GiB).max(reserve_ten_percent);

    Ok(ResolvedAuto {
        managed_limit,
        free_reserve,
        policy_version: AUTO_POLICY_VERSION,
        observed_bytes: observed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(non_upper_case_globals)]
    const GiB: i64 = 1024 * 1024 * 1024;

    #[test]
    fn constants_match_adr_0005() {
        // 128 GiB observed: managed = min(0.75*128, 128-8) = 96 GiB;
        // reserve = max(8, floor(0.10*128)) = max(8, 12) = 12 GiB (10% floors to whole GiB).
        let r = resolve_auto(128 * GiB).unwrap();
        assert_eq!(r.managed_limit, 96 * GiB);
        assert_eq!(r.free_reserve, 12 * GiB);
        assert_eq!(r.policy_version, AUTO_POLICY_VERSION);
    }

    #[test]
    fn small_host_degenerates_with_diagnostic() {
        // Below 16 GiB observed, managed_limit would fall to <= 8 GiB while
        // free_reserve stays >= 8 GiB — permanently un-admittable, so fail closed
        // with a named diagnostic instead of resolving unusable limits.
        let d = resolve_auto(8 * GiB);
        assert!(matches!(d, Err(Diagnostic { code, .. }) if code == "no_safe_estimate"));
    }
}

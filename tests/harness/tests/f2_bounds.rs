#[path = "../src/f2_bounds.rs"]
mod bounds;

#[test]
fn phase_margin_uses_two_gib_floor_and_rounds_quarter_up() {
    for (peak, expected) in [
        (0, 2_147_483_648),
        (1, 2_147_483_649),
        (8_589_934_592, 10_737_418_240),
        (8_589_934_593, 10_737_418_242),
        (12_884_901_888, 16_106_127_360),
    ] {
        assert_eq!(bounds::phase_bytes_with_margin(peak), Some(expected));
    }
}

#[test]
fn phase_margin_never_wraps_or_saturates_an_unrepresentable_bound() {
    assert_eq!(
        bounds::phase_bytes_with_margin(14_757_395_258_967_641_292),
        Some(u64::MAX)
    );
    assert_eq!(
        bounds::phase_bytes_with_margin(14_757_395_258_967_641_293),
        None
    );
    assert_eq!(bounds::phase_bytes_with_margin(u64::MAX), None);
}

#[test]
fn pressure_case_chooses_smallest_whole_gib_covering_every_step() {
    assert_eq!(
        bounds::pressure_case_ceiling(
            &[1, 3_221_225_473, 2_147_483_648],
            5_368_709_120,
            6_442_450_944
        ),
        Some(4_294_967_296)
    );
    assert_eq!(
        bounds::pressure_case_ceiling(&[3_221_225_472], 3_221_225_473, 3_221_225_472),
        Some(3_221_225_472)
    );
    assert_eq!(
        bounds::pressure_case_ceiling(&[103_079_215_104], 103_079_215_105, 103_079_215_104),
        Some(103_079_215_104)
    );
}

#[test]
fn pressure_case_requires_a_safe_nonempty_interval() {
    // Equality admits the direct wake, so it cannot prove the pressure case.
    assert_eq!(
        bounds::pressure_case_ceiling(&[1], 1_073_741_824, 2_147_483_648),
        None
    );
    // Whole-GiB rounding must not breach the measured safe ceiling.
    assert_eq!(
        bounds::pressure_case_ceiling(&[1_073_741_825], 3_221_225_472, 2_147_483_647),
        None
    );
    assert_eq!(
        bounds::pressure_case_ceiling(&[], 3_221_225_472, 2_147_483_648),
        None
    );
    assert_eq!(
        bounds::pressure_case_ceiling(&[0], 3_221_225_472, 2_147_483_648),
        None
    );
    assert_eq!(bounds::pressure_case_ceiling(&[1], 3_221_225_472, 0), None);
    assert_eq!(
        bounds::pressure_case_ceiling(&[1], 3_221_225_472, 103_079_215_105),
        None
    );
    assert_eq!(
        bounds::pressure_case_ceiling(&[u64::MAX], u64::MAX, 103_079_215_104),
        None
    );
}

#[test]
fn pressure_case_bounds_scenario_input_before_scanning() {
    assert_eq!(
        bounds::pressure_case_ceiling(&[1; 4096], 2_147_483_648, 1_073_741_824),
        Some(1_073_741_824)
    );
    assert_eq!(
        bounds::pressure_case_ceiling(&[1; 4097], 2_147_483_648, 1_073_741_824),
        None
    );
}

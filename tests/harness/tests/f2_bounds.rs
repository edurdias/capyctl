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

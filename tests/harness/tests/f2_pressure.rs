use capyctl_agent::memory::parse_meminfo;
use harness::f2_pressure::{PressureAbort, PressureGuard, PressureStatus};

const GIB: i64 = 1 << 30;

fn sample(
    total: i64,
    available: i64,
    swap: i64,
    now: i64,
) -> capyctl_agent::memory::HostMemorySample {
    parse_meminfo(
        &format!(
            "MemTotal: {} kB\nMemAvailable: {} kB\nSwapTotal: 1048576 kB\nSwapFree: {} kB\n",
            total / 1024,
            available / 1024,
            1048576 - swap / 1024
        ),
        now,
    )
    .unwrap()
}

fn ready() -> PressureGuard {
    let mut guard = PressureGuard::default();
    for t in (0..=30_000).step_by(250) {
        let status = guard
            .observe(&sample(128 * GIB, 100 * GIB, 0, t), t as u64, t)
            .unwrap();
        assert_eq!(
            status,
            if t == 30_000 {
                PressureStatus::Ready
            } else {
                PressureStatus::Warming
            }
        );
    }
    guard
}

#[test]
fn stable_baseline_requires_full_thirty_seconds_and_derives_fixed_bounds() {
    let guard = ready();
    let bounds = guard.bounds().unwrap();
    assert_eq!(bounds.protected_bytes, 27_487_790_695);
    assert_eq!(bounds.managed_bytes, 96 * GIB);
}

#[test]
fn low_headroom_aborts_and_cannot_recover_under_same_guard() {
    let mut guard = ready();
    assert_eq!(
        guard.observe(&sample(128 * GIB, 25 * GIB, 0, 30_250), 30_250, 30_250),
        Err(PressureAbort::Headroom)
    );
    assert_eq!(
        guard.observe(&sample(128 * GIB, 100 * GIB, 0, 30_500), 30_500, 30_500),
        Err(PressureAbort::Headroom)
    );
}

#[test]
fn swap_growth_aborts_at_sixty_four_mib_or_three_consecutive_increases() {
    let mut guard = ready();
    assert_eq!(
        guard.observe(
            &sample(128 * GIB, 100 * GIB, 64 << 20, 30_250),
            30_250,
            30_250
        ),
        Err(PressureAbort::SwapGrowth)
    );
    let mut guard = ready();
    for (i, swap) in [1024, 2048, 3072].into_iter().enumerate() {
        let t = 30_250 + 250 * i as i64;
        let result = guard.observe(&sample(128 * GIB, 100 * GIB, swap, t), t as u64, t);
        assert_eq!(
            result,
            if i == 2 {
                Err(PressureAbort::SwapGrowth)
            } else {
                Ok(PressureStatus::Ready)
            }
        );
    }
}

#[test]
fn baseline_tracks_existing_swap_and_restarts_stability_window_on_change() {
    let mut guard = PressureGuard::default();
    for t in (0..=40_000).step_by(250) {
        let swap = if t < 10_000 { 128 << 20 } else { 127 << 20 };
        let status = guard
            .observe(&sample(80 * GIB, 64 * GIB, swap, t), t as u64, t)
            .unwrap();
        assert_eq!(
            status,
            if t == 40_000 {
                PressureStatus::Ready
            } else {
                PressureStatus::Warming
            }
        );
    }
    let bounds = guard.bounds().unwrap();
    assert_eq!(bounds.protected_bytes, 16 * GIB);
    assert_eq!(bounds.managed_bytes, 64 * GIB);
}

#[test]
fn stale_future_missing_and_reversed_samples_fail_closed() {
    for (monotonic, sampled, now) in [
        (30_250, 28_249, 30_250),
        (30_250, 30_251, 30_250),
        (32_001, 32_001, 32_001),
        (30_000, 30_250, 30_250),
        (29_999, 30_250, 30_250),
    ] {
        let mut guard = ready();
        assert_eq!(
            guard.observe(&sample(128 * GIB, 100 * GIB, 0, sampled), monotonic, now),
            Err(PressureAbort::Observation)
        );
    }
}

#[test]
fn invalid_or_changed_capacity_and_domain_are_not_new_baselines() {
    let mut guard = ready();
    assert_eq!(
        guard.observe(&sample(127 * GIB, 100 * GIB, 0, 30_250), 30_250, 30_250),
        Err(PressureAbort::Observation)
    );
    let mut guard = PressureGuard::default();
    assert_eq!(
        guard.observe(&sample(16 * GIB, 16 * GIB, 0, 0), 0, 0),
        Err(PressureAbort::Headroom)
    );
    for mutation in 0..4 {
        let mut bad = sample(128 * GIB, 100 * GIB, 0, 0);
        match mutation {
            0 => bad.memory.domain = "gpu0".into(),
            1 => bad.swap_used_bytes = -1,
            2 => bad.memory.available_bytes = -1,
            _ => bad.memory.available_bytes = 129 * GIB,
        }
        assert_eq!(
            PressureGuard::default().observe(&bad, 0, 0),
            Err(PressureAbort::Observation)
        );
    }
}

#[test]
fn watchdog_aborts_without_waiting_for_a_new_sample() {
    let mut guard = ready();
    assert_eq!(guard.status_at(32_000, 32_000), Ok(PressureStatus::Ready));
    assert_eq!(
        guard.status_at(32_001, 32_001),
        Err(PressureAbort::Observation)
    );
    assert_eq!(
        guard.observe(&sample(128 * GIB, 100 * GIB, 0, 32_250), 32_250, 32_250),
        Err(PressureAbort::Observation)
    );
    assert_eq!(
        PressureGuard::default().status_at(0, 0),
        Err(PressureAbort::Observation)
    );
}

#[test]
fn unchanged_swap_breaks_consecutive_growth_but_not_baseline_threshold() {
    let mut guard = ready();
    for (i, swap) in [1024, 2048, 2048, 3072, 4096].into_iter().enumerate() {
        let t = 30_250 + 250 * i as i64;
        assert_eq!(
            guard.observe(&sample(128 * GIB, 100 * GIB, swap, t), t as u64, t),
            Ok(PressureStatus::Ready)
        );
    }
    assert_eq!(
        guard.observe(
            &sample(128 * GIB, 100 * GIB, 64 << 20, 31_500),
            31_500,
            31_500
        ),
        Err(PressureAbort::SwapGrowth)
    );
}

use harness::f2_metrics::{LatencyRecorder, SampleOutcome, MAX_SAMPLES};
use std::time::Duration;

#[test]
fn reports_exact_counts_median_and_nearest_rank_p95() {
    let mut recorder = LatencyRecorder::default();
    for value in (1..=100).rev() {
        recorder
            .record(SampleOutcome::Completed(Duration::from_micros(value)))
            .unwrap();
    }
    recorder.record(SampleOutcome::Failed).unwrap();
    recorder.record(SampleOutcome::Failed).unwrap();
    recorder.record(SampleOutcome::TimedOut).unwrap();
    let summary = recorder.summary();
    assert_eq!(
        (
            summary.total,
            summary.completed,
            summary.failures,
            summary.timeouts
        ),
        (103, 100, 2, 1)
    );
    assert_eq!(summary.minimum, Some(Duration::from_micros(1)));
    assert_eq!(summary.median, Some(Duration::from_nanos(50_500)));
    assert_eq!(summary.p95, Some(Duration::from_micros(95)));
    assert_eq!(summary.maximum, Some(Duration::from_micros(100)));
    assert_eq!(recorder.summary(), summary);
}

#[test]
fn empty_or_failed_samples_never_become_zero_duration_success() {
    let mut recorder = LatencyRecorder::default();
    assert_eq!(recorder.summary().total, 0);
    recorder.record(SampleOutcome::Failed).unwrap();
    recorder.record(SampleOutcome::TimedOut).unwrap();
    let summary = recorder.summary();
    assert_eq!(
        (
            summary.total,
            summary.completed,
            summary.failures,
            summary.timeouts
        ),
        (2, 0, 1, 1)
    );
    assert_eq!(
        (
            summary.minimum,
            summary.median,
            summary.p95,
            summary.maximum
        ),
        (None, None, None, None)
    );
}

#[test]
fn small_sets_and_large_durations_do_not_overflow_or_use_float_rounding() {
    let mut recorder = LatencyRecorder::default();
    recorder
        .record(SampleOutcome::Completed(Duration::MAX))
        .unwrap();
    let one = recorder.summary();
    assert_eq!(one.minimum, Some(Duration::MAX));
    assert_eq!(one.median, Some(Duration::MAX));
    assert_eq!(one.p95, Some(Duration::MAX));
    recorder
        .record(SampleOutcome::Completed(Duration::ZERO))
        .unwrap();
    let two = recorder.summary();
    assert_eq!(two.median, Some(Duration::MAX / 2));
    assert_eq!(two.p95, Some(Duration::MAX));
    recorder
        .record(SampleOutcome::Completed(Duration::from_nanos(3)))
        .unwrap();
    assert_eq!(recorder.summary().median, Some(Duration::from_nanos(3)));
}

#[test]
fn every_outcome_consumes_bounded_capacity_without_silent_drop() {
    for outcome in [
        SampleOutcome::Failed,
        SampleOutcome::TimedOut,
        SampleOutcome::Completed(Duration::ZERO),
    ] {
        let mut recorder = LatencyRecorder::default();
        for _ in 0..MAX_SAMPLES {
            recorder.record(outcome).unwrap();
        }
        let full = recorder.summary();
        assert_eq!(full.total as usize, MAX_SAMPLES);
        assert!(recorder.record(outcome).is_err());
        assert_eq!(recorder.summary(), full);
    }
}

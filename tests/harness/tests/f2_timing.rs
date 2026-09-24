#[path = "../src/f2_timing.rs"]
mod f2_timing;

use f2_timing::{Interval, RequestTimeline, TimingError};

fn timeline() -> RequestTimeline {
    RequestTimeline {
        accepted_ns: 100,
        queue: Some(Interval {
            start_ns: 110,
            end_ns: 170,
        }),
        activation: Some(Interval {
            start_ns: 130,
            end_ns: 180,
        }),
        dispatched_ns: 190,
        first_token_ns: Some(210),
        backend_terminal_ns: 250,
        delivery_end_ns: 270,
    }
}

#[test]
fn overlapping_queue_and_activation_remain_separate() {
    let result = timeline().durations().unwrap();
    assert_eq!(result.total_ns, 170);
    assert_eq!(result.before_dispatch_ns, 90);
    assert_eq!(result.queue_ns, Some(60));
    assert_eq!(result.activation_ns, Some(50));
    assert_eq!(result.time_to_first_token_ns, Some(110));
    assert_eq!(result.backend_ns, 60);
    assert_eq!(result.delivery_tail_ns, 20);
    // Their overlap is real: the sum is greater than pre-dispatch time.
    assert!(result.queue_ns.unwrap() + result.activation_ns.unwrap() > result.before_dispatch_ns);
}

#[test]
fn direct_collected_response_does_not_invent_queue_activation_or_first_token() {
    let mut input = timeline();
    input.queue = None;
    input.activation = None;
    input.first_token_ns = None;
    let result = input.durations().unwrap();
    assert_eq!(result.queue_ns, None);
    assert_eq!(result.activation_ns, None);
    assert_eq!(result.time_to_first_token_ns, None);
    assert_eq!(result.total_ns, 170);
}

#[test]
fn required_chronology_rejects_reversed_boundaries() {
    for (accepted, dispatched, terminal, delivered) in [
        (191, 190, 250, 270),
        (100, 251, 250, 270),
        (100, 190, 271, 270),
    ] {
        let mut input = timeline();
        input.accepted_ns = accepted;
        input.dispatched_ns = dispatched;
        input.backend_terminal_ns = terminal;
        input.delivery_end_ns = delivered;
        assert_eq!(input.durations(), Err(TimingError::Chronology));
    }
}

#[test]
fn each_optional_interval_must_be_ordered_and_inside_predispatch_window() {
    for (start_ns, end_ns) in [(99, 170), (110, 191), (180, 179)] {
        for queue in [true, false] {
            let mut input = timeline();
            let interval = Some(Interval { start_ns, end_ns });
            if queue {
                input.queue = interval;
            } else {
                input.activation = interval;
            }
            assert_eq!(input.durations(), Err(TimingError::Interval));
        }
    }
    for first in [189, 251] {
        let mut input = timeline();
        input.first_token_ns = Some(first);
        assert_eq!(input.durations(), Err(TimingError::FirstToken));
    }
}

#[test]
fn equal_boundaries_and_full_u64_span_do_not_overflow_or_require_positive_duration() {
    let input = RequestTimeline {
        accepted_ns: u64::MAX,
        queue: Some(Interval {
            start_ns: u64::MAX,
            end_ns: u64::MAX,
        }),
        activation: Some(Interval {
            start_ns: u64::MAX,
            end_ns: u64::MAX,
        }),
        dispatched_ns: u64::MAX,
        first_token_ns: Some(u64::MAX),
        backend_terminal_ns: u64::MAX,
        delivery_end_ns: u64::MAX,
    };
    let result = input.durations().unwrap();
    assert_eq!(result.total_ns, 0);
    assert_eq!(result.before_dispatch_ns, 0);
    assert_eq!(result.queue_ns, Some(0));
    assert_eq!(result.activation_ns, Some(0));
    assert_eq!(result.time_to_first_token_ns, Some(0));
    assert_eq!(result.backend_ns, 0);
    assert_eq!(result.delivery_tail_ns, 0);
    let wide = RequestTimeline {
        accepted_ns: 0,
        queue: None,
        activation: None,
        dispatched_ns: 0,
        first_token_ns: Some(u64::MAX),
        backend_terminal_ns: u64::MAX,
        delivery_end_ns: u64::MAX,
    }
    .durations()
    .unwrap();
    assert_eq!(wide.total_ns, u64::MAX);
    assert_eq!(wide.backend_ns, u64::MAX);
    assert_eq!(wide.time_to_first_token_ns, Some(u64::MAX));
}

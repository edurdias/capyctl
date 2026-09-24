#[path = "../src/f2_records.rs"]
mod f2_records;
#[path = "../src/f2_timing.rs"]
mod f2_timing;

use f2_records::{Engine, Mode, Outcome, RecordError, RequestJournal, RequestRecord};
use f2_timing::RequestTimeline;
use std::io::{self, Write};

fn record(outcome: Outcome) -> RequestRecord {
    RequestRecord {
        case: 7,
        request: 9,
        engine: Engine::Sglang,
        mode: Mode::Streaming,
        outcome,
    }
}

fn timeline() -> RequestTimeline {
    RequestTimeline {
        accepted_ns: 10,
        queue: None,
        activation: None,
        dispatched_ns: 20,
        first_token_ns: Some(25),
        backend_terminal_ns: 40,
        delivery_end_ns: 45,
    }
}

#[test]
fn emits_closed_jsonl_with_separate_outcomes_and_monotonic_durations() {
    let mut bytes = Vec::new();
    let mut journal = RequestJournal::new(&mut bytes, 4096).unwrap();
    journal
        .append(record(Outcome::Completed(timeline())))
        .unwrap();
    journal.append(record(Outcome::Failed)).unwrap();
    journal
        .append(RequestRecord {
            engine: Engine::Vllm,
            mode: Mode::NonStreaming,
            ..record(Outcome::TimedOut)
        })
        .unwrap();
    journal.finish().unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), concat!(
        "{\"sequence\":0,\"case\":7,\"request\":9,\"engine\":\"sglang\",\"mode\":\"streaming\",\"outcome\":\"completed\",\"total_ns\":35,\"before_dispatch_ns\":10,\"queue_ns\":null,\"activation_ns\":null,\"time_to_first_token_ns\":15,\"backend_ns\":20,\"delivery_tail_ns\":5}\n",
        "{\"sequence\":1,\"case\":7,\"request\":9,\"engine\":\"sglang\",\"mode\":\"streaming\",\"outcome\":\"failed\"}\n",
        "{\"sequence\":2,\"case\":7,\"request\":9,\"engine\":\"vllm\",\"mode\":\"nonstreaming\",\"outcome\":\"timed_out\"}\n"
    ));
}

#[test]
fn invalid_timeline_never_writes_or_consumes_sequence() {
    let mut bytes = Vec::new();
    let mut journal = RequestJournal::new(&mut bytes, 4096).unwrap();
    let mut bad = timeline();
    bad.dispatched_ns = 9;
    assert_eq!(
        journal.append(record(Outcome::Completed(bad))),
        Err(RecordError::Timing)
    );
    journal.append(record(Outcome::Failed)).unwrap();
    journal.finish().unwrap();
    assert!(String::from_utf8(bytes)
        .unwrap()
        .starts_with("{\"sequence\":0,"));
}

#[test]
fn exact_budget_accepts_whole_record_and_overflow_latches_without_partial_line() {
    let mut reference = Vec::new();
    let mut journal = RequestJournal::new(&mut reference, 4096).unwrap();
    journal.append(record(Outcome::Failed)).unwrap();
    journal.finish().unwrap();
    let mut bytes = Vec::new();
    let mut journal = RequestJournal::new(&mut bytes, reference.len() as u64).unwrap();
    journal.append(record(Outcome::Failed)).unwrap();
    assert_eq!(
        journal.append(record(Outcome::Failed)),
        Err(RecordError::Limit)
    );
    assert_eq!(
        journal.append(record(Outcome::TimedOut)),
        Err(RecordError::Stopped)
    );
    assert_eq!(journal.finish(), Err(RecordError::Stopped));
    assert_eq!(bytes, reference);
    assert!(RequestJournal::new(Vec::new(), 0).is_err());
    assert!(RequestJournal::new(Vec::new(), 104_857_601).is_err());
    assert!(RequestJournal::new(Vec::new(), 104_857_600).is_ok());
}

struct BrokenSink {
    bytes: Vec<u8>,
    zero: bool,
    calls: usize,
}
impl Write for BrokenSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        if self.calls == 1 {
            self.bytes.extend_from_slice(&bytes[..3]);
            return Ok(3);
        }
        if self.zero {
            Ok(0)
        } else {
            Err(io::Error::other("private sink diagnostic"))
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        panic!("failed writer must not flush")
    }
}

#[test]
fn partial_or_zero_write_stops_without_retry_or_exposing_sink_diagnostic() {
    for zero in [false, true] {
        let mut sink = BrokenSink {
            bytes: Vec::new(),
            zero,
            calls: 0,
        };
        let mut journal = RequestJournal::new(&mut sink, 4096).unwrap();
        assert_eq!(
            journal.append(record(Outcome::Failed)),
            Err(RecordError::Io)
        );
        assert_eq!(
            journal.append(record(Outcome::Failed)),
            Err(RecordError::Stopped)
        );
        assert_eq!(journal.finish(), Err(RecordError::Stopped));
        assert_eq!(sink.bytes, b"{\"s");
        assert_eq!(sink.calls, 2);
    }
}

struct FlushFailure(Vec<u8>);
impl Write for FlushFailure {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("private"))
    }
}

#[test]
fn finish_reports_flush_failure_instead_of_claiming_complete_artifact() {
    let mut journal = RequestJournal::new(FlushFailure(Vec::new()), 4096).unwrap();
    journal.append(record(Outcome::Failed)).unwrap();
    assert_eq!(journal.finish(), Err(RecordError::Io));
}

struct InterruptedSink {
    bytes: Vec<u8>,
    interrupted: bool,
}
impl Write for InterruptedSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.interrupted {
            self.interrupted = true;
            return Err(io::ErrorKind::Interrupted.into());
        }
        self.bytes.push(bytes[0]);
        Ok(1)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn interrupted_and_short_writes_preserve_one_complete_record() {
    let mut sink = InterruptedSink {
        bytes: Vec::new(),
        interrupted: false,
    };
    let mut journal = RequestJournal::new(&mut sink, 4096).unwrap();
    journal.append(record(Outcome::Failed)).unwrap();
    journal.finish().unwrap();
    assert_eq!(sink.bytes, b"{\"sequence\":0,\"case\":7,\"request\":9,\"engine\":\"sglang\",\"mode\":\"streaming\",\"outcome\":\"failed\"}\n");
}

#[test]
fn optional_intervals_and_full_width_values_remain_bounded_numeric_metadata() {
    use f2_timing::Interval;
    let mut bytes = Vec::new();
    let mut journal = RequestJournal::new(&mut bytes, 1024).unwrap();
    journal
        .append(record(Outcome::Completed(RequestTimeline {
            accepted_ns: 0,
            dispatched_ns: u64::MAX,
            backend_terminal_ns: u64::MAX,
            delivery_end_ns: u64::MAX,
            queue: Some(Interval {
                start_ns: 0,
                end_ns: u64::MAX,
            }),
            activation: Some(Interval {
                start_ns: 0,
                end_ns: u64::MAX,
            }),
            first_token_ns: None,
        })))
        .unwrap();
    journal.finish().unwrap();
    let json = String::from_utf8(bytes).unwrap();
    assert!(
        json.contains("\"queue_ns\":18446744073709551615,\"activation_ns\":18446744073709551615,")
    );
    assert!(
        json.contains("\"time_to_first_token_ns\":null,\"backend_ns\":0,\"delivery_tail_ns\":0")
    );
    assert!(json.len() < 1024);
}

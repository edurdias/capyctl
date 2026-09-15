//! Closed, bounded JSONL request metadata for the F2C runner.
//!
//! No free-text fields accept prompts, responses, credentials or diagnostics.
//! Corpus ordinals are labels, not verified route or operation identities.
//! The caller must persist its manifest first and provide a protected sink.
//! This helper does not create files, establish clock provenance, authorize
//! execution or make a qualification claim. Flush is not filesystem sync.
use crate::f2_timing::RequestTimeline;
use std::io::Write;

pub enum Engine {
    Vllm,
    Sglang,
}
pub enum Mode {
    Streaming,
    NonStreaming,
}
pub enum Outcome {
    Completed(RequestTimeline),
    Failed,
    TimedOut,
}
pub struct RequestRecord {
    pub case: u16,
    pub request: u16,
    pub engine: Engine,
    pub mode: Mode,
    pub outcome: Outcome,
}
#[derive(Debug, PartialEq, Eq)]
pub enum RecordError {
    Timing,
    Limit,
    Io,
    Stopped,
}
/// One writer for one request journal. The run must reserve other artifact bytes
/// separately: this budget is not a global filesystem or multi-writer quota.
pub struct RequestJournal<W> {
    sink: W,
    remaining: u64,
    sequence: u64,
    stopped: bool,
}
impl<W: Write> RequestJournal<W> {
    /// Require a positive budget no greater than the full 100-MiB run ceiling.
    pub fn new(sink: W, budget: u64) -> Result<Self, RecordError> {
        if budget == 0 || budget > 100 * 1024 * 1024 {
            return Err(RecordError::Limit);
        }
        Ok(Self {
            sink,
            remaining: budget,
            sequence: 0,
            stopped: false,
        })
    }

    /// Validate and encode one whole record before writing. Invalid timing has
    /// no side effects. A limit or I/O failure permanently stops this writer;
    /// a partially written final line is retained, never replayed or repaired.
    pub fn append(&mut self, record: RequestRecord) -> Result<(), RecordError> {
        if self.stopped {
            return Err(RecordError::Stopped);
        }
        let (outcome, durations) = match record.outcome {
            Outcome::Completed(timeline) => (
                "completed",
                Some(timeline.durations().map_err(|_| RecordError::Timing)?),
            ),
            Outcome::Failed => ("failed", None),
            Outcome::TimedOut => ("timed_out", None),
        };
        let engine = match record.engine {
            Engine::Vllm => "vllm",
            Engine::Sglang => "sglang",
        };
        let mode = match record.mode {
            Mode::Streaming => "streaming",
            Mode::NonStreaming => "nonstreaming",
        };
        // Every value is a fixed enum or bounded integer. Even u64::MAX values
        // keep this allocation below 1 KiB; no caller-supplied strings exist.
        let mut line = format!("{{\"sequence\":{},\"case\":{},\"request\":{},\"engine\":\"{}\",\"mode\":\"{}\",\"outcome\":\"{}\"",
            self.sequence, record.case, record.request, engine, mode, outcome);
        if let Some(d) = durations {
            let optional =
                |value: Option<u64>| value.map_or_else(|| "null".to_owned(), |n| n.to_string());
            line.push_str(&format!(",\"total_ns\":{},\"before_dispatch_ns\":{},\"queue_ns\":{},\"activation_ns\":{},\"time_to_first_token_ns\":{},\"backend_ns\":{},\"delivery_tail_ns\":{}",
                d.total_ns, d.before_dispatch_ns, optional(d.queue_ns), optional(d.activation_ns),
                optional(d.time_to_first_token_ns), d.backend_ns, d.delivery_tail_ns));
        }
        line.push_str("}\n");
        if line.len() as u64 > self.remaining {
            self.stopped = true;
            return Err(RecordError::Limit);
        }
        // Poison before I/O: a sink panic also cannot permit another append.
        self.stopped = true;
        self.sink
            .write_all(line.as_bytes())
            .map_err(|_| RecordError::Io)?;
        self.remaining -= line.len() as u64;
        // At least one byte per record and the 100-MiB bound prevent overflow.
        self.sequence += 1;
        self.stopped = false;
        Ok(())
    }

    /// Consume the writer. A successful flush means only the sink accepted it;
    /// the artifact owner still controls durable sync and run closeout.
    pub fn finish(mut self) -> Result<(), RecordError> {
        if self.stopped {
            return Err(RecordError::Stopped);
        }
        self.sink.flush().map_err(|_| RecordError::Io)
    }
}

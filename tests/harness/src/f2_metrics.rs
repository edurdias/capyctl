//! Bounded F2C latency summaries, not qualification or timing attribution.
//!
//! Keep a separate recorder for each case, engine, mode and measured interval.
//! Queue and activation intervals can overlap; never add their summaries to
//! invent a latency decomposition. Callers supply monotonic elapsed durations.
use std::time::Duration;

/// Fits the fixed F2C corpus, including its 320-request switching case, while
/// bounding stored samples and temporary sorting memory. Overflow is explicit.
pub const MAX_SAMPLES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleOutcome {
    Completed(Duration),
    Failed,
    TimedOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleLimit;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatencySummary {
    pub total: u32,
    pub completed: u32,
    /// Failed non-timeout attempts. Timeouts are counted separately.
    pub failures: u32,
    pub timeouts: u32,
    /// Latencies include only completed attempts. No samples means None, not 0.
    pub minimum: Option<Duration>,
    pub median: Option<Duration>,
    pub p95: Option<Duration>,
    pub maximum: Option<Duration>,
}

#[derive(Default)]
pub struct LatencyRecorder {
    completed: Vec<Duration>,
    failures: u32,
    timeouts: u32,
}

impl LatencyRecorder {
    /// Refuse overflow without changing any count or replacing an older sample.
    /// A failure/timeout is never a zero-duration successful measurement.
    pub fn record(&mut self, outcome: SampleOutcome) -> Result<(), SampleLimit> {
        if self.total() as usize >= MAX_SAMPLES {
            return Err(SampleLimit);
        }
        match outcome {
            SampleOutcome::Completed(duration) => self.completed.push(duration),
            SampleOutcome::Failed => self.failures += 1,
            SampleOutcome::TimedOut => self.timeouts += 1,
        }
        Ok(())
    }

    fn total(&self) -> u32 {
        self.completed.len() as u32 + self.failures + self.timeouts
    }

    /// Median averages the middle pair, rounding down by at most half a
    /// nanosecond. P95 uses nearest rank, ceil(0.95 * n), without floating point.
    /// This read neither resets nor otherwise changes the recorded outcomes.
    pub fn summary(&self) -> LatencySummary {
        let mut sorted = self.completed.clone();
        sorted.sort_unstable();
        let n = sorted.len();
        let (median, p95) = if n == 0 {
            (None, None)
        } else {
            let median = if n % 2 == 1 {
                sorted[n / 2]
            } else {
                // Duration::MAX is less than 2^94 nanoseconds. The sum of two
                // durations fits u128, and their mean still fits Duration.
                let nanos = (sorted[n / 2 - 1].as_nanos() + sorted[n / 2].as_nanos()) / 2;
                Duration::new(
                    (nanos / 1_000_000_000) as u64,
                    (nanos % 1_000_000_000) as u32,
                )
            };
            (Some(median), Some(sorted[(95 * n).div_ceil(100) - 1]))
        };
        LatencySummary {
            total: self.total(),
            completed: n as u32,
            failures: self.failures,
            timeouts: self.timeouts,
            minimum: sorted.first().copied(),
            median,
            p95,
            maximum: sorted.last().copied(),
        }
    }
}

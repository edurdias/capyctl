//! Process-local monotonic timing validation for completed F2C requests.
//!
//! The runner supplies offsets from one monotonic clock origin. This helper
//! cannot establish clock provenance, observe execution, or compare processes.
//! Failed and timed-out requests belong in separate outcome counters, not a
//! fabricated completed timeline. No prompt, response or identifier is retained.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub start_ns: u64,
    pub end_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestTimeline {
    pub accepted_ns: u64,
    pub queue: Option<Interval>,
    pub activation: Option<Interval>,
    pub dispatched_ns: u64,
    pub first_token_ns: Option<u64>,
    pub backend_terminal_ns: u64,
    pub delivery_end_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimingError {
    Chronology,
    Interval,
    FirstToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestDurations {
    pub total_ns: u64,
    pub before_dispatch_ns: u64,
    pub queue_ns: Option<u64>,
    pub activation_ns: Option<u64>,
    pub time_to_first_token_ns: Option<u64>,
    pub backend_ns: u64,
    pub delivery_tail_ns: u64,
}

impl RequestTimeline {
    /// Validate before subtracting. Queue and activation can overlap and are
    /// intentionally not summed. A missing first-token observation remains None
    /// even when a collected response is otherwise complete.
    pub fn durations(self) -> Result<RequestDurations, TimingError> {
        if self.accepted_ns > self.dispatched_ns
            || self.dispatched_ns > self.backend_terminal_ns
            || self.backend_terminal_ns > self.delivery_end_ns
        {
            return Err(TimingError::Chronology);
        }
        let interval = |value: Interval| {
            if value.start_ns < self.accepted_ns
                || value.start_ns > value.end_ns
                || value.end_ns > self.dispatched_ns
            {
                Err(TimingError::Interval)
            } else {
                Ok(value.end_ns - value.start_ns)
            }
        };
        let queue_ns = self.queue.map(interval).transpose()?;
        let activation_ns = self.activation.map(interval).transpose()?;
        let time_to_first_token_ns = self
            .first_token_ns
            .map(|first| {
                if first < self.dispatched_ns || first > self.backend_terminal_ns {
                    Err(TimingError::FirstToken)
                } else {
                    Ok(first - self.accepted_ns)
                }
            })
            .transpose()?;
        Ok(RequestDurations {
            total_ns: self.delivery_end_ns - self.accepted_ns,
            before_dispatch_ns: self.dispatched_ns - self.accepted_ns,
            queue_ns,
            activation_ns,
            time_to_first_token_ns,
            backend_ns: self.backend_terminal_ns - self.dispatched_ns,
            delivery_tail_ns: self.delivery_end_ns - self.backend_terminal_ns,
        })
    }
}

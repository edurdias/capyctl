//! Fixed F2C host-pressure rules, independent of engine effects and authority.
//!
//! The runner must collect trusted local host samples every 250 ms, use its own
//! monotonic clock, and enforce a two-second watchdog even if collection stalls.
//! `Ready` means only that this pressure check passes at this observation. It
//! does not grant admission, establish host identity, or prove owner attribution.
//! Any error latches for the whole run; a later good sample cannot resume it.
use capyctl_agent::memory::HostMemorySample;

const GIB: i64 = 1 << 30;
const MAX_AGE_MS: u64 = 2_000;
const BASELINE_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressureStatus {
    Warming,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressureAbort {
    Observation,
    Headroom,
    SwapGrowth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PressureBounds {
    pub protected_bytes: i64,
    pub managed_bytes: i64,
}

struct Baseline {
    capacity: i64,
    bounds: PressureBounds,
    swap: i64,
    previous_swap: i64,
    stable_since: u64,
    previous_monotonic: u64,
    previous_sampled: i64,
    previous_now: i64,
    increases: u8,
    ready: bool,
}

#[derive(Default)]
pub struct PressureGuard {
    baseline: Option<Baseline>,
    aborted: Option<PressureAbort>,
}

impl PressureGuard {
    /// Bounds are diagnostics, not a grant; retained after abort for reporting.
    pub fn bounds(&self) -> Option<PressureBounds> {
        self.baseline.as_ref().map(|b| b.bounds)
    }

    /// Call from the runner's watchdog and immediately before new effects.
    /// This performs no I/O, so a blocked collector cannot keep an old pass alive.
    pub fn status_at(
        &mut self,
        monotonic_ms: u64,
        now_ms: i64,
    ) -> Result<PressureStatus, PressureAbort> {
        if let Some(reason) = self.aborted {
            return Err(reason);
        }
        let Some(baseline) = &self.baseline else {
            self.aborted = Some(PressureAbort::Observation);
            return Err(PressureAbort::Observation);
        };
        if monotonic_ms < baseline.previous_monotonic
            || monotonic_ms - baseline.previous_monotonic > MAX_AGE_MS
            || now_ms < baseline.previous_now
            || now_ms - baseline.previous_sampled > MAX_AGE_MS as i64
        {
            self.aborted = Some(PressureAbort::Observation);
            return Err(PressureAbort::Observation);
        }
        Ok(if baseline.ready {
            PressureStatus::Ready
        } else {
            PressureStatus::Warming
        })
    }

    pub fn observe(
        &mut self,
        sample: &HostMemorySample,
        monotonic_ms: u64,
        now_ms: i64,
    ) -> Result<PressureStatus, PressureAbort> {
        if let Some(reason) = self.aborted {
            return Err(reason);
        }
        let result = self.check(sample, monotonic_ms, now_ms);
        if let Err(reason) = result {
            self.aborted = Some(reason);
        }
        result
    }

    fn check(
        &mut self,
        sample: &HostMemorySample,
        monotonic: u64,
        now: i64,
    ) -> Result<PressureStatus, PressureAbort> {
        let memory = &sample.memory;
        if memory.domain != "system"
            || memory.capacity_bytes <= 0
            || memory.available_bytes < 0
            || memory.available_bytes > memory.capacity_bytes
            || sample.swap_used_bytes < 0
            || memory.sampled_at_ms < 0
            || now < memory.sampled_at_ms
            || now - memory.sampled_at_ms > MAX_AGE_MS as i64
        {
            return Err(PressureAbort::Observation);
        }
        if self.baseline.is_none() {
            // Quotient plus remainder avoids overflow near i64::MAX.
            let fifth = memory.capacity_bytes / 5 + i64::from(memory.capacity_bytes % 5 != 0);
            let protected_bytes = (16 * GIB).max(fifth);
            if memory.capacity_bytes <= protected_bytes {
                return Err(PressureAbort::Headroom);
            }
            self.baseline = Some(Baseline {
                capacity: memory.capacity_bytes,
                bounds: PressureBounds {
                    protected_bytes,
                    managed_bytes: (96 * GIB).min(memory.capacity_bytes - protected_bytes),
                },
                swap: sample.swap_used_bytes,
                previous_swap: sample.swap_used_bytes,
                stable_since: monotonic,
                previous_monotonic: monotonic,
                previous_sampled: memory.sampled_at_ms,
                previous_now: now,
                increases: 0,
                ready: false,
            });
            if memory.available_bytes < protected_bytes {
                return Err(PressureAbort::Headroom);
            }
            return Ok(PressureStatus::Warming);
        }
        let baseline = self.baseline.as_mut().expect("baseline initialized above");
        if memory.capacity_bytes != baseline.capacity
            || monotonic <= baseline.previous_monotonic
            || monotonic - baseline.previous_monotonic > MAX_AGE_MS
            || memory.sampled_at_ms <= baseline.previous_sampled
            || now < baseline.previous_now
        {
            return Err(PressureAbort::Observation);
        }
        if memory.available_bytes < baseline.bounds.protected_bytes {
            return Err(PressureAbort::Headroom);
        }
        if baseline.ready {
            baseline.increases = if sample.swap_used_bytes > baseline.previous_swap {
                baseline.increases + 1
            } else {
                0
            };
            if sample.swap_used_bytes - baseline.swap >= 64 << 20 || baseline.increases >= 3 {
                return Err(PressureAbort::SwapGrowth);
            }
        } else {
            if sample.swap_used_bytes != baseline.swap {
                baseline.swap = sample.swap_used_bytes;
                baseline.stable_since = monotonic;
            }
            baseline.ready = monotonic - baseline.stable_since >= BASELINE_MS;
        }
        baseline.previous_swap = sample.swap_used_bytes;
        baseline.previous_monotonic = monotonic;
        baseline.previous_sampled = memory.sampled_at_ms;
        baseline.previous_now = now;
        Ok(if baseline.ready {
            PressureStatus::Ready
        } else {
            PressureStatus::Warming
        })
    }
}

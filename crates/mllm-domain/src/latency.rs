//! SPEC §17: bounded latency distributions.
//!
//! One histogram type serves every tier that measures request latency: the
//! router, the host ingress and the engine histograms a host agent forwards.
//! A histogram is a fixed list of upper bucket bounds (seconds, ascending,
//! with an implicit `+Inf` bucket), a non-cumulative count per bucket, and the
//! exact sum and count. Memory is bounded by the bucket list, never by the
//! number of requests, and no request content is ever recorded.
//!
//! Percentiles are estimated the way Prometheus's `histogram_quantile` does:
//! linear interpolation inside the bucket that holds the rank. An estimate is
//! therefore only as fine as the bucket that holds it; exact per-request values
//! are reported separately (the router's optional timing header).

use serde::Serialize;

/// The most buckets one histogram may carry. Engine layouts observed so far
/// (vLLM 0.29.0, SGLang 0.5.20) use at most 40.
pub const MAX_BUCKETS: usize = 64;

/// The mllm-level bucket layout, in seconds: a 1, 1.5, 2, 3, 5, 7 progression
/// per decade from 100 µs to 1000 s. Router and ingress share it, so their
/// distributions compare bucket for bucket.
pub const MLLM_BOUNDS: &[f64] = &[
    0.0001, 0.00015, 0.0002, 0.0003, 0.0005, 0.0007, //
    0.001, 0.0015, 0.002, 0.003, 0.005, 0.007, //
    0.01, 0.015, 0.02, 0.03, 0.05, 0.07, //
    0.1, 0.15, 0.2, 0.3, 0.5, 0.7, //
    1.0, 1.5, 2.0, 3.0, 5.0, 7.0, //
    10.0, 15.0, 20.0, 30.0, 50.0, 70.0, //
    100.0, 150.0, 200.0, 300.0, 500.0, 700.0, //
    1000.0,
];

/// Why a histogram shape was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid latency histogram")]
pub struct InvalidHistogram;

/// A bounded latency histogram.
#[derive(Debug, Clone, PartialEq)]
pub struct Histogram {
    bounds: Vec<f64>,
    /// `bounds.len() + 1` entries; the last is the `+Inf` bucket.
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

fn valid_bounds(bounds: &[f64]) -> bool {
    !bounds.is_empty()
        && bounds.len() <= MAX_BUCKETS
        && bounds.iter().all(|b| b.is_finite() && *b >= 0.0)
        && bounds.windows(2).all(|w| w[0] < w[1])
}

impl Histogram {
    /// An empty histogram over `bounds`.
    pub fn new(bounds: &[f64]) -> Result<Self, InvalidHistogram> {
        if !valid_bounds(bounds) {
            return Err(InvalidHistogram);
        }
        Ok(Self {
            bounds: bounds.to_vec(),
            counts: vec![0; bounds.len() + 1],
            sum: 0.0,
            count: 0,
        })
    }

    /// An empty histogram over the mllm-level layout.
    pub fn mllm() -> Self {
        Self::new(MLLM_BOUNDS).expect("the mllm layout is valid")
    }

    /// A histogram from its parts, validated: bounds within [`MAX_BUCKETS`],
    /// finite, non-negative and strictly ascending; one count per bucket plus
    /// `+Inf`; the total equal to the bucket counts; a finite non-negative sum.
    pub fn from_parts(
        bounds: Vec<f64>,
        counts: Vec<u64>,
        sum: f64,
        count: u64,
    ) -> Result<Self, InvalidHistogram> {
        if !valid_bounds(&bounds)
            || counts.len() != bounds.len() + 1
            || !sum.is_finite()
            || sum < 0.0
            || counts
                .iter()
                .try_fold(0u64, |total, c| total.checked_add(*c))
                != Some(count)
        {
            return Err(InvalidHistogram);
        }
        Ok(Self {
            bounds,
            counts,
            sum,
            count,
        })
    }

    /// A histogram from a Prometheus exposition's cumulative `le` counts.
    /// `cumulative[i]` counts observations at most `bounds[i]`; `total` is the
    /// `+Inf` bucket (the `_count` series).
    pub fn from_cumulative(
        bounds: Vec<f64>,
        cumulative: &[u64],
        total: u64,
        sum: f64,
    ) -> Result<Self, InvalidHistogram> {
        if cumulative.len() != bounds.len() {
            return Err(InvalidHistogram);
        }
        let mut counts = Vec::with_capacity(bounds.len() + 1);
        let mut previous = 0u64;
        for c in cumulative.iter().copied().chain(std::iter::once(total)) {
            counts.push(c.checked_sub(previous).ok_or(InvalidHistogram)?);
            previous = c;
        }
        Self::from_parts(bounds, counts, sum, total)
    }

    pub fn bounds(&self) -> &[f64] {
        &self.bounds
    }
    pub fn counts(&self) -> &[u64] {
        &self.counts
    }
    pub fn sum(&self) -> f64 {
        self.sum
    }
    pub fn count(&self) -> u64 {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Record one observation in seconds. A negative or non-finite value is
    /// not a duration and is ignored.
    pub fn observe(&mut self, seconds: f64) {
        if !seconds.is_finite() || seconds < 0.0 {
            return;
        }
        let index = self.bounds.partition_point(|b| *b < seconds);
        self.counts[index] = self.counts[index].saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.sum += seconds;
    }

    /// Add `other` into this histogram. `false`, and nothing changed, when the
    /// layouts differ.
    pub fn merge(&mut self, other: &Histogram) -> bool {
        if self.bounds != other.bounds {
            return false;
        }
        for (mine, theirs) in self.counts.iter_mut().zip(&other.counts) {
            *mine = mine.saturating_add(*theirs);
        }
        self.count = self.count.saturating_add(other.count);
        self.sum += other.sum;
        true
    }

    /// What was observed since `earlier`, for a cumulative source. `None` when
    /// nothing was, and the whole of `self` when the source was reset (a count
    /// went down) or changed layout.
    pub fn since(&self, earlier: &Histogram) -> Option<Histogram> {
        if self.bounds != earlier.bounds
            || self
                .counts
                .iter()
                .zip(&earlier.counts)
                .any(|(now, before)| now < before)
        {
            return (!self.is_empty()).then(|| self.clone());
        }
        if self.count == earlier.count {
            return None;
        }
        let counts: Vec<u64> = self
            .counts
            .iter()
            .zip(&earlier.counts)
            .map(|(now, before)| now - before)
            .collect();
        Some(Histogram {
            bounds: self.bounds.clone(),
            count: counts.iter().sum(),
            counts,
            sum: (self.sum - earlier.sum).max(0.0),
        })
    }

    /// The estimated `q` quantile (0..=1) in seconds; `None` when empty. An
    /// estimate in the `+Inf` bucket reports the highest finite bound.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.count == 0 || !(0.0..=1.0).contains(&q) {
            return None;
        }
        let rank = q * self.count as f64;
        let mut below = 0u64;
        for (i, count) in self.counts.iter().enumerate() {
            let through = below + count;
            if (through as f64) >= rank && *count > 0 {
                let Some(upper) = self.bounds.get(i).copied() else {
                    return self.bounds.last().copied();
                };
                let lower = if i == 0 {
                    0.0f64.min(upper)
                } else {
                    self.bounds[i - 1]
                };
                let within = (rank - below as f64) / *count as f64;
                return Some(lower + (upper - lower) * within.clamp(0.0, 1.0));
            }
            below = through;
        }
        self.bounds.last().copied()
    }

    /// The count, sum, mean and p50/p95/p99 estimates.
    pub fn summary(&self) -> Summary {
        Summary {
            count: self.count,
            sum_seconds: self.sum,
            mean_seconds: (self.count > 0).then(|| self.sum / self.count as f64),
            p50_seconds: self.quantile(0.50),
            p95_seconds: self.quantile(0.95),
            p99_seconds: self.quantile(0.99),
        }
    }

    /// The buckets as `(upper bound, non-cumulative count)`, `None` for `+Inf`.
    pub fn buckets(&self) -> Vec<Bucket> {
        self.counts
            .iter()
            .enumerate()
            .map(|(i, count)| Bucket {
                le: self.bounds.get(i).copied(),
                count: *count,
            })
            .collect()
    }
}

/// One bucket of a histogram as reported: its upper bound (`None` is `+Inf`)
/// and the observations in it alone.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Bucket {
    pub le: Option<f64>,
    pub count: u64,
}

/// A histogram's reported summary.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Summary {
    pub count: u64,
    pub sum_seconds: f64,
    pub mean_seconds: Option<f64>,
    pub p50_seconds: Option<f64>,
    pub p95_seconds: Option<f64>,
    pub p99_seconds: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // SPEC §17: distributions, not a single number; bounded by layout.
    #[test]
    fn quantiles_interpolate_within_the_holding_bucket() {
        let mut h = Histogram::new(&[0.1, 0.2, 0.4]).unwrap();
        for _ in 0..50 {
            h.observe(0.05);
        }
        for _ in 0..50 {
            h.observe(0.15);
        }
        assert_eq!(h.count(), 100);
        assert_eq!(h.counts(), &[50, 50, 0, 0]);
        let p50 = h.quantile(0.5).unwrap();
        assert!((p50 - 0.1).abs() < 1e-9, "{p50}");
        let p99 = h.quantile(0.99).unwrap();
        assert!(p99 > 0.19 && p99 <= 0.2, "{p99}");
        h.observe(9.0);
        assert_eq!(h.quantile(1.0), Some(0.4), "+Inf reports the top bound");
        assert!(Histogram::mllm().summary().p50_seconds.is_none());
    }

    #[test]
    fn cumulative_sources_yield_deltas_and_survive_resets() {
        let bounds = vec![0.5, 1.0];
        let a = Histogram::from_cumulative(bounds.clone(), &[1, 3], 4, 2.5).unwrap();
        assert_eq!(a.counts(), &[1, 2, 1]);
        let b = Histogram::from_cumulative(bounds.clone(), &[2, 5], 7, 4.0).unwrap();
        let d = b.since(&a).unwrap();
        assert_eq!((d.counts(), d.count()), (&[1u64, 1, 1][..], 3));
        assert!(b.since(&b).is_none());
        // A restarted source reports everything it holds now.
        let reset = Histogram::from_cumulative(bounds.clone(), &[0, 1], 1, 0.7).unwrap();
        assert_eq!(reset.since(&b).unwrap().count(), 1);
        // Inconsistent shapes are refused.
        assert!(Histogram::from_cumulative(bounds.clone(), &[3, 1], 4, 1.0).is_err());
        assert!(Histogram::from_parts(vec![1.0, 0.5], vec![0, 0, 0], 0.0, 0).is_err());
        assert!(Histogram::from_parts(bounds, vec![1, 0, 0], 0.1, 2).is_err());
        let mut m = Histogram::mllm();
        assert!(!m.merge(&a));
    }
}

//! The coordinator's observation source, backed by the host's own memory readings.
//!
//! Every existing implementation of this trait is a test double, so production had
//! nothing to give the coordinator. This is the adapter that closes that, and it
//! lives in the composition root because it is the only place that sees both the
//! coordinator and the host agent.
//!
//! Observation is a read, never a guess. A reading that cannot be taken is reported
//! as a failure rather than as an absent or stale figure: a coordinator that treats
//! an unavailable reading as "no memory in use" would admit work the host cannot
//! hold, which is the specific failure aggregate accounting exists to prevent.

use std::sync::Arc;

use mllm_controller::coordinator::{CoordinatorError, ObservationFuture, ServiceObservation};

/// Reads the host's memory through the agent's `/proc/meminfo` parser.
pub struct HostMemoryObservation;

impl ServiceObservation for HostMemoryObservation {
    fn observe(&self, _host_id: String) -> ObservationFuture {
        Box::pin(async move {
            // Synchronous and short: a `/proc` read, not a syscall that blocks.
            let sample = mllm_agent::memory::read_host_memory().map_err(|error| {
                CoordinatorError::Service(format!("host memory observation failed: {error}"))
            })?;
            Ok(vec![sample.memory])
        })
    }
}

/// The coordinator's clock.
///
/// A clock that cannot answer fails the call rather than substituting a default.
/// Every durable fence the coordinator writes is stamped with this, so a zero or a
/// stale reading would date evidence to a time it was not observed at.
pub fn system_clock() -> Arc<dyn Fn() -> Result<i64, CoordinatorError> + Send + Sync> {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
            .ok_or_else(|| CoordinatorError::Service("system clock is unreadable".into()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clock must produce a plausible wall-clock millisecond value, not a
    /// counter or a zero. Evidence stamped with either would misdate a fence.
    #[test]
    fn the_clock_reads_wall_clock_milliseconds() {
        let clock = system_clock();
        let now = clock().expect("a readable system clock");
        // After 2020 and before 2100: wide enough not to be brittle, narrow enough
        // to catch a counter or a seconds/millis mix-up.
        assert!(now > 1_577_836_800_000, "too early to be wall clock: {now}");
        assert!(now < 4_102_444_800_000, "too late to be wall clock: {now}");
    }

    #[tokio::test]
    async fn an_observation_reports_a_real_domain_with_capacity() {
        let observed = HostMemoryObservation
            .observe("host".into())
            .await
            .expect("this host can read its own memory");
        assert_eq!(observed.len(), 1, "one domain is observed");
        let memory = &observed[0];
        assert!(memory.capacity_bytes > 0, "capacity must be real");
        assert!(
            memory.available_bytes <= memory.capacity_bytes,
            "available cannot exceed capacity"
        );
        assert!(memory.sampled_at_ms > 0, "the sample must be dated");
    }
}

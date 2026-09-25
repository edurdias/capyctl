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

/// Reads the host's memory through the agent's `/proc/meminfo` parser and reports
/// it under the domains the host published.
///
/// The agent reads one physical pool and labels it `system`, which is a fact about
/// where the reading came from. A domain is the host's own accounting unit, named
/// by its resource policy, and the coordinator admits work per domain: an
/// observation naming a domain the policy does not declare is dropped as invalid.
/// So the translation belongs here, at the one adapter that sees both.
pub struct HostMemoryObservation {
    domains: Vec<String>,
    reader: MemoryReader,
    /// ADR 0007: per-process resident memory sampled beside the reading.
    /// `None` reports none (credits nothing).
    residency: Option<Arc<mllm_agent::process_residency::ResidencySampler>>,
}

/// Where standalone reads its host's memory from: `/proc/meminfo` in
/// production. A test states an explicit capacity instead, so its fixtures
/// and the standalone policy derived from them do not depend on how much
/// memory the machine running the suite happens to have free.
pub type MemoryReader =
    Arc<dyn Fn() -> Result<mllm_domain::resources::MemoryObservation, String> + Send + Sync>;

/// The production reader: the agent's `/proc/meminfo` parser.
pub fn proc_meminfo() -> MemoryReader {
    Arc::new(|| {
        mllm_agent::memory::read_host_memory()
            .map(|sample| sample.memory)
            .map_err(|error| error.to_string())
    })
}

/// A fixed reading: `capacity_bytes` total with `available_bytes` free, dated
/// when read.
pub fn fixed_memory(capacity_bytes: i64, available_bytes: i64) -> MemoryReader {
    Arc::new(move || {
        let sampled_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis();
        Ok(mllm_domain::resources::MemoryObservation {
            domain: "system".into(),
            capacity_bytes,
            available_bytes,
            sampled_at_ms: i64::try_from(sampled_at_ms).map_err(|error| error.to_string())?,
        })
    })
}

impl HostMemoryObservation {
    /// `domains` are the host policy's own domain names, in the order it declares
    /// them. A host that declares none observes nothing; inventing a domain would
    /// admit work against a ceiling that was never published.
    pub fn new(domains: impl IntoIterator<Item = String>) -> Self {
        Self::with_reader(domains, proc_meminfo())
    }

    /// As [`Self::new`], reading memory through `reader`.
    pub fn with_reader(domains: impl IntoIterator<Item = String>, reader: MemoryReader) -> Self {
        Self {
            domains: domains.into_iter().collect(),
            reader,
            residency: None,
        }
    }

    /// Report each GPU process's resident memory with every reading (ADR
    /// 0007), so admission credits the engines already resident here.
    pub fn with_process_residency(
        mut self,
        sampler: Arc<mllm_agent::process_residency::ResidencySampler>,
    ) -> Self {
        self.residency = Some(sampler);
        self
    }
}

impl ServiceObservation for HostMemoryObservation {
    fn observe_with_residents(
        &self,
        host_id: String,
    ) -> mllm_controller::coordinator::ResidentObservationFuture {
        let observed = self.observe(host_id);
        let residency = self.residency.clone();
        Box::pin(async move {
            // Availability first, then the processes still alive (ADR 0007).
            let observed = observed.await?;
            let residents = residency
                .map(|sampler| sampler.current())
                .unwrap_or_default();
            Ok((observed, residents))
        })
    }

    fn observe(&self, _host_id: String) -> ObservationFuture {
        let domains = self.domains.clone();
        let reader = self.reader.clone();
        Box::pin(async move {
            // Synchronous and short: a `/proc` read, not a syscall that blocks.
            let memory = reader().map_err(|error| {
                CoordinatorError::Service(format!("host memory observation failed: {error}"))
            })?;
            // One reading, reported once per domain. This host's domains share a
            // single physical pool, which is what `unified` in its policy means.
            Ok(domains
                .into_iter()
                .map(|domain| mllm_domain::resources::MemoryObservation {
                    domain,
                    ..memory.clone()
                })
                .collect())
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
        let observed = HostMemoryObservation::new(["unified".to_string()])
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

    /// The reading is reported against the domains the host declared, not the
    /// name `/proc/meminfo` is read under. A coordinator admits work per domain
    /// and drops an observation naming one it does not know, so an adapter that
    /// reports the agent's own label stalls every activation on this host.
    #[tokio::test]
    async fn observations_are_named_by_the_hosts_declared_domains() {
        let declared = ["unified".to_string(), "second".to_string()];
        let observed = HostMemoryObservation::new(declared.clone())
            .observe("host".into())
            .await
            .expect("this host can read its own memory");
        let names: Vec<_> = observed.iter().map(|o| o.domain.clone()).collect();
        assert_eq!(
            names, declared,
            "every declared domain is observed, in order"
        );
        assert!(
            observed.iter().all(|o| o.capacity_bytes > 0),
            "each carries the same real reading"
        );
    }

    /// A host that declared no domain has nothing to admit against. Reporting an
    /// invented one would let work in against a ceiling nobody published.
    #[tokio::test]
    async fn a_host_with_no_declared_domain_observes_nothing() {
        let observed = HostMemoryObservation::new([])
            .observe("host".into())
            .await
            .expect("this host can read its own memory");
        assert!(observed.is_empty(), "no domain declared, none observed");
    }
}

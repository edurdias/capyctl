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

use mllm_agent::gpu_memory::{GpuSample, GpuSampler};
use mllm_agent::memory::HostMemorySample;
use mllm_controller::coordinator::{CoordinatorError, ObservationFuture, ServiceObservation};
use mllm_domain::resources::MemoryObservation;

/// Reads the host's memory through the agent's `/proc/meminfo` parser, and a
/// discrete GPU's memory through the GPU collector, and reports each under the
/// domain the host published for it.
///
/// The agent reads one physical pool and labels it `system`, which is a fact about
/// where the reading came from. A domain is the host's own accounting unit, named
/// by its resource policy, and the coordinator admits work per domain: an
/// observation naming a domain the policy does not declare is dropped as invalid.
/// So the translation belongs here, at the one adapter that sees both.
pub struct HostMemoryObservation {
    domains: Vec<ObservedDomain>,
    reader: MemoryReader,
    /// SPEC §7.2 / ADR 0019: where a device domain's memory is read from.
    /// `None` observes no device, so every device domain stays unobserved.
    gpu: Option<Arc<GpuSampler>>,
    /// ADR 0007: per-process resident memory sampled beside the reading.
    /// `None` reports none (credits nothing).
    residency: Option<Arc<mllm_agent::process_residency::ResidencySampler>>,
}

/// One published domain and the source its memory is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedDomain {
    /// Host memory (`/proc/meminfo`): a `system`, `distinct` or `unified` domain.
    Host(String),
    /// ADR 0019: the VRAM of the GPU at nvidia-smi `index`.
    Device { domain: String, index: u32 },
}

/// The observation for each of `domains`: host memory from `host`, device
/// memory from `gpu`.
///
/// SPEC §7.2: a device absent from `gpu` (no sample, a device the sample does
/// not list, or one without memory of its own) yields no observation for its
/// domain. The coordinator treats a missing observation as unknown and closes
/// admission there; host RAM is never substituted for VRAM.
pub fn observe_domains(
    domains: &[ObservedDomain],
    host: &HostMemorySample,
    gpu: Option<&GpuSample>,
) -> Vec<MemoryObservation> {
    domains
        .iter()
        .filter_map(|observed| match observed {
            ObservedDomain::Host(domain) => Some(MemoryObservation {
                domain: domain.clone(),
                ..host.memory.clone()
            }),
            // SPEC §7.2: VRAM is read from the device, never substituted with RAM.
            ObservedDomain::Device { domain, index } => {
                let sample = gpu?;
                let memory = sample
                    .devices
                    .iter()
                    .find(|device| device.index == *index)?
                    .memory
                    .as_ref()?;
                Some(MemoryObservation {
                    domain: domain.clone(),
                    capacity_bytes: memory.total_bytes,
                    available_bytes: memory.free_bytes,
                    sampled_at_ms: sample.sampled_at_ms,
                })
            }
        })
        .collect()
}

/// The observed domains of a published policy, in its order: a `device`
/// domain is read from the GPU its `gpuN` device id names (the nvidia-smi
/// index standalone publishes it under), every other domain from host memory.
///
/// A device domain whose device id is not `gpuN` has no source, so it is left
/// out and stays unobserved: admission closes on it rather than guessing.
pub fn observed_domains(
    domains: &std::collections::BTreeMap<String, mllm_config::effective::DomainPolicy>,
) -> Vec<ObservedDomain> {
    domains
        .iter()
        .filter_map(|(name, policy)| match policy.memory {
            mllm_config::effective::DomainMemory::Device => {
                let index = policy
                    .device
                    .as_deref()
                    .and_then(mllm_agent::gpu_memory::device_index)?;
                Some(ObservedDomain::Device {
                    domain: name.clone(),
                    index,
                })
            }
            _ => Some(ObservedDomain::Host(name.clone())),
        })
        .collect()
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
        Self::with_domains(domains.into_iter().map(ObservedDomain::Host).collect())
            .with_memory_reader(reader)
    }

    /// Observe `domains`, each from its own source (SPEC §7.2): host memory
    /// through `/proc/meminfo`, a device domain through the GPU sampler
    /// ([`Self::with_gpu_sampler`]); without one, device domains stay
    /// unobserved.
    pub fn with_domains(domains: Vec<ObservedDomain>) -> Self {
        Self {
            domains,
            reader: proc_meminfo(),
            gpu: None,
            residency: None,
        }
    }

    /// Read host memory through `reader` instead of `/proc/meminfo`.
    pub fn with_memory_reader(mut self, reader: MemoryReader) -> Self {
        self.reader = reader;
        self
    }

    /// Read device domains through `sampler` (ADR 0019). It runs only when a
    /// device domain is declared, so a unified host never runs the collector.
    pub fn with_gpu_sampler(mut self, sampler: Arc<GpuSampler>) -> Self {
        self.gpu = Some(sampler);
        self
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
            // A sample taken for this observation: admission asks rarely, so
            // a cached one would be past its age bound (found live on a 16 GB
            // discrete GPU). The collector is a bounded process, so it runs
            // off the async thread.
            let observed = observed.await?;
            let residents = match residency {
                Some(sampler) => tokio::task::spawn_blocking(move || sampler.sample_fresh())
                    .await
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            Ok((observed, residents))
        })
    }

    fn observe(&self, _host_id: String) -> ObservationFuture {
        let domains = self.domains.clone();
        let reader = self.reader.clone();
        let gpu = self.gpu.clone().filter(|_| {
            domains
                .iter()
                .any(|d| matches!(d, ObservedDomain::Device { .. }))
        });
        Box::pin(async move {
            // Synchronous and short: a `/proc` read, not a syscall that blocks.
            let memory = reader().map_err(|error| {
                CoordinatorError::Service(format!("host memory observation failed: {error}"))
            })?;
            // Only the memory figure is reported; swap is not an observation.
            let host = HostMemorySample {
                memory,
                swap_used_bytes: 0,
            };
            // The collector spawns a bounded process, so it runs off the async
            // thread. A failed or panicked sample is no sample: the device
            // domains go unobserved (SPEC §7.2), host domains are unaffected.
            let sample = match gpu {
                Some(sampler) => tokio::task::spawn_blocking(move || sampler())
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            // A host domain reports the one host reading; on a unified host its
            // single domain is that whole physical pool.
            Ok(observe_domains(&domains, &host, sample.as_ref()))
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

    fn host_sample(
        capacity_bytes: i64,
        available_bytes: i64,
        sampled_at_ms: i64,
    ) -> HostMemorySample {
        HostMemorySample {
            memory: MemoryObservation {
                domain: "x".into(),
                capacity_bytes,
                available_bytes,
                sampled_at_ms,
            },
            swap_used_bytes: 0,
        }
    }

    // T26 T27 (ADR 0007): found live on the 16 GB discrete-GPU laptop host.
    // Admission asks for residents once per attempt (tens of seconds apart),
    // so a cached sample was always older than its 5 s bound and none was
    // reported: a parked model's pinned copy went uncredited and a start
    // beside it neither fitted nor reclaimed it. Each observation now carries
    // a sample taken for it.
    #[tokio::test]
    async fn every_observation_carries_a_fresh_resident_sample() {
        let me = std::process::id();
        let sampler = mllm_agent::process_residency::ResidencySampler::with_collector(
            std::sync::Arc::new(move || Some(vec![(me, 1 << 20)])),
        );
        let observation =
            HostMemoryObservation::new(["unified".to_string()]).with_process_residency(sampler);
        let (_, residents) = observation
            .observe_with_residents("host".into())
            .await
            .expect("this host can read its own memory");
        assert_eq!(residents.len(), 1, "the first observation already has one");
        assert_eq!(residents[0].pid, me);
    }

    const DISCRETE_ROW: &str =
        "0, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, RTX, 16376, 1536, 14840\n";

    // T26: host RAM for the system domain, VRAM for the device domain.
    #[test]
    fn a_device_domain_is_observed_from_the_gpu() {
        let host = host_sample(61 << 30, 50 << 30, 10);
        let gpu = mllm_agent::gpu_memory::parse_query_gpu(DISCRETE_ROW, 11).unwrap();
        let domains = [
            ObservedDomain::Host("system".into()),
            ObservedDomain::Device {
                domain: "gpu0".into(),
                index: 0,
            },
        ];
        let observed = observe_domains(&domains, &host, Some(&gpu));
        assert_eq!(observed[0].domain, "system");
        assert_eq!(observed[0].capacity_bytes, 61 << 30);
        assert_eq!(observed[1].domain, "gpu0");
        assert_eq!(observed[1].capacity_bytes, 16376 << 20);
        assert_eq!(observed[1].available_bytes, 14840 << 20);
        assert_eq!(observed[1].sampled_at_ms, 11);
    }

    // T29: a GPU sample that failed reports nothing for the device domain.
    #[test]
    fn an_unobserved_device_has_no_observation() {
        let host = host_sample(1, 1, 1);
        let domains = [
            ObservedDomain::Host("system".into()),
            ObservedDomain::Device {
                domain: "gpu0".into(),
                index: 0,
            },
        ];
        let observed = observe_domains(&domains, &host, None);
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].domain, "system");
    }

    // T29: a sample that lacks the device, or reports it without memory of
    // its own, observes nothing for its domain; host RAM never stands in.
    #[test]
    fn a_device_missing_from_the_sample_has_no_observation() {
        let host = host_sample(1 << 30, 1 << 30, 1);
        let domains = [ObservedDomain::Device {
            domain: "gpu1".into(),
            index: 1,
        }];
        let gpu = mllm_agent::gpu_memory::parse_query_gpu(DISCRETE_ROW, 2).unwrap();
        assert!(observe_domains(&domains, &host, Some(&gpu)).is_empty());
        let integrated = mllm_agent::gpu_memory::parse_query_gpu(
            "1, GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee, 0000000F:01:00.0, GB10, [N/A], [N/A], [N/A]\n",
            2,
        )
        .unwrap();
        assert!(observe_domains(&domains, &host, Some(&integrated)).is_empty());
    }

    /// The domain list follows the published policy: a device domain is read
    /// from its `gpuN` device, every other domain from host memory.
    // T26
    #[test]
    fn observed_domains_follow_the_policy_shape() {
        use mllm_config::effective::{DomainMemory, DomainPolicy};
        let domain = |memory, device: Option<&str>| DomainPolicy {
            managed_limit: 1,
            free_reserve: 0,
            host_kv_limit: None,
            parked_limit: None,
            memory,
            device: device.map(str::to_owned),
        };
        let mut domains = std::collections::BTreeMap::new();
        domains.insert("system".to_string(), domain(DomainMemory::Distinct, None));
        domains.insert(
            "gpu0".to_string(),
            domain(DomainMemory::Device, Some("gpu0")),
        );
        domains.insert(
            "gpu3".to_string(),
            domain(DomainMemory::Device, Some("gpu3")),
        );
        assert_eq!(
            observed_domains(&domains),
            vec![
                ObservedDomain::Device {
                    domain: "gpu0".into(),
                    index: 0
                },
                ObservedDomain::Device {
                    domain: "gpu3".into(),
                    index: 3
                },
                ObservedDomain::Host("system".into()),
            ]
        );
    }

    /// The adapter samples the GPU only when a device domain is declared, and
    /// reports each domain from its own source.
    // T26 T29
    #[tokio::test]
    async fn the_adapter_reads_device_domains_through_its_sampler() {
        let sampled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sampler = |present: bool| -> Arc<GpuSampler> {
            let sampled = sampled.clone();
            Arc::new(move || {
                sampled.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                present.then(|| mllm_agent::gpu_memory::parse_query_gpu(DISCRETE_ROW, 5).unwrap())
            })
        };
        let discrete = vec![
            ObservedDomain::Host("system".into()),
            ObservedDomain::Device {
                domain: "gpu0".into(),
                index: 0,
            },
        ];
        let observed = HostMemoryObservation::with_domains(discrete.clone())
            .with_memory_reader(fixed_memory(8 << 30, 4 << 30))
            .with_gpu_sampler(sampler(true))
            .observe("host".into())
            .await
            .unwrap();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0].capacity_bytes, 8 << 30);
        assert_eq!(observed[1].capacity_bytes, 16376 << 20);
        let observed = HostMemoryObservation::with_domains(discrete)
            .with_memory_reader(fixed_memory(8 << 30, 4 << 30))
            .with_gpu_sampler(sampler(false))
            .observe("host".into())
            .await
            .unwrap();
        assert_eq!(observed.len(), 1, "the device domain is unobserved");
        assert_eq!(sampled.load(std::sync::atomic::Ordering::SeqCst), 2);
        HostMemoryObservation::with_reader(["unified".to_string()], fixed_memory(8, 4))
            .with_gpu_sampler(sampler(true))
            .observe("host".into())
            .await
            .unwrap();
        assert_eq!(
            sampled.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a unified host never runs the GPU collector"
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

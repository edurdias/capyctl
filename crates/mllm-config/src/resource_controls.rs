use crate::effective::{DomainPolicy, HostPolicy, PortRange, QueuePolicy, Sharing};
use crate::{ConfigError, ConfigErrorCode};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceControls {
    pub domains: BTreeMap<String, DomainPolicy>,
    pub max_parked: u32,
    pub observation_ttl_ms: i64,
    pub planner_max_states: u32,
    pub queue: QueuePolicy,
    pub device_sharing: Sharing,
    pub device_sharing_overrides: BTreeMap<String, Sharing>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceContext {
    pub host_id: String,
    pub domain_ids: BTreeSet<String>,
    pub device_domains: BTreeMap<String, String>,
    pub endpoint_port_range: PortRange,
}

fn invalid(detail: &'static str) -> ConfigError {
    ConfigError::new(
        ConfigErrorCode::UnsupportedCombination,
        "resource_policy",
        detail,
    )
}

impl ResourceControls {
    pub fn from_host(host: &HostPolicy) -> Self {
        Self {
            domains: host.domains.clone(),
            max_parked: host.max_parked,
            observation_ttl_ms: host.observation_ttl_ms,
            planner_max_states: host.planner_max_states,
            queue: host.queue.clone(),
            device_sharing: host.device_sharing,
            device_sharing_overrides: host
                .devices
                .iter()
                .map(|(id, policy)| (id.clone(), policy.sharing))
                .collect(),
        }
    }

    pub fn validate(&self, context: &ResourceContext) -> Result<(), ConfigError> {
        if context.host_id.is_empty()
            || context.domain_ids.is_empty()
            || context.domain_ids.iter().any(String::is_empty)
            || context.device_domains.iter().any(|(device, domain)| {
                device.is_empty() || domain.is_empty() || !context.domain_ids.contains(domain)
            })
            || context.endpoint_port_range.start == 0
            || context.endpoint_port_range.start > context.endpoint_port_range.end
        {
            return Err(invalid("invalid immutable resource context"));
        }
        if self.domains.keys().collect::<BTreeSet<_>>()
            != context.domain_ids.iter().collect::<BTreeSet<_>>()
            || self
                .device_sharing_overrides
                .keys()
                .collect::<BTreeSet<_>>()
                != context.device_domains.keys().collect::<BTreeSet<_>>()
        {
            return Err(invalid("resource controls do not match immutable context"));
        }
        if self.domains.values().any(|domain| {
            domain.managed_limit <= 0
                || domain.free_reserve < 0
                || domain.host_kv_limit.is_some_and(|value| value < 0)
                || domain.parked_limit.is_some_and(|value| value < 0)
        }) || self.device_sharing == Sharing::Exclusive
            && self
                .device_sharing_overrides
                .values()
                .any(|sharing| *sharing == Sharing::Shared)
            || self.max_parked > 16
            || !(1..=10_000).contains(&self.observation_ttl_ms)
            || !(1..=65_536).contains(&self.planner_max_states)
            || !(1..=4_096).contains(&self.queue.max_pending_per_deployment)
            || !(1..=16_384).contains(&self.queue.max_pending_total)
            || self.queue.max_pending_per_deployment > self.queue.max_pending_total
            || !(1..=(1_i64 << 30)).contains(&self.queue.max_buffered_bytes_total)
            || !(1..=3_600_000).contains(&self.queue.request_deadline_ms)
            || !(1..=30_000).contains(&self.queue.admission_window_ms)
            || self.queue.admission_window_ms > self.queue.request_deadline_ms
        {
            return Err(invalid("invalid bounded resource controls"));
        }
        Ok(())
    }
}

impl ResourceContext {
    pub fn from_host(host: &HostPolicy) -> Self {
        Self {
            host_id: host.name.clone(),
            domain_ids: host.domains.keys().cloned().collect(),
            device_domains: host
                .devices
                .iter()
                .map(|(id, policy)| (id.clone(), policy.domain.clone()))
                .collect(),
            endpoint_port_range: host.endpoint_port_range.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effective::{DevicePolicy, DomainPolicy, PortRange, QueuePolicy, Sharing};
    use std::collections::{BTreeMap, BTreeSet};

    fn host() -> crate::effective::HostPolicy {
        let (context, controls) = fixture();
        crate::effective::HostPolicy {
            name: context.host_id,
            hardware_fingerprint: "hardware-secret-owner".into(),
            environment_fingerprint: "environment-secret-owner".into(),
            domains: controls.domains,
            devices: BTreeMap::from([(
                "gpu0".into(),
                DevicePolicy {
                    domain: "system".into(),
                    sharing: Sharing::Exclusive,
                },
            )]),
            max_parked: controls.max_parked,
            observation_ttl_ms: controls.observation_ttl_ms,
            device_sharing: controls.device_sharing,
            endpoint_port_range: context.endpoint_port_range,
            planner_max_states: controls.planner_max_states,
            queue: controls.queue,
            qualification_policy: None,
        }
    }

    fn fixture() -> (ResourceContext, ResourceControls) {
        let context = ResourceContext {
            host_id: "host-a".into(),
            domain_ids: BTreeSet::from(["system".into()]),
            device_domains: BTreeMap::from([("gpu0".into(), "system".into())]),
            endpoint_port_range: PortRange {
                start: 20_000,
                end: 20_100,
            },
        };
        let controls = ResourceControls {
            domains: BTreeMap::from([(
                "system".into(),
                DomainPolicy {
                    managed_limit: 64 << 30,
                    free_reserve: 16 << 30,
                    host_kv_limit: Some(8 << 30),
                    parked_limit: Some(32 << 30),
                },
            )]),
            max_parked: 16,
            observation_ttl_ms: 10_000,
            planner_max_states: 65_536,
            queue: QueuePolicy {
                max_pending_per_deployment: 4_096,
                max_pending_total: 16_384,
                max_buffered_bytes_total: 1 << 30,
                request_deadline_ms: 3_600_000,
                admission_window_ms: 30_000,
            },
            device_sharing: Sharing::Shared,
            device_sharing_overrides: BTreeMap::from([("gpu0".into(), Sharing::Exclusive)]),
        };
        (context, controls)
    }

    #[test]
    fn structural_validation_accepts_exact_maxima_and_zero_optional_limits() {
        let (context, mut controls) = fixture();
        assert!(controls.validate(&context).is_ok());
        controls.domains.get_mut("system").unwrap().free_reserve = 0;
        controls.domains.get_mut("system").unwrap().host_kv_limit = Some(0);
        controls.domains.get_mut("system").unwrap().parked_limit = Some(0);
        assert!(controls.validate(&context).is_ok());
    }

    #[test]
    fn structural_validation_rejects_one_over_and_zero_positive_bounds() {
        let (context, controls) = fixture();
        let mut cases = Vec::new();
        let mut value = controls.clone();
        value.max_parked = 17;
        cases.push(value);
        let mut value = controls.clone();
        value.observation_ttl_ms = 10_001;
        cases.push(value);
        let mut value = controls.clone();
        value.planner_max_states = 65_537;
        cases.push(value);
        let mut value = controls.clone();
        value.queue.max_pending_per_deployment = 4_097;
        cases.push(value);
        let mut value = controls.clone();
        value.queue.max_pending_total = 16_385;
        cases.push(value);
        let mut value = controls.clone();
        value.queue.max_buffered_bytes_total = (1 << 30) + 1;
        cases.push(value);
        let mut value = controls.clone();
        value.queue.request_deadline_ms = 3_600_001;
        cases.push(value);
        let mut value = controls.clone();
        value.queue.admission_window_ms = 30_001;
        cases.push(value);
        for value in cases {
            assert!(value.validate(&context).is_err());
        }
        let mut value = controls.clone();
        value.domains.get_mut("system").unwrap().managed_limit = 0;
        assert!(value.validate(&context).is_err());
        let mut value = controls.clone();
        value.observation_ttl_ms = 0;
        assert!(value.validate(&context).is_err());
        let mut value = controls.clone();
        value.planner_max_states = 0;
        assert!(value.validate(&context).is_err());
        let mut value = controls;
        value.queue.max_pending_total = 0;
        assert!(value.validate(&context).is_err());
    }

    #[test]
    fn structural_validation_rejects_negative_categories_and_queue_inversions() {
        let (context, controls) = fixture();
        let mut value = controls.clone();
        value.domains.get_mut("system").unwrap().free_reserve = -1;
        assert!(value.validate(&context).is_err());
        let mut value = controls.clone();
        value.domains.get_mut("system").unwrap().host_kv_limit = Some(-1);
        assert!(value.validate(&context).is_err());
        let mut value = controls.clone();
        value.domains.get_mut("system").unwrap().parked_limit = Some(-1);
        assert!(value.validate(&context).is_err());
        let mut value = controls.clone();
        value.queue.max_pending_per_deployment = value.queue.max_pending_total + 1;
        assert!(value.validate(&context).is_err());
        let mut value = controls;
        value.queue.admission_window_ms = value.queue.request_deadline_ms + 1;
        assert!(value.validate(&context).is_err());
    }

    #[test]
    fn structural_validation_rejects_membership_mapping_endpoint_and_sharing_errors() {
        let (context, controls) = fixture();
        let mut bad_context = context.clone();
        bad_context.host_id.clear();
        assert!(controls.validate(&bad_context).is_err());
        let mut bad_context = context.clone();
        bad_context.domain_ids.insert("extra".into());
        assert!(controls.validate(&bad_context).is_err());
        let mut bad_context = context.clone();
        bad_context
            .device_domains
            .insert("gpu1".into(), "missing".into());
        assert!(controls.validate(&bad_context).is_err());
        let mut bad_context = context.clone();
        bad_context.endpoint_port_range.start = 0;
        assert!(controls.validate(&bad_context).is_err());
        let mut value = controls.clone();
        value.device_sharing_overrides.clear();
        assert!(value.validate(&context).is_err());
        let mut value = controls.clone();
        value
            .domains
            .insert("extra".into(), value.domains["system"].clone());
        assert!(value.validate(&context).is_err());
        let mut value = controls.clone();
        value
            .device_sharing_overrides
            .insert("gpu1".into(), Sharing::Exclusive);
        assert!(value.validate(&context).is_err());
        let mut value = controls;
        value.device_sharing = Sharing::Exclusive;
        value
            .device_sharing_overrides
            .insert("gpu0".into(), Sharing::Shared);
        assert!(value.validate(&context).is_err());
    }

    #[test]
    fn host_projection_preserves_every_owned_field_and_isolates_other_authority() {
        let host = host();
        let context = ResourceContext::from_host(&host);
        let controls = ResourceControls::from_host(&host);
        assert_eq!(context.host_id, "host-a");
        assert_eq!(context.domain_ids, BTreeSet::from(["system".into()]));
        assert_eq!(
            context.device_domains,
            BTreeMap::from([("gpu0".into(), "system".into())])
        );
        assert_eq!(controls.domains["system"].managed_limit, 64 << 30);
        assert_eq!(
            controls.device_sharing_overrides["gpu0"],
            Sharing::Exclusive
        );
        assert!(controls.validate(&context).is_ok());
        let encoded = serde_json::to_string(&controls).unwrap();
        assert!(!encoded.contains("hardware-secret-owner"));
        assert!(!encoded.contains("environment-secret-owner"));
        assert!(!encoded.contains("qualification"));
    }
}

//! ADR 0028 §3: a host's multi-node group policy, the `resource_policy.groups`
//! block: the address its peers reach it on, the rendezvous port range it
//! hands a group, and whether a group on it requires RDMA. Stated three ways
//! (YAML, flag, environment) through [`crate::engine_settings`]; this module
//! owns the strict parse and the value rules.
use std::net::IpAddr;
use std::ops::RangeInclusive;

use serde_json::Value;

use crate::{ConfigError, ConfigErrorCode};

/// ADR 0028 §3: the default rendezvous port range.
pub const DEFAULT_RENDEZVOUS_PORTS: RangeInclusive<u16> = 25000..=25099;

const GROUPS: &str = "resource_policy.groups";

/// ADR 0028 §3: the resolved group policy of one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupsPolicy {
    pub peer_address: Option<IpAddr>,
    pub rendezvous_ports: RangeInclusive<u16>,
    pub require_rdma: bool,
}

impl Default for GroupsPolicy {
    fn default() -> Self {
        Self {
            peer_address: None,
            rendezvous_ports: DEFAULT_RENDEZVOUS_PORTS,
            require_rdma: false,
        }
    }
}

/// The settings a layer states; `None` is "not stated".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatedGroups {
    pub peer_address: Option<IpAddr>,
    pub rendezvous_ports: Option<(u16, u16)>,
    pub require_rdma: Option<bool>,
}

fn refuse(path: &str, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

/// ADR 0028 §3: a peer address is a unicast address a peer can reach: not
/// loopback, not unspecified, not multicast.
pub fn peer_address(name: &str, text: &str) -> Result<IpAddr, ConfigError> {
    let address: IpAddr = text.parse().map_err(|_| {
        refuse(
            name,
            format!("must be an IP address such as 192.0.2.10; got {text:?}"),
        )
    })?;
    if address.is_loopback() || address.is_unspecified() || address.is_multicast() {
        return Err(refuse(
            name,
            format!("must be a unicast address a peer host can reach; got {address}"),
        ));
    }
    Ok(address)
}

fn number(value: &Value) -> Option<u16> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
    .and_then(|port| u16::try_from(port).ok())
}

/// The settings `host`'s `resource_policy.groups` block states. A block that
/// is absent states nothing. Unknown fields are refused.
pub fn stated_groups(host: &Value) -> Result<StatedGroups, ConfigError> {
    let Some(groups) = host
        .get("resource_policy")
        .and_then(|policy| policy.get("groups"))
        .filter(|groups| !groups.is_null())
    else {
        return Ok(StatedGroups::default());
    };
    let groups = groups
        .as_object()
        .ok_or_else(|| refuse(GROUPS, "must be a mapping"))?;
    for key in groups.keys() {
        if !matches!(
            key.as_str(),
            "peer_address" | "rendezvous_port_range" | "require_rdma"
        ) {
            return Err(ConfigError::new(
                ConfigErrorCode::UnknownField,
                format!("{GROUPS}.{key}"),
                "unknown groups field",
            ));
        }
    }
    let peer = groups
        .get("peer_address")
        .map(|value| {
            let path = format!("{GROUPS}.peer_address");
            let text = value
                .as_str()
                .ok_or_else(|| refuse(&path, "must be an IP address"))?;
            peer_address(&path, text)
        })
        .transpose()?;
    let ports = groups
        .get("rendezvous_port_range")
        .map(|range| {
            let path = format!("{GROUPS}.rendezvous_port_range");
            let port = |key: &str| range.get(key).and_then(number);
            let (Some(start), Some(end)) = (port("start"), port("end")) else {
                return Err(refuse(&path, "must state `start` and `end` ports"));
            };
            // ADR 0028 §3: inclusive, 1024 <= start <= end.
            crate::engine_settings::port_range(&path, &format!("{start}-{end}"))
        })
        .transpose()?;
    let require_rdma = groups
        .get("require_rdma")
        .map(|value| {
            let path = format!("{GROUPS}.require_rdma");
            match value {
                Value::Bool(flag) => Ok(*flag),
                Value::String(text) => crate::engine_settings::boolean(&path, text),
                _ => Err(refuse(&path, "must be `true` or `false`")),
            }
        })
        .transpose()?;
    Ok(StatedGroups {
        peer_address: peer,
        rendezvous_ports: ports,
        require_rdma,
    })
}

/// ADR 0028 §3: the host's group policy, defaults applied. Compaction never
/// depends on it; it only governs groups.
pub fn host_groups_policy(host: &Value) -> Result<GroupsPolicy, ConfigError> {
    let stated = stated_groups(host)?;
    Ok(GroupsPolicy {
        peer_address: stated.peer_address,
        rendezvous_ports: stated
            .rendezvous_ports
            .map_or(DEFAULT_RENDEZVOUS_PORTS, |(start, end)| start..=end),
        require_rdma: stated.require_rdma.unwrap_or(false),
    })
}

//! SPEC §§7,11: reversible host-scoped resource identities at the remote boundary.
use crate::{ConfigError, ConfigErrorCode};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
pub fn ledger_key(host: &str, kind: &str, local: &str) -> String {
    format!("host/{}:{host}/{kind}/{local}", host.len())
}
pub fn policy_fingerprint(host: &Value) -> String {
    format!("{:x}", Sha256::digest(host.to_string().as_bytes()))
}
fn invalid() -> ConfigError {
    ConfigError::new(
        ConfigErrorCode::UnsupportedCombination,
        "host",
        "invalid remote resource namespace",
    )
}
/// Remove role-local settings, retaining every engine and resource safety field.
pub fn local_host_document(raw: &Value) -> Result<Value, ConfigError> {
    let mut host = raw.clone();
    let object = host.as_object_mut().ok_or_else(invalid)?;
    for field in [
        "state_dir",
        "identity_dir",
        "listeners",
        "runtime_dir",
        "ingress",
        "load_report_interval",
        "shutdown",
    ] {
        object.remove(field);
    }
    Ok(host)
}
pub fn scope_host_document(host_id: &str, raw: &Value) -> Result<Value, ConfigError> {
    let mut host = local_host_document(raw)?;
    host["name"] = host_id.into();
    for kind in ["domains", "devices"] {
        let source = host["resource_policy"][kind]
            .as_object_mut()
            .ok_or_else(invalid)?;
        let mut target = Map::new();
        for (key, mut value) in std::mem::take(source) {
            if kind == "devices" {
                let local = value["domain"].as_str().ok_or_else(invalid)?;
                value["domain"] = ledger_key(host_id, "domain", local).into();
            }
            target.insert(
                ledger_key(
                    host_id,
                    if kind == "domains" {
                        "domain"
                    } else {
                        "device"
                    },
                    &key,
                ),
                value,
            );
        }
        *source = target;
    }
    Ok(host)
}
fn transform_deployment(host_id: &str, raw: &Value, scope: bool) -> Result<Value, ConfigError> {
    let mut deployment = raw.clone();
    deployment
        .as_object_mut()
        .ok_or_else(invalid)?
        .remove("host");
    let key = |kind: &str, value: &str| -> Result<String, ConfigError> {
        if scope {
            Ok(ledger_key(host_id, kind, value))
        } else {
            value
                .strip_prefix(&ledger_key(host_id, kind, ""))
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(invalid)
        }
    };
    fn devices(
        value: &mut Value,
        map: &impl Fn(&str, &str) -> Result<String, ConfigError>,
    ) -> Result<(), ConfigError> {
        if value.is_null() {
            return Ok(());
        }
        for claim in value.as_array_mut().ok_or_else(invalid)? {
            // ADR 0013 §2: an unnamed claim (sharing mode only) names no
            // host-local id; resolution against a host assigns one.
            if claim.get("id").is_none() {
                continue;
            }
            claim["id"] = map("device", claim["id"].as_str().ok_or_else(invalid)?)?.into();
        }
        Ok(())
    }
    if let Some(value) = deployment.get_mut("devices") {
        devices(value, &key)?;
    }
    for phase in ["cold", "ready", "parking", "parked", "wake"] {
        let Some(value) = deployment
            .get_mut("resources")
            .and_then(|r| r.get_mut(phase))
        else {
            continue;
        };
        if let Some(devices_value) = value.get_mut("devices") {
            devices(devices_value, &key)?;
        }
        if let Some(allocations) = value.get_mut("allocations") {
            for allocation in allocations.as_array_mut().ok_or_else(invalid)? {
                allocation["domain"] =
                    key("domain", allocation["domain"].as_str().ok_or_else(invalid)?)?.into();
            }
        }
    }
    Ok(deployment)
}
pub fn scope_deployment_document(host_id: &str, raw: &Value) -> Result<Value, ConfigError> {
    transform_deployment(host_id, raw, true)
}
pub fn local_deployment_document(host_id: &str, scoped: &Value) -> Result<Value, ConfigError> {
    transform_deployment(host_id, scoped, false)
}

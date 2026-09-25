//! Output plumbing: stdout carries results/IDs, stderr carries diagnostics,
//! `--format json` (or the older `--output json`) selects machine mode, and
//! stable exit codes per design §7. Record views print tables otherwise
//! (see `table`).

use std::fmt;

use crate::grammar::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitCode(pub i32);

impl ExitCode {
    pub const SUCCESS: Self = Self(0);
    pub const INVALID_CONFIG: Self = Self(2);
    pub const UNAUTHORIZED: Self = Self(3);
    pub const INSUFFICIENT_RESOURCES: Self = Self(4);
    pub const UNSUPPORTED: Self = Self(5);
    pub const UNRECONCILED: Self = Self(6);
    pub const DEVICE_CONFLICT: Self = Self(7);
    pub const CATEGORY_LIMIT: Self = Self(8);
    pub const ACTIVATION_TIMEOUT: Self = Self(10);
    pub const TOPOLOGY_UNKNOWN: Self = Self(11);
    pub const NO_SAFE_ESTIMATE: Self = Self(12);
    /// Store/I-O/runtime-boot failures: never masquerade as invalid config (F1 design §4).
    pub const INTERNAL: Self = Self(13);
    /// SPEC §4.1, ADR 0016: the controller answered that this host's
    /// certificate is revoked. The host role exits instead of reconnecting;
    /// its engines keep running for `join host --recover` to re-prove. The
    /// packaged units do not restart on it (`RestartPreventExitStatus`).
    pub const HOST_REVOKED: Self = Self(14);
    /// Owner decision 2026-09-25: no allowed host is eligible for placement
    /// (drain-only after version skew, draining, revoked, offline). A CLI
    /// command's exit, never a role's, so no unit lists it.
    pub const HOST_INELIGIBLE: Self = Self(15);
    /// ADR 0018 §6: engine registration's closed codes (9 stays unused).
    pub const ENGINE_NOT_FOUND: Self = Self(16);
    pub const ENGINE_UNSUPPORTED: Self = Self(17);
    pub const ENGINE_VERSION_FAILED: Self = Self(18);
    pub const PROFILE_EXISTS: Self = Self(19);
    pub const PROFILE_IN_USE: Self = Self(20);
    pub const PUBLISH_REJECTED: Self = Self(21);
    pub const AGENT_UNREACHABLE: Self = Self(22);
    pub const NOT_INTERACTIVE: Self = Self(23);
    /// ADR 0018 §7: a deploy named a runtime profile no allowed host
    /// publishes; nothing was stored.
    pub const PROFILE_NOT_PUBLISHED: Self = Self(24);
}

impl From<ExitCode> for u8 {
    fn from(code: ExitCode) -> Self {
        code.0 as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationError {
    InvalidConfig,
    Unauthorized,
    InsufficientResources,
    Unsupported,
    Unreconciled,
    DeviceConflict,
    CategoryLimit,
    ActivationTimeout,
    TopologyUnknown,
    NoSafeEstimate,
}

impl OperationError {
    pub const fn exit_code(self) -> ExitCode {
        match self {
            OperationError::InvalidConfig => ExitCode::INVALID_CONFIG,
            OperationError::Unauthorized => ExitCode::UNAUTHORIZED,
            OperationError::InsufficientResources => ExitCode::INSUFFICIENT_RESOURCES,
            OperationError::Unsupported => ExitCode::UNSUPPORTED,
            OperationError::Unreconciled => ExitCode::UNRECONCILED,
            OperationError::DeviceConflict => ExitCode::DEVICE_CONFLICT,
            OperationError::CategoryLimit => ExitCode::CATEGORY_LIMIT,
            OperationError::ActivationTimeout => ExitCode::ACTIVATION_TIMEOUT,
            OperationError::TopologyUnknown => ExitCode::TOPOLOGY_UNKNOWN,
            OperationError::NoSafeEstimate => ExitCode::NO_SAFE_ESTIMATE,
        }
    }

    pub const fn code(self) -> &'static str {
        match self {
            OperationError::InvalidConfig => "invalid_config",
            OperationError::Unauthorized => "unauthorized",
            OperationError::InsufficientResources => "insufficient_resources",
            OperationError::Unsupported => "unsupported",
            OperationError::Unreconciled => "unreconciled",
            OperationError::DeviceConflict => "device_conflict",
            OperationError::CategoryLimit => "category_limit",
            OperationError::ActivationTimeout => "activation_timeout",
            OperationError::TopologyUnknown => "topology_unknown",
            OperationError::NoSafeEstimate => "no_safe_estimate",
        }
    }
}

impl From<OperationError> for ExitCode {
    fn from(err: OperationError) -> Self {
        err.exit_code()
    }
}

impl fmt::Display for OperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} [{}]", self.code(), self.exit_code().0)
    }
}

/// SPEC §13.2 / T33: the role's durable state (store or host journal) was
/// written by a newer mllm. Exits as unsupported, which service managers are told
/// not to restart: only a newer binary or a restored backup resolves it.
pub const STORE_FROM_NEWER_VERSION: &str = "store_from_newer_version";

/// SPEC §4.1, ADR 0016: the host role stopped because the controller revoked
/// its certificate. Exits with [`ExitCode::HOST_REVOKED`].
pub const HOST_REVOKED: &str = "host_revoked";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredError {
    pub code: &'static str,
    pub message: String,
}

impl StructuredError {
    pub fn not_yet_implemented(action: &str) -> Self {
        Self {
            code: "not_implemented",
            message: format!(
                "{action} is not yet implemented; role wiring lands with the standalone exit gate"
            ),
        }
    }

    pub fn exit_code(&self) -> ExitCode {
        match self.code {
            "internal" | "management_unavailable" | "operation_failed" => ExitCode::INTERNAL,
            "invalid_config" | "command_rejected" | "not_found" => ExitCode::INVALID_CONFIG,
            "unauthorized" => ExitCode::UNAUTHORIZED,
            "insufficient_resources" => ExitCode::INSUFFICIENT_RESOURCES,
            "unreconciled" => ExitCode::UNRECONCILED,
            "device_conflict" => ExitCode::DEVICE_CONFLICT,
            "category_limit" => ExitCode::CATEGORY_LIMIT,
            "activation_timeout" => ExitCode::ACTIVATION_TIMEOUT,
            "topology_unknown" => ExitCode::TOPOLOGY_UNKNOWN,
            "no_safe_estimate" => ExitCode::NO_SAFE_ESTIMATE,
            HOST_REVOKED => ExitCode::HOST_REVOKED,
            "host_ineligible" => ExitCode::HOST_INELIGIBLE,
            "engine_not_found" => ExitCode::ENGINE_NOT_FOUND,
            "engine_unsupported" => ExitCode::ENGINE_UNSUPPORTED,
            "engine_version_failed" => ExitCode::ENGINE_VERSION_FAILED,
            "profile_exists" => ExitCode::PROFILE_EXISTS,
            "profile_in_use" => ExitCode::PROFILE_IN_USE,
            "publish_rejected" => ExitCode::PUBLISH_REJECTED,
            "agent_unreachable" => ExitCode::AGENT_UNREACHABLE,
            "not_interactive" => ExitCode::NOT_INTERACTIVE,
            "profile_not_published" => ExitCode::PROFILE_NOT_PUBLISHED,
            _ => ExitCode::UNSUPPORTED,
        }
    }

    pub fn to_json(&self) -> String {
        format!(
            "{{\"code\":{},\"message\":{}}}",
            json_string(self.code),
            json_string(&self.message)
        )
    }
}

impl fmt::Display for StructuredError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error [{}]: {}", self.code, self.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

impl OutputFormat {
    pub fn from_flag(flag: &str) -> Option<Self> {
        match flag {
            "json" => Some(OutputFormat::Json),
            "text" => Some(OutputFormat::Text),
            _ => None,
        }
    }
}

pub fn print_error(err: &StructuredError, format: OutputFormat) {
    match format {
        OutputFormat::Text => eprintln!("{err}"),
        OutputFormat::Json => eprintln!("{}", err.to_json()),
    }
}

pub fn exit_code_for_cli_error(err: &CliError) -> ExitCode {
    match err {
        CliError::Clap(_) => ExitCode::INVALID_CONFIG,
    }
}

/// SPEC §9.1 / T21 / ADR 0012 / owner decision P4: human-readable warnings
/// for every deployment and host installation in a status, inspect or list
/// view whose launch exposes vLLM development controls, and for any whose
/// exposure could not be derived. The server derives the mark from effective
/// configuration; this only renders it. Printed to stderr in text mode, so the
/// JSON result on stdout is unchanged.
pub fn development_controls_notices(view: &serde_json::Value) -> Vec<String> {
    let mut notices = Vec::new();
    collect_notices(view, &mut notices);
    notices
}

fn collect_notices(view: &serde_json::Value, notices: &mut Vec<String>) {
    use serde_json::Value;
    match view {
        Value::Array(items) => items.iter().for_each(|item| collect_notices(item, notices)),
        Value::Object(object) => {
            if let Some(hosts) = object.get("hosts") {
                collect_notices(hosts, notices);
            }
            if let Some(deployments) = object.get("deployments") {
                collect_notices(deployments, notices);
            }
            let Some(controls) = object.get("development_controls") else {
                return;
            };
            let label = object
                .get("name")
                .or_else(|| object.get("id"))
                .or_else(|| object.get("host_id"))
                .and_then(Value::as_str)
                .unwrap_or("?");
            if object.contains_key("host_id") {
                let installations = controls["installations"].as_array();
                for installation in installations.into_iter().flatten() {
                    let profile = installation["profile"].as_str().unwrap_or("?");
                    push_notice(
                        &format!("host {label} installation {profile}"),
                        installation,
                        notices,
                    );
                }
                if controls["state"] == "unknown"
                    && installations.is_none_or(|all| all.iter().all(|i| i["state"] != "unknown"))
                {
                    notices.push(format!(
                        "warning: host {label}: development-control exposure is unknown (no readable published configuration)"
                    ));
                }
            } else {
                push_notice(&format!("deployment {label}"), controls, notices);
                // ADR 0013 §6 (W14 per-instance marking): an instance carries the
                // mark of the revision as resolved on its own host. Only a mark
                // that differs from the deployment's is repeated.
                for instance in object
                    .get("instances")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let mark = &instance["development_controls"];
                    if !mark.is_null() && mark != controls {
                        let index = instance["index"].as_u64().unwrap_or(0);
                        push_notice(
                            &format!("deployment {label} instance {index}"),
                            mark,
                            notices,
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

fn push_notice(subject: &str, controls: &serde_json::Value, notices: &mut Vec<String>) {
    let list = |key: &str| {
        controls[key]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    match controls["state"].as_str() {
        Some("exposed") => {
            let deep_park = controls["deep_park"].as_str().unwrap_or("?");
            let source = controls["deep_park_source"].as_str().unwrap_or("host_policy");
            // ADR 0014 §4: an installation's mark applies to the parking
            // deployments launched on it, where sleep mode is derived on.
            let scope = match controls["applies_to"].as_str() {
                Some("parking_deployments") => " for parking deployments",
                _ => "",
            };
            notices.push(format!(
                "warning: {subject} launches{scope} with vLLM development mode on (deep_park {deep_park} ({source}), sleep mode); \
                 exposed controls: {}; mitigations in force: {}; not production-safe (SPEC §9.1)",
                list("surface"),
                list("mitigations"),
            ));
        }
        Some("unknown") => notices.push(format!(
            "warning: {subject}: development-control exposure is unknown (effective configuration unavailable)"
        )),
        _ => {}
    }
    // Owner decision 2026-09-22 (T21): SGLang exempts `/metrics` from its API
    // key. Accepted, and noted wherever the server derived the mark.
    let surfaces = &controls["unauthenticated_local_surfaces"];
    if surfaces.is_object() {
        let routes = surfaces["surface"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        notices.push(format!(
            "note: {subject} serves {routes} without authentication on its {} listener ({}; accepted by owner decision)",
            surfaces["listener"].as_str().unwrap_or("?"),
            surfaces["access"].as_str().unwrap_or("?"),
        ));
    }
}

pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

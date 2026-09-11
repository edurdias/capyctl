//! Output plumbing: stdout carries results/IDs, stderr carries diagnostics,
//! `--output json` selects machine mode, and stable exit codes per design §7.

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredError {
    pub code: &'static str,
    pub message: String,
}

impl StructuredError {
    pub fn not_yet_implemented(action: &str) -> Self {
        Self {
            code: "not_implemented",
            message: format!("{action} is not yet implemented; role wiring lands with the standalone exit gate"),
        }
    }

    pub fn exit_code(&self) -> ExitCode {
        ExitCode::UNSUPPORTED
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
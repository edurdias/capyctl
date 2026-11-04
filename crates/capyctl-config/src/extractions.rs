//! ADR 0041 §§1–2: custom-only extraction models and their declared field types.
use crate::effective::Residency;
use crate::engine_policy::Engine;
use crate::{ConfigError, ConfigErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const HOST_CAPABILITY: &str = "extractions";
pub const CAPABILITY_MISSING: &str = "capability_missing:extractions";

/// ADR 0041 §2: declaration order is the canonical manifest order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Text,
    Number,
    Date,
    Time,
    Datetime,
    Duration,
    List,
    Estimate,
    Forecast,
}
impl FieldType {
    pub const ALL: [Self; 9] = [
        Self::Text,
        Self::Number,
        Self::Date,
        Self::Time,
        Self::Datetime,
        Self::Duration,
        Self::List,
        Self::Estimate,
        Self::Forecast,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Number => "number",
            Self::Date => "date",
            Self::Time => "time",
            Self::Datetime => "datetime",
            Self::Duration => "duration",
            Self::List => "list",
            Self::Estimate => "estimate",
            Self::Forecast => "forecast",
        }
    }
}
fn invalid(path: &str, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}
/// ADR 0041 §1: no native engine, at any version, serves this contract.
pub fn check_engine(engine: Engine) -> Result<(), ConfigError> {
    if engine == Engine::Custom {
        return Ok(());
    }
    Err(invalid("api", format!("{CAPABILITY_MISSING}: {} has no extraction contract; use a custom engine whose manifest declares api: extractions", engine.name())))
}
pub fn default_residency() -> Residency {
    Residency::RestartOnly
}
/// ADR 0041 §1: one machine, restarting instead of parking.
pub fn check_document(deployment: &Value) -> Result<(), ConfigError> {
    if deployment.get("topology").is_some_and(|v| !v.is_null()) {
        return Err(invalid(
            "topology",
            "an extractions deployment runs on one machine; remove `topology`",
        ));
    }
    if let Some(tier @ ("deep" | "host_backed")) =
        deployment.get("residency").and_then(Value::as_str)
    {
        return Err(invalid("residency", format!("an extractions deployment restarts instead of parking, so residency `{tier}` is refused; declare `restart_only` or leave it out")));
    }
    Ok(())
}

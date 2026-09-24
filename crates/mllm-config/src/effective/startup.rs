//! Owner decision 2026-09-23: the startup memory budget of a resolved revision.
//!
//! Admission reserves a launch's startup peak (its cold phase, ADR 0007) from
//! arm until Ready, then the steady request. The peak is declared as
//! `engine_config.memory.startup`, measured on a first run on a host (kept by
//! the store per revision, host and installation), or a conservative
//! placeholder default. This module names what the frozen revision says; the
//! store adds the measured case.

use super::EffectiveDeployment;
use mllm_domain::launch::SettingSource;
use serde::Serialize;

/// Where a startup reservation came from (T14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupProvenance {
    /// `engine_config.memory.startup`.
    Declared,
    /// The peak a first run on this host measured (the store's record).
    Measured,
    /// The placeholder `max(request, weights × 1.6 + margin)`.
    Default,
    /// The cold phase of a declared `resources:` block.
    Resources,
    /// A revision resolved before the startup budget existed: the request.
    Request,
    /// Owner decision 2026-09-23: an unmeasured placeholder above the host's
    /// managed limit. The first start runs alone on its host, reserving the
    /// whole managed limit, and is measured there.
    WholeHost,
}

/// A startup reservation and its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StartupBudget {
    /// The bytes the cold phase reserves, summed over its allocations.
    pub bytes: i64,
    pub provenance: StartupProvenance,
}

/// The startup budget the frozen revision states, before any measurement.
pub fn startup_budget(effective: &EffectiveDeployment) -> StartupBudget {
    let bytes = effective
        .resources
        .cold
        .allocations
        .iter()
        .fold(0_i64, |total, allocation| {
            total.saturating_add(allocation.bytes)
        });
    let settings = &effective.engine_config;
    let provenance = if settings.provenance().get("resources") != Some(&SettingSource::Derived) {
        StartupProvenance::Resources
    } else if settings.memory().startup_bytes.is_none() {
        StartupProvenance::Request
    } else if settings.provenance().contains_key("memory.startup") {
        StartupProvenance::Default
    } else {
        StartupProvenance::Declared
    };
    StartupBudget { bytes, provenance }
}

/// Whether a measured peak may replace the frozen startup budget: only a
/// placeholder or a pre-budget request is replaced; a declared startup or a
/// declared cold phase is the operator's and stays. The derived cold phase
/// must be one allocation for the measured bytes to replace it.
pub fn measurable(effective: &EffectiveDeployment) -> bool {
    matches!(
        startup_budget(effective).provenance,
        StartupProvenance::Default | StartupProvenance::Request
    ) && effective.resources.cold.allocations.len() == 1
}

/// Owner decision 2026-09-23: the checks a declared `memory.startup` must pass
/// without a host (`validate config` with no `--host`): a positive quantity,
/// not below a declared request, and not beside a declared `resources:` block.
/// Resolution against a host repeats them.
pub fn validate_declared_startup(deployment: &serde_json::Value) -> Result<(), crate::ConfigError> {
    use super::{invalid, parse_bytes};
    const PATH: &str = "engine_config.memory.startup";
    let memory = &deployment["engine_config"]["memory"];
    let Some(text) = memory.get("startup") else {
        return Ok(());
    };
    let peak = parse_bytes(
        text.as_str()
            .ok_or_else(|| invalid(PATH, "must be a byte quantity"))?,
    )?;
    if peak <= 0 {
        return Err(invalid(PATH, "must be positive"));
    }
    if deployment.get("resources").is_some() {
        return Err(invalid(
            PATH,
            "a deployment that declares its resources states its startup peak as the cold \
             phase; remove memory.startup or the resources block",
        ));
    }
    if let Some(request) = memory.get("request").and_then(serde_json::Value::as_str) {
        if peak < parse_bytes(request)? {
            return Err(invalid(
                PATH,
                "the startup peak cannot be below the steady memory request",
            ));
        }
    }
    Ok(())
}

//! Owner decision 2026-09-23: the startup memory budget of a resolved revision.
//!
//! Admission reserves a launch's startup peak (its cold phase, ADR 0007) from
//! arm until Ready, then the steady request. The peak is declared as
//! `engine_config.memory.startup`, measured on a first run on a host (kept by
//! the store per revision, host and installation), or a conservative
//! placeholder default. This module names what the frozen revision says; the
//! store adds the measured case.

use super::EffectiveDeployment;
use capyctl_domain::launch::SettingSource;
use serde::Serialize;

/// Where a startup reservation came from (T14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupProvenance {
    /// `engine_config.memory.startup`.
    Declared,
    /// The peak a first run on this host measured (the store's record).
    Measured,
    /// The placeholder `max(request + graphs, weights × 2.25 + margin)`
    /// (ADR 0014 amendment A8).
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

/// How a first run on a host measures a revision's startup peak, when its
/// frozen budget is only a placeholder (a declared startup or a declared cold
/// phase is the operator's and stays, so nothing is measured for it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupMeasure {
    /// One allocation on one memory pool (unified memory, or a host without
    /// a GPU domain): the largest drop in the pool's published availability.
    Pool { domain: String },
    /// ADR 0019 §3: a discrete GPU, whose cold phase charges the card and
    /// host RAM apart. Each is measured on its own from the launch's own
    /// processes: the card from the GPU memory the driver attributes to them,
    /// host RAM from their resident pages (`RssAnon` + `RssShmem`, ADR 0019
    /// §4), and each domain's estimate is replaced once it is measured.
    ///
    /// ADR 0035 §3: on N GPUs of one host, `device` is rank 0's domain, under
    /// which the largest figure any of the GPUs showed is recorded (keyed by
    /// N); it is applied to every GPU of a later start at that N
    /// ([`startup_device_domains`]).
    Domains { device: String, host: String },
}

/// ADR 0035 §3: every device domain the cold phase charges, in rank order:
/// one on a single GPU, N on N GPUs of one host, none on a unified host.
pub fn startup_device_domains(effective: &EffectiveDeployment) -> Vec<String> {
    effective
        .resources
        .cold
        .allocations
        .iter()
        .filter(|a| {
            effective
                .host
                .domains
                .get(&a.domain)
                .is_some_and(|d| d.memory == super::DomainMemory::Device)
        })
        .map(|a| a.domain.clone())
        .collect()
}

/// A cold phase that charges a discrete GPU more than the card's managed
/// limit, while that limit is all admission may ever reserve there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartupOverLimit {
    /// The card's device domain.
    pub domain: String,
    /// What the first start charges the card.
    pub bytes: i64,
    /// The card's managed limit.
    pub limit_bytes: i64,
}

/// #219 (ADR 0019 §3): the first card whose startup charge in the frozen
/// cold phase is above its managed limit. Such a start is never admitted;
/// unlike one pool (whose first start runs alone on the host, owner decision
/// 2026-09-23) a card has no solo first start, so an estimate is never
/// measured, and a declared peak is never replaced. `None` when every card
/// holds its charge, and on a host without a device domain.
pub fn startup_over_limit(effective: &EffectiveDeployment) -> Option<StartupOverLimit> {
    effective
        .resources
        .cold
        .allocations
        .iter()
        .find_map(|allocation| {
            let domain = effective.host.domains.get(&allocation.domain)?;
            (domain.memory == super::DomainMemory::Device
                && allocation.bytes > domain.managed_limit)
                .then(|| StartupOverLimit {
                    domain: allocation.domain.clone(),
                    bytes: allocation.bytes,
                    limit_bytes: domain.managed_limit,
                })
        })
}

/// How a measured peak may replace the frozen startup budget, if it may:
/// only a placeholder or a pre-budget request is replaced. The derived cold
/// phase must be one allocation, or exactly one card and one host RAM
/// allocation on a discrete GPU, for the measured bytes to replace it.
pub fn startup_measure(effective: &EffectiveDeployment) -> Option<StartupMeasure> {
    if !matches!(
        startup_budget(effective).provenance,
        StartupProvenance::Default | StartupProvenance::Request
    ) {
        return None;
    }
    let on_device = |domain: &str| {
        effective
            .host
            .domains
            .get(domain)
            .map(|d| d.memory == super::DomainMemory::Device)
    };
    match effective.resources.cold.allocations.as_slice() {
        [allocation] => Some(StartupMeasure::Pool {
            domain: allocation.domain.clone(),
        }),
        [a, b] => match (on_device(&a.domain)?, on_device(&b.domain)?) {
            (true, false) => Some(StartupMeasure::Domains {
                device: a.domain.clone(),
                host: b.domain.clone(),
            }),
            (false, true) => Some(StartupMeasure::Domains {
                device: b.domain.clone(),
                host: a.domain.clone(),
            }),
            _ => None,
        },
        // ADR 0035 §3: N GPUs (one rank each) and the host's RAM.
        allocations if allocations.len() > 2 => {
            let (devices, hosts): (Vec<&super::Allocation>, Vec<&super::Allocation>) = allocations
                .iter()
                .partition(|a| on_device(&a.domain) == Some(true));
            match (devices.first(), hosts.as_slice()) {
                (Some(device), [host]) if on_device(&host.domain) == Some(false) => {
                    Some(StartupMeasure::Domains {
                        device: device.domain.clone(),
                        host: host.domain.clone(),
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
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

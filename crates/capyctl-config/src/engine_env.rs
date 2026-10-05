//! The engine environment a profile and a deployment may set.
//!
//! ADR 0028 §2.1: a profile's own `env` is host-authored; a deployment's
//! `engine_config.env` names must match a profile `security.approved_env` entry.
//! Names CapyCTL owns are never settable at either level, whatever an approval
//! says. Values are bounded, single-line, and shown in the effective
//! configuration: they are not secret storage.

use serde::Serialize;
use std::collections::BTreeMap;

/// ADR 0028 §2.1 rule 1: whole families CapyCTL renders or closes.
/// `PYTHON` takes every interpreter switch (`PYTHONHOME`, `PYTHONSTARTUP`, ...);
/// the safe name `PYTHONUNBUFFERED` is the one exception ([`is_owned`]).
pub const OWNED_PREFIXES: &[&str] = &["NCCL_", "GLOO_", "MASTER_", "CAPYCTL_", "LD_", "PYTHON"];

/// ADR 0028 §2.1 rule 1: single names CapyCTL renders or closes, across all
/// three engines.
pub const OWNED_NAMES: &[&str] = &[
    "VLLM_HOST_IP",
    "SGLANG_HOST_IP",
    "HOST_IP",
    "SGLANG_LOCAL_IP_NIC",
    "SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE",
    "TF_COMM_BACKEND",
    "TF_NCCL_LIB",
    "PATH",
    "PYTHONPATH",
    "CUDA_HOME",
    "CUDA_VISIBLE_DEVICES",
    // ADR 0028 §2.1 (coordinator ruling): every other name the adapters pin or
    // render in their fixed environments.
    "HOME",
    "CUDA_DEVICE_ORDER",
    "HF_HUB_OFFLINE",
    "TRANSFORMERS_OFFLINE",
    "VLLM_PLUGINS",
    "VLLM_SERVER_DEV_MODE",
    "VLLM_API_KEY",
    "TORCH_EXTENSIONS_DIR",
    "TENSORFOLD_CUDA_MEMORY_LIMIT_GB",
    "TENSORFOLD_NO_UPDATE_CHECK",
    // Ruling R17: the engine port clashes with group ports; the other is a
    // security switch.
    "VLLM_PORT",
    "VLLM_ALLOW_INSECURE_SERIALIZATION",
];

/// ADR 0028 §2.1 rule 2: the existing safe names. They need no approval and
/// keep their value rules at both levels.
const SAFE_NAMES: &[&str] = &[
    "RUST_LOG",
    "TOKENIZERS_PARALLELISM",
    "PYTHONUNBUFFERED",
    "MAX_JOBS",
    "FLASHINFER_NVCC_THREADS",
];

/// Safe names whose value must be a positive integer.
const COUNT_NAMES: &[&str] = &["MAX_JOBS", "FLASHINFER_NVCC_THREADS"];

/// ADR 0028 §2.1 rule 7.
const MAX_VALUE_BYTES: usize = 4096;
const MAX_NAMES: usize = 64;
const MAX_NAME_BYTES: usize = 128;

/// Whether CapyCTL owns `name` (rule 1). Compared upper-cased, so a lower-case
/// spelling of an owned name is not a way around it.
pub fn is_owned(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if SAFE_NAMES.contains(&upper.as_str()) {
        return false;
    }
    OWNED_NAMES.contains(&upper.as_str())
        || OWNED_PREFIXES
            .iter()
            .any(|prefix| upper.starts_with(prefix))
}

/// Why an engine environment was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvRefusal {
    Reserved(String),
    NotApproved(String),
    Conflict(String),
    Invalid(String),
}

impl EnvRefusal {
    /// The closed reason code (ADR 0028 §16).
    pub fn code(&self) -> String {
        match self {
            EnvRefusal::Reserved(name) => format!("engine_env_reserved:{name}"),
            EnvRefusal::NotApproved(name) => format!("engine_env_not_approved:{name}"),
            EnvRefusal::Conflict(name) => format!("engine_env_conflict:{name}"),
            EnvRefusal::Invalid(detail) => detail.clone(),
        }
    }
}

impl std::fmt::Display for EnvRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.code())
    }
}

impl std::error::Error for EnvRefusal {}

/// A profile's `security.approved_env`: names and trailing-`*` globs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovedEnv(Vec<String>);

impl ApprovedEnv {
    /// ADR 0028 §2.1 rule 3: upper-case `A`-`Z`, `0`-`9`, `_`, optionally ending
    /// in one `*`; a bare `*` is refused, and so is an entry matching only
    /// owned names.
    pub fn parse(entries: &[String]) -> Result<ApprovedEnv, EnvRefusal> {
        if entries.len() > MAX_NAMES {
            return Err(EnvRefusal::Invalid(format!(
                "approved_env holds more than {MAX_NAMES} entries"
            )));
        }
        for entry in entries {
            let (stem, glob) = match entry.strip_suffix('*') {
                Some(stem) => (stem, true),
                None => (entry.as_str(), false),
            };
            let shaped = !stem.is_empty()
                && entry.len() <= MAX_NAME_BYTES
                && stem
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
            if !shaped {
                return Err(EnvRefusal::Invalid(format!(
                    "approved_env entry `{entry}` must be upper-case letters, digits and `_`, \
                     optionally ending in one `*` (a bare `*` is refused)"
                )));
            }
            // A glob stays wide when its stem is shorter than an owned prefix:
            // the concrete name is checked at resolution (rule 1).
            let owned_only = if glob {
                OWNED_PREFIXES.iter().any(|prefix| stem.starts_with(prefix))
            } else {
                is_owned(stem)
            };
            if owned_only {
                return Err(EnvRefusal::Reserved(entry.clone()));
            }
        }
        Ok(ApprovedEnv(entries.to_vec()))
    }

    /// Whether an entry matches `name`: exact, or by prefix for a glob.
    pub fn admits(&self, name: &str) -> bool {
        self.0.iter().any(|entry| match entry.strip_suffix('*') {
            Some(stem) => name.starts_with(stem),
            None => entry == name,
        })
    }
}

/// Where a resolved variable came from (ADR 0028 §2.1 rule 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvSource {
    Profile,
    Deployment,
}

/// What output shows in place of an environment value.
pub const REDACTED: &str = "<redacted>";

/// One variable as output shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedactedVar {
    pub value: &'static str,
    pub source: EnvSource,
}

/// The engine environment after merging: each variable with its value and source.
#[derive(Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResolvedEnv {
    pub vars: BTreeMap<String, (String, EnvSource)>,
}

/// Debug shows names and sources only: values may hold tokens.
impl std::fmt::Debug for ResolvedEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedEnv")
            .field("vars", &self.redacted())
            .finish()
    }
}

impl ResolvedEnv {
    /// Name to value, for the launch environment.
    pub fn values(&self) -> BTreeMap<String, String> {
        self.vars
            .iter()
            .map(|(name, (value, _))| (name.clone(), value.clone()))
            .collect()
    }

    /// The deployment-level entries alone, which the recipe fingerprint adds
    /// (the profile's own env is already part of it).
    pub fn deployment_values(&self) -> BTreeMap<String, String> {
        self.vars
            .iter()
            .filter(|(_, (_, source))| *source == EnvSource::Deployment)
            .map(|(name, (value, _))| (name.clone(), value.clone()))
            .collect()
    }

    /// ADR 0028 §2.1: the view every user-facing rendering uses. Values stay
    /// stored (they feed the fingerprint and the launch); output shows the name
    /// and source only.
    pub fn redacted(&self) -> BTreeMap<String, RedactedVar> {
        self.vars
            .iter()
            .map(|(name, (_, source))| {
                (
                    name.clone(),
                    RedactedVar {
                        value: REDACTED,
                        source: *source,
                    },
                )
            })
            .collect()
    }

    /// Whether no deployment-level entry exists. The effective configuration
    /// shows the resolved env only when it adds something to the profile's
    /// own `env`, which keeps every earlier document byte-identical.
    pub fn has_no_deployment_entries(&self) -> bool {
        self.vars
            .values()
            .all(|(_, source)| *source == EnvSource::Profile)
    }
}

fn check_entry(name: &str, value: &str) -> Result<(), EnvRefusal> {
    let shaped = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !shaped {
        return Err(EnvRefusal::Invalid(format!(
            "environment name `{name}` must be letters, digits and `_`, not starting with a digit"
        )));
    }
    if is_owned(name) {
        return Err(EnvRefusal::Reserved(name.to_owned()));
    }
    if value.len() > MAX_VALUE_BYTES || value.contains(['\n', '\r', '\0']) {
        return Err(EnvRefusal::Invalid(format!(
            "environment value of `{name}` must be one line of at most {MAX_VALUE_BYTES} bytes"
        )));
    }
    if COUNT_NAMES.contains(&name) && !value.parse::<u32>().is_ok_and(|count| count >= 1) {
        return Err(EnvRefusal::Invalid(format!(
            "environment value of `{name}` must be a positive integer"
        )));
    }
    Ok(())
}

/// ADR 0028 §2.1 rules 1-4 and 7. The profile's names need no approval; a
/// deployment name needs a matching approval unless it is a safe name; the
/// deployment's value overrides the profile's.
pub fn resolve_engine_env(
    profile_env: &BTreeMap<String, String>,
    approved: &ApprovedEnv,
    deployment_env: &BTreeMap<String, String>,
) -> Result<ResolvedEnv, EnvRefusal> {
    for (level, env) in [("profile", profile_env), ("deployment", deployment_env)] {
        if env.len() > MAX_NAMES {
            return Err(EnvRefusal::Invalid(format!(
                "the {level} env holds more than {MAX_NAMES} names"
            )));
        }
    }
    let mut vars = BTreeMap::new();
    for (name, value) in profile_env {
        check_entry(name, value)?;
        vars.insert(name.clone(), (value.clone(), EnvSource::Profile));
    }
    for (name, value) in deployment_env {
        check_entry(name, value)?;
        if !SAFE_NAMES.contains(&name.as_str()) && !approved.admits(name) {
            return Err(EnvRefusal::NotApproved(name.clone()));
        }
        vars.insert(name.clone(), (value.clone(), EnvSource::Deployment));
    }
    Ok(ResolvedEnv { vars })
}

//! The engine environment a profile and a deployment may set.
//!
//! ADR 0028 §2.1: a profile's own `env` is host-authored; a deployment's
//! `engine_config.env` names must match a profile `security.approved_env` entry.
//! Names CapyCTL owns are never settable at either level, whatever an approval
//! says. Values are bounded and single-line; they are stored for the launch
//! and the recipe fingerprint, and output shows each name with its source,
//! never its value (R10). They are not secret storage.

use crate::engine_policy::{FlagClass, FlagRow, NameMatch};
use serde::Serialize;
use std::collections::BTreeMap;

/// ADR 0028 §2.1 rule 1: whole families CapyCTL renders or closes.
/// `PYTHON` takes every interpreter switch (`PYTHONHOME`, `PYTHONSTARTUP`, ...);
/// the safe name `PYTHONUNBUFFERED` is the one exception ([`is_owned`]).
/// `SMG_` closes smg-grpc-servicer's settings (ADR 0008 amendment 2026-10-17):
/// its SGLang plugin activates on `SMG_SGLANG_ZMQ_HANDSHAKE`.
pub const OWNED_PREFIXES: &[&str] = &[
    "NCCL_", "GLOO_", "MASTER_", "CAPYCTL_", "LD_", "PYTHON", "SMG_",
];

/// ADR 0008 amendment 2026-10-17, SPEC §13.3 / T21: SGLang's plugin
/// selection, rendered into every SGLang process CapyCTL starts.
pub const SGLANG_PLUGINS: &str = "SGLANG_PLUGINS";

/// The [`SGLANG_PLUGINS`] value: a whitelist naming no entry point, so
/// SGLang's loader (0.5.20 to 0.5.22) skips every installed plugin unloaded.
/// SGLang 0.5.22 requires smg-grpc-servicer, which installs two. The runtime
/// guard (`runtime/sglang_startup_guards.py` `NO_PLUGINS_SELECTION`) accepts
/// installed plugins only under exactly this value.
pub const SGLANG_NO_PLUGINS: &str = "capyctl-loads-no-plugins";

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
    // ADR 0008 amendment 2026-10-17: SGLang's plugin and platform selection.
    "SGLANG_PLUGINS",
    "SGLANG_PLATFORM",
    "VLLM_SERVER_DEV_MODE",
    "VLLM_API_KEY",
    // TensorFold reads this key (`server/authentication.py`) and then refuses
    // the unkeyed health and metrics routes CapyCTL reads.
    "TENSORFOLD_API_KEY",
    "TORCH_EXTENSIONS_DIR",
    "TENSORFOLD_CUDA_MEMORY_LIMIT_GB",
    "TENSORFOLD_NO_UPDATE_CHECK",
    // Ruling R17: the engine port clashes with group ports; the other is a
    // security switch.
    "VLLM_PORT",
    "VLLM_ALLOW_INSECURE_SERIALIZATION",
    // Owner decision 2026-10-22: the engines' usage-reporting opt-outs are
    // the host's (`engine_telemetry`), never a profile's or a deployment's.
    // vLLM reads all three (`vllm/envs.py`, `usage/usage_lib.py`);
    // `DO_NOT_TRACK` is the general convention every engine is started with.
    "DO_NOT_TRACK",
    "VLLM_DO_NOT_TRACK",
    "VLLM_NO_USAGE_STATS",
];

/// SPEC §8.2, T21 (owner decision 2026-10-20): environment families that make
/// SGLang dump requests or tensors to disk. Nobody may set them, whatever an
/// approval says; refused as `engine_env_reserved:<name>`. Enumerated from
/// SGLang 0.5.21 and 0.5.22 (`srt/environ.py` and the modules that read the
/// environment directly).
pub const DUMP_RESERVED_PREFIXES: &[(&str, &str)] = &[
    (
        "DUMPER_",
        "SGLang's dumper (srt/debug_utils/dumper.py) writes tensors to disk; \
         DUMPER_ENABLE turns it on",
    ),
    (
        "SGLANG_DUMP",
        "SGLang dump settings (SGLANG_DUMP_LOADER_DIR replays a saved dump)",
    ),
    (
        "SGLANG_KERNEL_API_",
        "kernel API logging; SGLANG_KERNEL_API_LOGLEVEL dumps kernel input \
         tensors to SGLANG_KERNEL_API_DUMP_DIR",
    ),
    (
        "SGLANG_CUDA_COREDUMP",
        "a CUDA core dump writes GPU memory, which holds requests' KV cache, \
         to disk",
    ),
];

/// SPEC §8.2, T21 (owner decision 2026-10-20): single SGLang names that turn
/// on a dump to disk or name where one is written.
pub const DUMP_RESERVED_NAMES: &[(&str, &str)] = &[
    (
        "SGLANG_DSPARK_DEBUG_DUMP",
        "dumps DSpark speculative-decoding state",
    ),
    (
        "SGLANG_DSPARK_ENABLE_SPS_RECORD",
        "records DSpark speculative-decoding state, as SGLANG_DSPARK_DEBUG_DUMP does",
    ),
    (
        "SGLANG_EXPERT_DISTRIBUTION_RECORDER_DIR",
        "where the expert-distribution recorder writes its dumps",
    ),
    (
        "SGLANG_TORCH_PROFILER_DIR",
        "where profiler traces are written",
    ),
];

/// ADR 0033 §11: LMCache's configuration variables. Its 0.5.6 vLLM helper
/// reads its configuration from the environment after any file, so a
/// variable here would change what CapyCTL renders. Reserved on every engine,
/// whatever an approval says; refused as `engine_env_reserved:<name>`.
pub const LMCACHE_RESERVED_PREFIX: (&str, &str) = (
    "LMCACHE_",
    "LMCache reads its configuration from the environment after any file; CapyCTL \
     renders LMCache from the deployment's `lmcache` setting",
);

/// Every reserved family: the dump families and LMCache's.
fn reserved_prefixes() -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    DUMP_RESERVED_PREFIXES
        .iter()
        .chain(std::iter::once(&LMCACHE_RESERVED_PREFIX))
}

fn reserved(upper: &str) -> bool {
    DUMP_RESERVED_NAMES.iter().any(|(name, _)| *name == upper)
        || reserved_prefixes().any(|(prefix, _)| upper.starts_with(prefix))
}

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
pub(crate) const MAX_NAMES: usize = 64;
const MAX_NAME_BYTES: usize = 128;

/// Whether CapyCTL owns or reserves `name` (rule 1, the dump families and
/// LMCache's, above). Compared upper-cased, so a lower-case spelling of an
/// owned name is not a way around it.
pub fn is_owned(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if SAFE_NAMES.contains(&upper.as_str()) {
        return false;
    }
    OWNED_NAMES.contains(&upper.as_str())
        || OWNED_PREFIXES
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        || reserved(&upper)
}

/// ADR 0039 §5: the names CapyCTL renders into the native TensorFold line's
/// closed environment. All but the engine's memory reserve are owned on
/// every engine already ([`OWNED_NAMES`], the `CAPYCTL_` family); the reserve
/// is owned on the native line alone, so the Python line keeps its meaning.
pub const TENSORFOLD_NATIVE_RENDERED: &[&str] = &[
    "PATH",
    "HOME",
    "CUDA_VISIBLE_DEVICES",
    "CUDA_DEVICE_ORDER",
    "HF_HUB_OFFLINE",
    "TENSORFOLD_NO_UPDATE_CHECK",
    "TENSORFOLD_CUDA_MEMORY_LIMIT_GB",
    "TENSORFOLD_MEMORY_RESERVE_GIB",
    "CAPYCTL_ENGINE_LOG",
    "DO_NOT_TRACK",
];

/// ADR 0039 §5: why a native TensorFold profile may not name `name` in its
/// `env`, its `security.approved_env` or a deployment's `engine_config.env`,
/// as the closed reason `engine_env_reserved:<name>` with why: a name the
/// native line renders, or one of `tensorfold_native::REFUSED_ENV`, which
/// the native engine reads and CapyCTL never carries. `None` for any other
/// name. Compared upper-cased, as [`is_owned`] is.
pub fn tensorfold_native_refusal(name: &str) -> Option<String> {
    let upper = name.to_ascii_uppercase();
    if let Some(why) = crate::tensorfold_native::refused_env(&upper) {
        return Some(format!(
            "engine_env_reserved:{name}: TensorFold native never carries it: {why}"
        ));
    }
    TENSORFOLD_NATIVE_RENDERED
        .contains(&upper.as_str())
        .then(|| {
            format!("engine_env_reserved:{name}: CapyCTL renders it on the TensorFold native line")
        })
}

/// [`tensorfold_native_refusal`] of the first refused name among `names`.
/// An approval glob (`TF_*`) is not a name: the concrete names it admits are
/// checked where a deployment sets them.
pub fn tensorfold_native_refused<'a>(
    names: impl IntoIterator<Item = &'a String>,
) -> Option<String> {
    names
        .into_iter()
        .filter(|name| !name.ends_with('*'))
        .find_map(|name| tensorfold_native_refusal(name))
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
                OWNED_PREFIXES
                    .iter()
                    .chain(reserved_prefixes().map(|(prefix, _)| prefix))
                    .any(|prefix| stem.starts_with(prefix))
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
    /// ADR 0031 §2: a custom engine's manifest, below the profile.
    Manifest,
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

/// ADR 0028 §2.1 (R10): replace every engine environment value in a stored
/// effective configuration (or a view holding several) with [`REDACTED`],
/// keeping each name and source: a profile's or a declaration's `env`
/// object, and the resolved `engine_env.vars` (`[value, source]`).
pub fn redact_values(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Object(fields) => {
            for (name, field) in fields.iter_mut() {
                match (name.as_str(), &mut *field) {
                    ("env", Value::Object(env)) => {
                        for entry in env.values_mut() {
                            *entry = Value::String(REDACTED.into());
                        }
                    }
                    ("engine_env", Value::Object(resolved)) => {
                        if let Some(Value::Object(vars)) = resolved.get_mut("vars") {
                            for entry in vars.values_mut() {
                                match entry {
                                    Value::Array(pair) if !pair.is_empty() => {
                                        pair[0] = Value::String(REDACTED.into());
                                    }
                                    other => *other = Value::String(REDACTED.into()),
                                }
                            }
                        }
                    }
                    (_, field) => redact_values(field),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_values),
        _ => {}
    }
}

pub(crate) fn check_entry(name: &str, value: &str) -> Result<(), EnvRefusal> {
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

/// ADR 0031 §2: [`resolve_engine_env`] for a custom engine, whose manifest's
/// `env` is the lowest layer: the profile's names override it, and the
/// deployment's override both. The manifest's names were checked when it
/// validated; they are checked again here, at every resolution and so at
/// every launch, against the names CapyCTL owns now. Its values may hold
/// placeholders, which only the launch renders.
pub fn resolve_engine_env_with_manifest(
    manifest_env: &BTreeMap<String, String>,
    profile_env: &BTreeMap<String, String>,
    approved: &ApprovedEnv,
    deployment_env: &BTreeMap<String, String>,
) -> Result<ResolvedEnv, EnvRefusal> {
    let mut resolved = resolve_engine_env(profile_env, approved, deployment_env)?;
    for (name, value) in manifest_env {
        check_entry(name, value)?;
        resolved
            .vars
            .entry(name.clone())
            .or_insert_with(|| (value.clone(), EnvSource::Manifest));
    }
    Ok(resolved)
}

/// ADR 0028 §2.1, ADR 0021: every environment name and family the policy
/// names, with its class and why, built from the tables [`is_owned`] reads.
/// Names are compared upper-cased. A name not listed needs a matching
/// `security.approved_env` entry when a deployment sets it.
pub fn env_table() -> Vec<FlagRow> {
    let row = |name: &str, name_match, class, reason: &str| FlagRow {
        name: name.to_owned(),
        name_match,
        class,
        approval: None,
        value: None,
        reason: reason.to_owned(),
    };
    let owned = "CapyCTL renders or closes it";
    OWNED_PREFIXES
        .iter()
        .map(|prefix| row(prefix, NameMatch::Prefix, FlagClass::Owned, owned))
        .chain(
            OWNED_NAMES
                .iter()
                .map(|name| row(name, NameMatch::Name, FlagClass::Owned, owned)),
        )
        .chain(
            reserved_prefixes().map(|(prefix, reason)| {
                row(prefix, NameMatch::Prefix, FlagClass::Reserved, reason)
            }),
        )
        .chain(
            DUMP_RESERVED_NAMES
                .iter()
                .map(|(name, reason)| row(name, NameMatch::Name, FlagClass::Reserved, reason)),
        )
        .chain(SAFE_NAMES.iter().map(|name| {
            row(
                name,
                NameMatch::Name,
                FlagClass::Ordinary,
                "needs no approval",
            )
        }))
        .collect()
}

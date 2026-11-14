//! SPEC §6.3, ADR 0008, ADR 0040 §10: the model copies a host downloaded
//! into its store, listed with whether a deployment references each, and
//! pruned against a reference set the caller computes when the prune is
//! requested. Only verified copies under `<store>/sources` that no deployment
//! references are removed, never one whose download lock is held, and
//! without `apply` nothing is removed.
use super::{error, HostAdminError};
use crate::sources::{prune, PrunedSource};
use capyctl_config::model_settings::{default_models_root, resolve, ModelOverrides};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn invalid(e: capyctl_config::ConfigError) -> HostAdminError {
    error(
        "invalid_config",
        format!("Invalid host configuration: {}: {}", e.path, e.detail),
    )
}

/// The sources store a host document names (ADR 0008; owner ruling
/// 2026-09-25), resolved by the host role's rule
/// (`capyctl_config::model_settings`) with `env` as the environment's layer:
/// `model_sources.path`, else the models directory (`model_store.path`, else
/// `CAPYCTL_MODELS_ROOT`, else `~/models`), under which downloads live in
/// `sources/`.
pub fn model_store(host_config: &Path, env: &ModelOverrides) -> Result<PathBuf, HostAdminError> {
    let text = std::fs::read_to_string(host_config)
        .map_err(|_| error("invalid_config", "Cannot read the host configuration"))?;
    let host = capyctl_config::parse_strict(capyctl_config::ConfigKind::Host, &text)
        .map_err(|e| error("invalid_config", format!("Invalid host configuration: {e}")))?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let settings = resolve(
        &host,
        &ModelOverrides::default(),
        env,
        default_models_root(home.as_deref()).as_deref(),
    )
    .map_err(invalid)?;
    Ok(settings.policy.root(&settings.models_root).to_path_buf())
}

fn unreadable() -> HostAdminError {
    error(
        "internal",
        "Cannot read or change the model store's sources",
    )
}

/// ADR 0040 §10: the verified copies in `store`, each marked `referenced`
/// when a key of `referenced` names it. A marker capyctl would never prune
/// (a key outside `sources/`) is not a copy and is left out.
pub fn list_copies(store: &Path, referenced: &BTreeSet<String>) -> Result<Value, HostAdminError> {
    let report = prune(store, referenced, false).map_err(|_| unreadable())?;
    let row = |copy: &PrunedSource, referenced: bool| json!({"key": copy.key, "bytes": copy.bytes, "referenced": referenced});
    let mut copies: Vec<(String, Value)> = report
        .kept
        .iter()
        .map(|copy| (copy.key.clone(), row(copy, true)))
        .chain(
            report
                .removed
                .iter()
                .map(|copy| (copy.key.clone(), row(copy, false))),
        )
        .collect();
    copies.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(json!({
        "model_store": store,
        "copies": copies.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
    }))
}

/// SPEC §6.3: prune `store` against `referenced`; without `apply`, the
/// copies that would be removed.
pub fn prune_copies(
    store: &Path,
    referenced: &BTreeSet<String>,
    apply: bool,
) -> Result<Value, HostAdminError> {
    let report = prune(store, referenced, apply).map_err(|_| unreadable())?;
    let bytes = |items: &[PrunedSource]| items.iter().map(|s| s.bytes).sum::<u64>();
    Ok(json!({
        "model_store": store,
        "applied": report.applied,
        // Without `apply` these are the copies that would be removed.
        "removed": report.removed,
        "removed_bytes": bytes(&report.removed),
        "kept": report.kept,
        "skipped": report.skipped,
    }))
}

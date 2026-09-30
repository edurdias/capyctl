//! `capyctl prune sources --host-config <host.yaml> [--apply]`.
//!
//! SPEC §6.3: deleting a deployment never deletes user-owned checkpoints or
//! cache files implicitly, and ADR 0008 materialized sources are shared by
//! every deployment that declares the same pinned source. Reclaiming one is
//! therefore an explicit, host-side operation: this command reads the host
//! document for its model store, asks the server which store keys existing
//! deployments still reference (or reads that answer from a file), and
//! removes only verified copies under `<store>/sources` that none does. It
//! lists them unless `--apply` is given, and never touches a copy whose
//! download lock is held or anything outside `sources/`. Without a referenced
//! set it removes nothing.
use crate::output::StructuredError;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

fn error(code: &'static str, message: impl Into<String>) -> StructuredError {
    StructuredError {
        code,
        message: message.into(),
    }
}

/// The sources store a host document names (ADR 0008; owner ruling
/// 2026-09-25), resolved by the host role's rule
/// (`capyctl_config::model_settings`): `model_sources.path`, else the models
/// directory (`model_store.path`, else `CAPYCTL_MODELS_ROOT`, else `~/models`),
/// under which downloads live in `sources/`.
fn model_store(host_config: &Path) -> Result<std::path::PathBuf, StructuredError> {
    use capyctl_config::model_settings::{default_models_root, resolve, ModelOverrides};
    let text = std::fs::read_to_string(host_config)
        .map_err(|_| error("invalid_config", "Cannot read the host configuration"))?;
    let host = capyctl_config::parse_strict(capyctl_config::ConfigKind::Host, &text)
        .map_err(|e| error("invalid_config", format!("Invalid host configuration: {e}")))?;
    let invalid = |e: capyctl_config::ConfigError| {
        error(
            "invalid_config",
            format!("Invalid host configuration: {}: {}", e.path, e.detail),
        )
    };
    let env = ModelOverrides::from_process_env().map_err(invalid)?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let settings = resolve(
        &host,
        &ModelOverrides::default(),
        &env,
        default_models_root(home.as_deref()).as_deref(),
    )
    .map_err(invalid)?;
    Ok(settings.policy.root(&settings.models_root).to_path_buf())
}

/// The referenced store keys, from a `GET /management/v1/model-sources` body.
pub fn referenced_keys(body: &Value) -> Result<BTreeSet<String>, StructuredError> {
    let invalid = || error("internal", "Invalid referenced model sources");
    body["referenced"]
        .as_array()
        .ok_or_else(invalid)?
        .iter()
        .map(|key| {
            key.as_str()
                .filter(|key| key.starts_with("sources/"))
                .map(str::to_owned)
                .ok_or_else(invalid)
        })
        .collect()
}

/// Prune the host's model store against a referenced set.
pub fn prune_store(
    host_config: &Path,
    referenced: &BTreeSet<String>,
    apply: bool,
) -> Result<Value, StructuredError> {
    let store = model_store(host_config)?;
    let report = capyctl_agent::sources::prune(&store, referenced, apply).map_err(|_| {
        error(
            "internal",
            "Cannot read or change the model store's sources",
        )
    })?;
    let bytes =
        |items: &[capyctl_agent::sources::PrunedSource]| items.iter().map(|s| s.bytes).sum::<u64>();
    Ok(json!({
        "model_store": store,
        "applied": report.applied,
        // Without --apply these are the copies that would be removed.
        "removed": report.removed,
        "removed_bytes": bytes(&report.removed),
        "kept": report.kept,
        "skipped": report.skipped,
    }))
}

pub async fn execute(
    host_config: &Path,
    apply: bool,
    referenced_file: Option<&Path>,
    state_dir: &Path,
    config: Option<&Path>,
) -> Result<Value, StructuredError> {
    let body = match referenced_file {
        Some(file) => {
            let text = std::fs::read_to_string(file)
                .map_err(|_| error("invalid_config", "Cannot read the referenced-sources file"))?;
            serde_json::from_str(&text)
                .map_err(|_| error("invalid_config", "The referenced-sources file is not JSON"))?
        }
        None => {
            // Fail closed: with no answer from the server nothing is removed.
            let target = crate::local_role::resolve(state_dir, config)?;
            crate::remote_roles::management_call(
                &target.endpoint,
                &target.token,
                reqwest::Method::GET,
                "/model-sources",
                None,
            )
            .await
            .map_err(|_| {
                error(
                    "management_unavailable",
                    "Cannot learn which sources deployments reference; nothing was removed \
                     (pass --referenced-file with the server's /management/v1/model-sources answer)",
                )
            })?
        }
    };
    let referenced = referenced_keys(&body)?;
    prune_store(host_config, &referenced, apply)
}

#[cfg(test)]
mod tests {
    use super::*;

    // SPEC §6.3 (ADR 0008): the CLI prunes only what the referenced set does
    // not name, lists without --apply, and refuses a malformed referenced set.
    #[test]
    fn prune_sources_lists_then_removes_only_unreferenced_copies() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("models");
        let state = store.join("sources/.capyctl");
        std::fs::create_dir_all(&state).unwrap();
        let keys = ["sources/http/aaa", "sources/http/bbb"];
        for key in keys {
            std::fs::create_dir_all(store.join(key)).unwrap();
            std::fs::write(store.join(key).join("w.bin"), b"123").unwrap();
            let id = {
                use sha2::Digest;
                let digest = sha2::Sha256::digest(key.as_bytes());
                digest[..12]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            };
            std::fs::write(
                state.join(format!("{id}.verified")),
                json!({"key": key, "state": "verified", "bytes": 3, "files": 1}).to_string(),
            )
            .unwrap();
        }
        let host = root.path().join("host.yaml");
        std::fs::write(
            &host,
            format!(
                "schema_version: 1\nkind: host\nname: h\nmodel_store:\n  path: {}\n",
                store.display()
            ),
        )
        .unwrap();
        let referenced = referenced_keys(&json!({"referenced": ["sources/http/aaa"]})).unwrap();
        let listed = prune_store(&host, &referenced, false).unwrap();
        assert_eq!(listed["removed"][0]["key"], "sources/http/bbb");
        assert!(
            store.join("sources/http/bbb").is_dir(),
            "listing removes nothing"
        );
        let applied = prune_store(&host, &referenced, true).unwrap();
        assert_eq!(applied["removed_bytes"], 3);
        assert!(!store.join("sources/http/bbb").exists());
        assert!(store.join("sources/http/aaa").is_dir());
        // Owner ruling 2026-09-25: with no `model_sources.path`, downloads
        // live under the models directory; a stated path wins.
        let with_store = root.path().join("with-store.yaml");
        std::fs::write(
            &with_store,
            format!(
                "schema_version: 1\nkind: host\nname: h\nstate_dir: {}\nmodel_store:\n  path: {}\n",
                root.path().join("state").display(),
                root.path().join("models").display()
            ),
        )
        .unwrap();
        assert_eq!(
            model_store(&with_store).unwrap(),
            root.path().join("models")
        );
        let stated = root.path().join("stated.yaml");
        std::fs::write(
            &stated,
            "schema_version: 1\nkind: host\nname: h\nstate_dir: /s\nmodel_sources:\n  path: /dl\n",
        )
        .unwrap();
        assert_eq!(model_store(&stated).unwrap(), Path::new("/dl"));
        assert!(referenced_keys(&json!({"referenced": ["/etc"]})).is_err());
        assert!(referenced_keys(&json!({})).is_err());
    }
}

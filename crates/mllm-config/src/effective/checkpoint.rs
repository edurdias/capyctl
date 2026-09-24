//! ADR 0014 §7 (WE3): checkpoint identity inputs that do not need the
//! checkpoint's own facts.
//!
//! A host digests a checkpoint before the deployment's memory request can be
//! derived from it, so locating the checkpoint must not depend on full
//! resolution. Re-resolving a frozen snapshot with the recorded facts is what
//! lets the server and the host resolve the same revision identically.

use super::core::normalize_model;
use super::snapshot::{declared_engine_config, snapshot_inputs};
use super::*;
use serde_json::Value;

/// Where one deployment's checkpoint lives on one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointLocation {
    /// The host's model store, as the host document states it (absolute).
    pub model_store: PathBuf,
    /// The checkpoint directory, as the deployment's source resolves against
    /// the store. Containment is checked by whoever opens it, on the opened
    /// descriptors rather than on this text.
    pub checkpoint: PathBuf,
    /// The deployment's declared `model.content_fingerprint`.
    pub content_fingerprint: String,
}

/// ADR 0014 §7: locate a deployment's checkpoint from its `model` block and the
/// host's model store only. Only a local source names a directory; any other
/// source is `NotMaterializable` here exactly as at launch.
pub fn checkpoint_location(
    deployment: &Value,
    host: &Value,
) -> Result<CheckpointLocation, ConfigError> {
    let raw: RawModel = decode(&deployment["model"], "deployment.model")?;
    let host: HostInput = decode_host(host)?;
    let store = PathBuf::from(host.model_store.path);
    if !store.is_absolute() {
        return Err(invalid("host.model_store.path", "must be absolute"));
    }
    let model = normalize_model(raw, Some(&store))?;
    let checkpoint = PathBuf::from(model.require_resolved_path()?);
    Ok(CheckpointLocation {
        model_store: store,
        checkpoint,
        content_fingerprint: model.content_fingerprint,
    })
}

/// The prefix of every checkpoint digest (ADR 0014 §7).
pub const CHECKPOINT_DIGEST_PREFIX: &str = "sha256:";

/// Whether `value` is a canonical checkpoint digest: `sha256:` and 64 lowercase
/// hexadecimal characters.
pub fn is_checkpoint_digest(value: &str) -> bool {
    value
        .strip_prefix(CHECKPOINT_DIGEST_PREFIX)
        .is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

/// ADR 0014 §7: a declared `model.content_fingerprint` is the expected digest
/// only when it has the canonical digest form. Any other value is a label the
/// deployment was written with (for example the standalone `sha256:<name>`),
/// which expects nothing; the digest a host computes is then recorded as is.
pub fn declared_checkpoint_digest(content_fingerprint: &str) -> Option<&str> {
    is_checkpoint_digest(content_fingerprint).then_some(content_fingerprint)
}

/// ADR 0014 §5, §7: re-resolve a frozen, exact snapshot with checkpoint facts
/// the digest has since supplied. Values the snapshot derived (a memory request
/// or KV cache, and derived resource phases) are derived again from the new
/// facts; everything the deployment declared is kept. The stored snapshot must
/// first decode exactly, so nothing unvalidated is carried forward.
pub fn resolve_snapshot_with_checkpoint(
    text: &str,
    facts: CheckpointFacts,
) -> Result<EffectiveDeployment, ConfigError> {
    decode_effective_snapshot(text)?;
    let value = crate::strict_yaml::build_value(text)?;
    let (engine_config, resources_derived, _) = declared_engine_config(&value["engine_config"])?;
    let (deployment, host) = snapshot_inputs(&value, engine_config, resources_derived)?;
    resolve_effective_with_checkpoint(&deployment, &host, facts)
}

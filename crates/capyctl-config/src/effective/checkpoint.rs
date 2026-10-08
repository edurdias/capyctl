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
    /// The root the checkpoint must be inside (absolute): the host's model
    /// store for a local source, its sources store for a remote one (ADR
    /// 0008, owner decision 2026-09-25).
    pub model_store: PathBuf,
    /// The checkpoint directory, as the deployment's source resolves against
    /// the store. Containment is checked by whoever opens it, on the opened
    /// descriptors rather than on this text.
    pub checkpoint: PathBuf,
    /// The deployment's declared `model.content_fingerprint`.
    pub content_fingerprint: String,
    /// ADR 0014 §5 amendment A6: the draft model the launch loads beside the
    /// checkpoint, whose weights are counted with the checkpoint's.
    pub drafter: Option<DrafterLocation>,
    /// ADR 0014 amendment A16: an SGLang launch's arguments (the
    /// installation's and the deployment's), which size the hybrid state slot
    /// measured beside the weights; `None` for another engine.
    pub sglang_args: Option<Vec<String>>,
}

impl CheckpointLocation {
    /// ADR 0014 amendment A16: one request slot of the hybrid state an SGLang
    /// launch of this checkpoint keeps, from its `config.json`; `None` for any
    /// other model or engine.
    pub fn state_slot_bytes(&self) -> Option<i64> {
        crate::context_fit::sglang_state_slot_bytes(&self.checkpoint, self.sglang_args.as_ref()?)
    }

    /// ADR 0028 §5 (amendment of 2026-10-07): how the checkpoint's weights
    /// split across a group's ranks, from its safetensors headers; `None`
    /// when it has none or one cannot be read.
    pub fn layout(&self) -> Option<crate::checkpoint_layout::CheckpointLayout> {
        crate::checkpoint_layout::read_checkpoint_layout(&self.checkpoint)
    }
}

/// ADR 0014 §5 amendment A6: a draft model directory and the approved root
/// (`security.approved_paths`) it lies in. Containment is checked again by
/// whoever opens it, on the opened descriptors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrafterLocation {
    pub root: PathBuf,
    pub path: PathBuf,
}

/// ADR 0014 §5 amendment A6: where the draft model the arguments name lies,
/// when it lies inside an approved root. Resolution refuses a draft model
/// outside them, so `None` then only means nothing is counted for it.
pub fn drafter_location(
    engine: Engine,
    profile_args: &[String],
    extra_args: &[String],
    approved_paths: &[String],
) -> Option<DrafterLocation> {
    let args: Vec<String> = profile_args.iter().chain(extra_args).cloned().collect();
    let path = crate::engine_policy::draft_model_path(engine, &args)?;
    let root = approved_paths
        .iter()
        .find(|root| crate::engine_policy::path_within(&path, Path::new(root)))?;
    Some(DrafterLocation {
        root: root.into(),
        path: path.into(),
    })
}

impl EffectiveDeployment {
    /// ADR 0014 §5 amendment A6: the draft model this launch loads, if any.
    pub fn drafter_location(&self) -> Option<DrafterLocation> {
        drafter_location(
            self.profile.engine,
            &self.profile.args,
            self.engine_config.extra_args(),
            &self.profile.security.approved_paths,
        )
    }

    /// ADR 0014 amendment A16: one request slot of the hybrid state an SGLang
    /// launch of this deployment keeps, read from its checkpoint on this
    /// machine; `None` for any other model or engine.
    pub fn state_slot_bytes(&self) -> Option<i64> {
        if self.profile.engine != Engine::Sglang {
            return None;
        }
        let args: Vec<String> = self
            .profile
            .args
            .iter()
            .chain(self.engine_config.extra_args())
            .cloned()
            .collect();
        crate::context_fit::sglang_state_slot_bytes(
            Path::new(self.model.resolved_path.as_deref()?),
            &args,
        )
    }

    /// ADR 0028 §5 (amendment of 2026-10-07): the layout of this
    /// deployment's checkpoint, read on this machine.
    pub fn checkpoint_layout(&self) -> Option<crate::checkpoint_layout::CheckpointLayout> {
        crate::checkpoint_layout::read_checkpoint_layout(Path::new(
            self.model.resolved_path.as_deref()?,
        ))
    }
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
    let sources = crate::model_source::ModelSourcePolicy::from_raw(host.model_sources)?;
    let profile = host
        .runtime_profiles
        .get(deployment["runtime_profile"].as_str().unwrap_or_default());
    let extra: Vec<String> = deployment["engine_config"]["extra_args"]
        .as_array()
        .map(|args| {
            args.iter()
                .filter_map(|arg| arg.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let drafter = profile.and_then(|profile| {
        drafter_location(
            profile.engine,
            &profile.args,
            &extra,
            &profile.security.approved_paths,
        )
    });
    let sglang_args = profile
        .filter(|profile| profile.engine == Engine::Sglang)
        .map(|profile| profile.args.iter().chain(&extra).cloned().collect());
    let model = normalize_model(raw, Some(&store), Some(sources.root(&store)))?;
    let checkpoint = PathBuf::from(model.require_resolved_path()?);
    let root = match model.source {
        ModelSource::Local { .. } => store,
        _ => sources.root(&store).to_path_buf(),
    };
    Ok(CheckpointLocation {
        model_store: root,
        checkpoint,
        content_fingerprint: model.content_fingerprint,
        drafter,
        sglang_args,
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
/// first decode exactly, so nothing unvalidated is carried forward. A group
/// member's snapshot keeps the topology its share is taken of (ADR 0028 §5).
pub fn resolve_snapshot_with_checkpoint(
    text: &str,
    facts: CheckpointFacts,
) -> Result<EffectiveDeployment, ConfigError> {
    decode_effective_snapshot(text)?;
    let value = crate::strict_yaml::build_value(text)?;
    let (engine_config, resources_derived, frozen) =
        declared_engine_config(&value["engine_config"])?;
    let (deployment, host) = snapshot_inputs(&value, engine_config, resources_derived)?;
    resolve_effective_with_checkpoint(
        &deployment,
        &host,
        CheckpointFacts {
            member_of: frozen.member_of,
            ..facts
        },
    )
}

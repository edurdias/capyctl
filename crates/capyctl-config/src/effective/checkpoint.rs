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
    /// ADR 0014 §7 (amendment of 2026-10-08): the declaration a host may
    /// trust without a full read when its own policy allows it
    /// ([`ModelIdentity::trustable_declaration`]): a local source's canonical
    /// `content_fingerprint`, else `None`.
    pub trustable_declaration: Option<String>,
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

    /// ADR 0028 §5 (amendment of 2026-10-07) and ADR 0014 amendment A20: how
    /// the checkpoint's weights split across a group's ranks, and the tables
    /// an engine option can keep on disk, from one read of its safetensors
    /// headers; each `None` when it has none or a header cannot be read.
    pub fn header_facts(
        &self,
    ) -> (
        Option<crate::checkpoint_layout::CheckpointLayout>,
        Option<crate::checkpoint_layout::CheckpointTables>,
    ) {
        crate::checkpoint_layout::read_header_facts(&self.checkpoint)
    }
}

/// ADR 0014 §5 amendment A6: a draft model directory and the root it lies
/// in: an approved root (`security.approved_paths`) for a drafter the
/// arguments name, the model or sources store for a declared one.
/// Containment is checked again by whoever opens it, on the opened
/// descriptors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrafterLocation {
    pub root: PathBuf,
    pub path: PathBuf,
    /// ADR 0008 amendment 2026-10-08: a declared local drafter may lie
    /// outside `root`, inside its own root, exactly as a local
    /// `model.source` may (found live 2026-10-03); any other drafter never.
    pub outside_root_allowed: bool,
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
        outside_root_allowed: false,
    })
}

/// ADR 0008 amendment 2026-10-08: a declared drafter, in the store its
/// source resolved into (`model_store` for a local source, `sources` for a
/// remote one, which CapyCTL materialized and verified there). ADR 0014 §5
/// amendment A6 counts its weights with the checkpoint's. No
/// `approved_paths` or `approved_options` are involved: the operator named
/// no path, CapyCTL chose the directory.
fn declared_drafter(
    draft: &DraftModel,
    model_store: &Path,
    sources: &Path,
) -> Option<DrafterLocation> {
    let local = matches!(draft.source, ModelSource::Local { .. });
    Some(DrafterLocation {
        root: if local { model_store } else { sources }.to_path_buf(),
        path: PathBuf::from(draft.resolved_path.as_deref()?),
        outside_root_allowed: local,
    })
}

impl EffectiveDeployment {
    /// ADR 0014 §5 amendment A6: the draft model this launch loads, if any:
    /// the declared drafter (ADR 0008 amendment 2026-10-08), else one the
    /// arguments name inside an approved root. Resolution refuses both.
    pub fn drafter_location(&self) -> Option<DrafterLocation> {
        match &self.model.draft {
            Some(draft) => declared_drafter(
                draft,
                &self.host.model_store,
                self.host.model_sources.root(&self.host.model_store),
            ),
            None => drafter_location(
                self.profile.engine,
                &self.profile.args,
                self.engine_config.extra_args(),
                &self.profile.security.approved_paths,
            ),
        }
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
        self.checkpoint_header_facts().0
    }

    /// ADR 0014 amendment A20: the tables of this deployment's checkpoint,
    /// read on this machine.
    pub fn checkpoint_tables(&self) -> Option<crate::checkpoint_layout::CheckpointTables> {
        self.checkpoint_header_facts().1
    }

    /// The layout and the tables from one read of the checkpoint's headers.
    pub fn checkpoint_header_facts(
        &self,
    ) -> (
        Option<crate::checkpoint_layout::CheckpointLayout>,
        Option<crate::checkpoint_layout::CheckpointTables>,
    ) {
        match self.model.resolved_path.as_deref() {
            Some(path) => crate::checkpoint_layout::read_header_facts(Path::new(path)),
            None => (None, None),
        }
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
    let sglang_args = profile
        .filter(|profile| profile.engine == Engine::Sglang)
        .map(|profile| profile.args.iter().chain(&extra).cloned().collect());
    let model = normalize_model(raw, Some(&store), Some(sources.root(&store)))?;
    let drafter = match &model.draft {
        Some(draft) => declared_drafter(draft, &store, sources.root(&store)),
        None => profile.and_then(|profile| {
            drafter_location(
                profile.engine,
                &profile.args,
                &extra,
                &profile.security.approved_paths,
            )
        }),
    };
    let checkpoint = PathBuf::from(model.require_resolved_path()?);
    let root = match model.source {
        ModelSource::Local { .. } => store,
        _ => sources.root(&store).to_path_buf(),
    };
    Ok(CheckpointLocation {
        model_store: root,
        checkpoint,
        trustable_declaration: model.trustable_declaration().map(str::to_owned),
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

/// ADR 0014 §7 (amendment of 2026-10-08): where the per-file hashes behind a
/// recorded checkpoint digest came from. Every form names the same canonical
/// manifest digest; only how much of it the host read differs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestProvenance {
    /// The host hashed every file (the WE3 first placement).
    #[default]
    Measured,
    /// CapyCTL downloaded the checkpoint and verified every file against its
    /// pin as it was written; the manifest was built from those verified
    /// hashes, with nothing read a second time.
    Fetched,
    /// The deployment's declared `content_fingerprint`, trusted without a full
    /// read because the host's policy allows it
    /// (`checkpoints.trust_declared_digest`). Its bytes were never measured.
    DeclaredTrusted,
}

impl DigestProvenance {
    /// The closed wire and status name.
    pub fn code(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Fetched => "fetched",
            Self::DeclaredTrusted => "declared_trusted",
        }
    }

    /// The provenance a wire or stored name names. An empty name (a host
    /// from before provenance was reported) is `measured`, the only form it
    /// could produce.
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "" | "measured" => Some(Self::Measured),
            "fetched" => Some(Self::Fetched),
            "declared_trusted" => Some(Self::DeclaredTrusted),
            _ => None,
        }
    }
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

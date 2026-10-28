//! ADR 0036 §1, §2, §7: what a deployment that serves embedding models
//! (`api: embeddings`) may resolve to. Every refusal here happens before
//! anything is stored or launched.
//!
//! - Every engine that has an embeddings route takes one, at every version:
//!   vLLM, SGLang, llama.cpp and custom engines whose manifest says
//!   `api: embeddings`. The versions qualified live are shown `certified`, any
//!   other runs as an uncertified custom setup and is never refused for it
//!   (owner decision 2026-10-27, question 1). TensorFold has no embeddings
//!   route and is refused `capability_missing:embeddings`.
//! - One GPU on one machine (§7.1): a deployment declaring a `topology` is
//!   refused.
//! - Parking (§7.2): vLLM and SGLang park `deep` while their support data
//!   says embeddings parking passed (row EM9); with no stated residency they
//!   resolve `deep`, and `host_backed` is refused. llama.cpp and custom
//!   engines resolve `restart_only` and refuse `deep`.
//! - No parser (ADR 0024 applies to chat), checked by
//!   `decisions::check_parsers`; LMCache off (`lmcache.rs`).

use serde_json::Value;

use crate::decisions::EngineVersion;
use crate::effective::Residency;
use crate::engine_policy::Engine;
use crate::{ConfigError, ConfigErrorCode};

/// ADR 0036 §1: the host capability (ADR 0017) a host declares when it serves
/// embedding models. `capyctl_protocol::capabilities::EMBEDDINGS` is this name.
pub const HOST_CAPABILITY: &str = "embeddings";
/// The closed code of an installation that cannot serve embedding models.
pub const CAPABILITY_MISSING: &str = "capability_missing:embeddings";
/// ADR 0036 §2: why LMCache is off for an embeddings deployment.
pub const LMCACHE_OFF_REASON: &str = "embeddings_unsupported";

/// ADR 0036 §1 (owner decision 2026-10-27, question 1): the vLLM release
/// certified for embeddings.
pub const VLLM_CERTIFIED: EngineVersion = EngineVersion::new(0, 32, 0);
/// The SGLang release certified for embeddings.
pub const SGLANG_CERTIFIED: EngineVersion = EngineVersion::new(0, 5, 22);
/// The llama.cpp release (v0.7.0, build b11669) certified for embeddings.
pub const LLAMACPP_CERTIFIED: EngineVersion = EngineVersion::new(0, 7, 0);

fn invalid(path: &str, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

/// ADR 0036 §1: whether an engine version is one qualified live. Status only:
/// an uncertified version is never refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Certification {
    Certified,
    Uncertified,
}

impl Certification {
    /// The word status shows.
    pub fn name(self) -> &'static str {
        match self {
            Self::Certified => "certified",
            Self::Uncertified => "uncertified",
        }
    }
}

/// ADR 0036 §1: the certification of `engine` at the build its
/// `build_fingerprint` names. Only the certified release itself is
/// certified (not a pre-release, an older or a newer one, nor a fingerprint
/// that names no version); custom engines and TensorFold never are.
pub fn certification(engine: Engine, build_fingerprint: &str) -> Certification {
    let certified = match engine {
        Engine::Vllm => VLLM_CERTIFIED,
        Engine::Sglang => SGLANG_CERTIFIED,
        Engine::Llamacpp => LLAMACPP_CERTIFIED,
        Engine::Tensorfold | Engine::Custom => return Certification::Uncertified,
    };
    match EngineVersion::parse(build_fingerprint.trim()) {
        Some(version) if version == certified => Certification::Certified,
        _ => Certification::Uncertified,
    }
}

/// ADR 0036 §1: why `engine` cannot serve embedding models, or `None` when it
/// can. Only TensorFold is refused: it has no embeddings route. The version
/// is never a reason (owner decision 2026-10-27).
pub fn engine_refusal(engine: Engine) -> Option<String> {
    (engine == Engine::Tensorfold).then(|| {
        format!(
            "{CAPABILITY_MISSING}: TensorFold has no embeddings route to forward to; serve \
             embedding models on vLLM, SGLang, llama.cpp or a custom engine"
        )
    })
}

/// [`engine_refusal`] as the configuration error resolution returns, at `api`.
pub fn check_engine(engine: Engine) -> Result<(), ConfigError> {
    match engine_refusal(engine) {
        Some(reason) => Err(invalid("api", reason)),
        None => Ok(()),
    }
}

/// ADR 0036 §7.2: an engine's support data for embeddings deployments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingSupport {
    /// Embeddings deployments park `deep`. Turned off for an engine whose
    /// park-and-wake row (EM9) fails: its deployments then resolve
    /// `restart_only` and a stated `deep` is refused.
    pub deep_park: bool,
}

/// ADR 0036 §7.2: the support data of each engine that can park an
/// embeddings deployment; `None` for one that never parks (llama.cpp,
/// custom engines, TensorFold).
pub fn support(engine: Engine) -> Option<EmbeddingSupport> {
    match engine {
        // Owner decision 2026-10-27, question 2: deep parking in 0.2.0,
        // qualified per engine by row EM9.
        Engine::Vllm | Engine::Sglang => Some(EmbeddingSupport { deep_park: true }),
        Engine::Llamacpp | Engine::Custom | Engine::Tensorfold => None,
    }
}

/// ADR 0036 §7.2: the residency of an embeddings deployment that states
/// none: `deep` where the engine's support data allows it and the profile
/// permits deep parking (on a discrete host too), `restart_only` elsewhere.
pub fn default_residency(support: Option<EmbeddingSupport>, profile_deep_park: bool) -> Residency {
    if profile_deep_park && support.is_some_and(|s| s.deep_park) {
        Residency::Deep
    } else {
        Residency::RestartOnly
    }
}

/// ADR 0036 §7.2: refuse a stated residency an embeddings deployment on
/// `engine`, with this support data, cannot take: `host_backed` everywhere,
/// and `deep` where the engine does not park embeddings.
pub fn check_residency(
    engine: Engine,
    stated: Option<Residency>,
    support: Option<EmbeddingSupport>,
) -> Result<(), ConfigError> {
    match stated {
        Some(Residency::HostBacked) => Err(host_backed_refusal()),
        Some(Residency::Deep) if !support.is_some_and(|s| s.deep_park) => Err(invalid(
            "residency",
            format!(
                "capability_missing:deep_park: an embeddings deployment on {} does not park \
                 in this version, so residency `deep` is refused; declare `restart_only` or \
                 leave it out",
                engine.name()
            ),
        )),
        _ => Ok(()),
    }
}

fn host_backed_refusal() -> ConfigError {
    invalid(
        "residency",
        "an embeddings deployment parks `deep` or restarts in this version, so residency \
         `host_backed` is refused; declare `deep` or `restart_only`, or leave it out",
    )
}

/// ADR 0036 §7: the refusals that depend on an embeddings deployment's
/// document alone: a `topology`, and residency `host_backed`. Called by
/// `decisions::check_document`, which every path validating a document's
/// shape runs.
pub fn check_document(deployment: &Value) -> Result<(), ConfigError> {
    if deployment.get("topology").is_some_and(|t| !t.is_null()) {
        return Err(invalid(
            "topology",
            "an embeddings deployment runs on one GPU of one machine in this version; remove \
             `topology`",
        ));
    }
    if deployment.get("residency").and_then(Value::as_str) == Some("host_backed") {
        return Err(host_backed_refusal());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // T47: only the certified release itself is certified.
    #[test]
    fn certification_is_the_qualified_release_only() {
        use Certification::*;
        assert_eq!(certification(Engine::Vllm, "0.32.0"), Certified);
        assert_eq!(certification(Engine::Vllm, "0.31.0"), Uncertified);
        assert_eq!(certification(Engine::Vllm, "0.32.0rc1"), Uncertified);
        assert_eq!(certification(Engine::Sglang, "0.5.22"), Certified);
        assert_eq!(certification(Engine::Sglang, "0.5.21"), Uncertified);
        assert_eq!(certification(Engine::Llamacpp, "0.7.0+620d222"), Certified);
        assert_eq!(certification(Engine::Llamacpp, "0.6.9+abc"), Uncertified);
        assert_eq!(certification(Engine::Llamacpp, "unknown"), Uncertified);
        assert_eq!(certification(Engine::Custom, "0.32.0"), Uncertified);
        assert_eq!(Certified.name(), "certified");
    }
}

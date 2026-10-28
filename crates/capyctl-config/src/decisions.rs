//! ADR 0030 §1, §7: what a deployment that serves decision models
//! (`api: decisions`) may resolve to. Every rule here refuses before anything
//! is stored or launched.
//!
//! - Only an engine installation that serves `POST /v1/systemone` takes one:
//!   llama.cpp 0.7.0 or newer and SGLang 0.5.22 or newer. vLLM is refused on
//!   every version (ADR 0030 §6, amended 2026-10-21: 0.32.0 serves the
//!   endpoint but reads Clef and Clef-Flash from their text output, not their
//!   decision head), and so is TensorFold (its 1.0 binary is not integrated).
//!   Every refusal is `capability_missing:decisions`.
//! - The version is read from the installation's `build_fingerprint`, the
//!   one fact about the build that every resolution of the deployment has:
//!   the server's, against the profile the host published, and the host's
//!   own at launch, against the same profile (ADR 0018 §1: `engine add`
//!   records the version; ADR 0029 §2: llama.cpp's is `<version>+<commit>`).
//!   A fingerprint that names no version (one an operator set by hand, or what
//!   a role's own interpreter printed) is refused: the capability cannot be
//!   shown, so it fails closed.
//! - One machine only (§7.1): a deployment declaring a `topology` is refused.
//! - Restart instead of park (§7.2): an undeclared residency resolves
//!   `restart_only`; a declared `deep` or `host_backed` is refused.
//! - No parser (ADR 0024 applies to chat deployments): parser auto-selection
//!   yields none, and a parser the deployment states, in its engine
//!   configuration or its extra arguments, is refused.

use std::fmt;

use capyctl_domain::launch::LaunchSettings;
use serde_json::Value;

use crate::effective::DeploymentApi;
use crate::engine_policy::Engine;
use crate::{ConfigError, ConfigErrorCode};

/// ADR 0030 §1: the host capability (ADR 0017) a host declares when it serves
/// decision models. `capyctl_protocol::capabilities::DECISIONS` is this name.
pub const HOST_CAPABILITY: &str = "decisions";
/// The closed code of an installation that cannot serve decision models.
pub const CAPABILITY_MISSING: &str = "capability_missing:decisions";

/// The first llama.cpp release that serves `/v1/systemone` (ADR 0030 §6).
pub const LLAMACPP_MINIMUM: EngineVersion = EngineVersion::new(0, 7, 0);
/// The first SGLang release that serves `/v1/systemone` for the decision
/// models of this release (ADR 0030 §6).
pub const SGLANG_MINIMUM: EngineVersion = EngineVersion::new(0, 5, 22);

fn invalid(path: &str, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

/// An engine release, `major.minor.patch`, and whether the text named a
/// pre-release of it (`0.5.22rc1`, `0.7.0.dev3`), which comes before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre_release: bool,
}

impl EngineVersion {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            pre_release: false,
        }
    }

    /// The version a build fingerprint names: `<major>.<minor>.<patch>`, then
    /// optionally a pre-release or post-release suffix (`rc1`, `.post1`,
    /// `.dev0`) and a local part after `+` (llama.cpp's commit, a custom
    /// build's label). `None` for anything else, a whole line of text
    /// included: only a fingerprint that is a version names one.
    pub fn parse(text: &str) -> Option<Self> {
        let release = text.split_once('+').map_or(text, |(release, _)| release);
        let mut parts = release.split('.');
        let number = |part: Option<&str>| -> Option<u64> {
            let part = part?;
            (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse().ok())
                .flatten()
        };
        let major = number(parts.next())?;
        let minor = number(parts.next())?;
        let patch_text = parts.next()?;
        let digits = patch_text.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let patch = patch_text[..digits].parse().ok()?;
        let mut pre_release = suffix_is_pre_release(&patch_text[digits..])?;
        for part in parts {
            pre_release |= suffix_is_pre_release(part)?;
        }
        Some(Self {
            major,
            minor,
            patch,
            pre_release,
        })
    }

    /// Whether this release is `minimum` or newer. A pre-release of the
    /// minimum is older than it.
    pub fn at_least(&self, minimum: &Self) -> bool {
        let release = (self.major, self.minor, self.patch);
        let floor = (minimum.major, minimum.minor, minimum.patch);
        release > floor || (release == floor && !self.pre_release)
    }
}

/// Whether one version suffix marks a pre-release: empty, digits and `post`
/// releases do not; `rc`, `dev`, `a` and `b` do. `None` for anything else.
fn suffix_is_pre_release(suffix: &str) -> Option<bool> {
    let tag = suffix.trim_end_matches(|c: char| c.is_ascii_digit());
    match tag {
        "" | "post" => Some(false),
        "rc" | "dev" | "a" | "b" | "alpha" | "beta" => Some(true),
        _ => None,
    }
}

impl fmt::Display for EngineVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if self.pre_release {
            f.write_str(" (pre-release)")?;
        }
        Ok(())
    }
}

/// ADR 0030 §1: why `engine`, at the build `build_fingerprint` names, cannot
/// serve decision models, or `None` when it can. The reason leads with
/// [`CAPABILITY_MISSING`].
pub fn engine_refusal(engine: Engine, build_fingerprint: &str) -> Option<String> {
    let serve_on = format!(
        "serve decision models on llama.cpp {LLAMACPP_MINIMUM} or newer, or SGLang \
         {SGLANG_MINIMUM} or newer"
    );
    let (name, minimum) = match engine {
        Engine::Llamacpp => ("llama.cpp", LLAMACPP_MINIMUM),
        Engine::Sglang => ("SGLang", SGLANG_MINIMUM),
        // ADR 0030 §6 (amended 2026-10-21, owner decision): measured live,
        // vLLM 0.32.0 answers Clef and Clef-Flash from their text output
        // instead of their decision head.
        Engine::Vllm => {
            return Some(format!(
                "{CAPABILITY_MISSING}: vLLM serves Clef and Clef-Flash from their text \
                 output instead of their decision head (questions over 26 options are \
                 refused and accuracy and calibration fall), so CapyCTL admits it once a \
                 vLLM release uses these models' decision heads; {serve_on}"
            ))
        }
        Engine::Tensorfold => {
            return Some(format!(
                "{CAPABILITY_MISSING}: CapyCTL does not run TensorFold's decision models \
                 yet; {serve_on}"
            ))
        }
        // ADR 0038 §11: TensorRT-LLM has no decision path (ADR 0030 §6).
        Engine::TensorrtLlm => {
            return Some(format!(
                "{CAPABILITY_MISSING}: TensorRT-LLM has no decision-model path; {serve_on}"
            ))
        }
        // ADR 0031 §2: a custom engine serves the API its manifest names,
        // which resolution compares with the deployment's (no version check).
        Engine::Custom => return None,
    };
    match EngineVersion::parse(build_fingerprint.trim()) {
        Some(version) if version.at_least(&minimum) => None,
        Some(version) => Some(format!(
            "{CAPABILITY_MISSING}: this {name} installation is {version}, and decision \
             models need {name} {minimum} or newer; register a newer installation with \
             `capyctl engine add`"
        )),
        None => Some(format!(
            "{CAPABILITY_MISSING}: this {name} installation's build fingerprint \
             `{build_fingerprint}` names no version, so it cannot be shown to serve decision \
             models ({name} {minimum} or newer); register it with `capyctl engine add`, \
             which records its version"
        )),
    }
}

/// [`engine_refusal`] as the configuration error resolution returns, at `api`.
pub fn check_engine(engine: Engine, build_fingerprint: &str) -> Result<(), ConfigError> {
    match engine_refusal(engine, build_fingerprint) {
        Some(reason) => Err(invalid("api", reason)),
        None => Ok(()),
    }
}

/// The API a deployment document declares, and ADR 0030 §7's refusals that
/// depend on the document alone: a decisions deployment declaring a
/// `topology` or a parking `residency`. Checked with the instance spec
/// (`instances::parse_instance_spec`), so every path that validates a
/// document's shape refuses them before anything is resolved or stored.
pub fn check_document(deployment: &Value) -> Result<DeploymentApi, ConfigError> {
    let api = match deployment.get("api") {
        None | Some(Value::Null) => DeploymentApi::Chat,
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|_| invalid("api", "must be chat or decisions"))?,
    };
    if api == DeploymentApi::Chat {
        return Ok(api);
    }
    if deployment.get("topology").is_some_and(|t| !t.is_null()) {
        return Err(invalid(
            "topology",
            "a decisions deployment runs on one machine in this version (no multi-node \
             group); remove `topology`",
        ));
    }
    if let Some(tier @ ("deep" | "host_backed")) =
        deployment.get("residency").and_then(Value::as_str)
    {
        return Err(invalid(
            "residency",
            format!(
                "a decisions deployment restarts instead of parking in this version, so \
                 residency `{tier}` is refused; declare `restart_only` or leave it out"
            ),
        ));
    }
    Ok(api)
}

/// ADR 0030 §1 (amending ADR 0024 §1): a decisions deployment gets no
/// tool-call or reasoning parser, so one it states is refused: a parser
/// setting other than `auto` in its engine configuration, or the parser
/// options in its extra arguments. The installation's host-fixed arguments
/// are the operator's, as for every deployment on it.
pub fn check_parsers(settings: &LaunchSettings) -> Result<(), ConfigError> {
    let (block, tool, reasoning) = match settings {
        LaunchSettings::Vllm(s) => ("vllm", &s.tool_call_parser, &s.reasoning_parser),
        LaunchSettings::Sglang(s) => ("sglang", &s.tool_call_parser, &s.reasoning_parser),
        // ADR 0029 §12: llama-server has no parser setting.
        LaunchSettings::Tensorfold(_) | LaunchSettings::Llamacpp(_) => return Ok(()),
        LaunchSettings::TensorrtLlm(s) => ("tensorrt-llm", &s.tool_call_parser, &s.reasoning_parser),
        // ADR 0031 §4: a custom engine's parsers are refused at resolution.
        LaunchSettings::Custom(_) => return Ok(()),
    };
    let refused = |path: String, what: &str| {
        Err(invalid(
            &path,
            format!(
                "a decisions deployment has no tool-call or reasoning parser (they apply to \
                 chat deployments); remove {what}"
            ),
        ))
    };
    for (field, value) in [("tool_call_parser", tool), ("reasoning_parser", reasoning)] {
        if value.is_some() {
            return refused(
                format!("engine_config.{block}.{field}"),
                &format!("`{field}`"),
            );
        }
    }
    for option in [
        crate::parsers::TOOL_CALL_OPTION,
        crate::parsers::REASONING_OPTION,
    ] {
        if crate::parsers::args_set(settings.extra_args(), option) {
            return refused(
                "engine_config.extra_args".into(),
                &format!("`{option}` from extra_args"),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // T43: the versions a build fingerprint names, and the minimums.
    #[test]
    fn a_fingerprint_names_a_version_only_when_it_is_one() {
        let v = |text| EngineVersion::parse(text);
        assert_eq!(v("0.5.22"), Some(EngineVersion::new(0, 5, 22)));
        assert_eq!(v("0.7.0+d812350"), Some(EngineVersion::new(0, 7, 0)));
        assert_eq!(v("0.5.22.post1"), Some(EngineVersion::new(0, 5, 22)));
        assert!(v("0.5.22rc1").unwrap().pre_release);
        assert!(v("0.7.0.dev3").unwrap().pre_release);
        for none in [
            "",
            "Python 3.12.3",
            "0.5",
            "v0.7.0",
            "0.x.1",
            "0.5.22-custom",
        ] {
            assert_eq!(v(none), None, "{none}");
        }
        let at_least = |text, minimum| v(text).unwrap().at_least(&minimum);
        assert!(at_least("0.5.22", SGLANG_MINIMUM));
        assert!(at_least("0.5.23", SGLANG_MINIMUM));
        assert!(at_least("0.6.0", SGLANG_MINIMUM));
        assert!(!at_least("0.5.21", SGLANG_MINIMUM));
        assert!(!at_least("0.5.22rc1", SGLANG_MINIMUM));
        assert!(at_least("0.7.0+abc", LLAMACPP_MINIMUM));
        assert!(at_least("1.0.0+abc", LLAMACPP_MINIMUM));
        assert!(!at_least("0.6.9+abc", LLAMACPP_MINIMUM));
    }
}

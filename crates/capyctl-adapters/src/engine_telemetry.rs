//! Owner decision 2026-10-22 (SPEC §13.3): the engines CapyCTL starts report
//! nothing about their use off the host unless the host allows it.
//!
//! The host's `engine_telemetry` (`off` by default, or `allow`) is read when
//! the role starts and holds for every launch from then on, embedded
//! (standalone) and remote (host) alike. Under `off` every engine is started
//! with its usage-reporting opt-outs set; under `allow` CapyCTL sets none and
//! each engine keeps its own default. Either way a profile or a deployment
//! cannot set them (`engine_env_reserved:<name>`,
//! `capyctl_config::engine_env::OWNED_NAMES`).
//!
//! What each engine reports, from its own source:
//! - vLLM (0.31.0 `usage/usage_lib.py`, `envs.py`): the first rank posts the
//!   platform, the model architecture and the engine settings to
//!   `https://stats.vllm.ai` at start, and its id and runtime figures every
//!   ten minutes after, unless
//!   `VLLM_NO_USAGE_STATS=1`, `VLLM_DO_NOT_TRACK=1` or `DO_NOT_TRACK=1`
//!   (`VLLM_DO_NOT_TRACK`, when set, hides `DO_NOT_TRACK`, so both are set).
//! - SGLang 0.5.22: no usage reporting of its own (no usage library, no
//!   tracking switch; its OpenTelemetry tracing is off unless a launch asks
//!   for an exporter). `DO_NOT_TRACK=1` turns off the Hugging Face Hub's
//!   telemetry it imports (`huggingface_hub/constants.py`).
//! - llama.cpp (`llama-server`, v0.6.0 source): no usage reporting.
//! - TensorFold (0.6.6): no usage reporting; its release check, which asks
//!   GitHub for a newer version, is off on every launch whatever this
//!   setting says (`TENSORFOLD_NO_UPDATE_CHECK`, `--no-update-check`).
//! - TensorRT-LLM (`trtllm-serve`, ADR 0038 §10): 1.2.1 has no usage
//!   reporting of its own; `DO_NOT_TRACK=1` turns off the Hugging Face Hub's
//!   telemetry it imports. The 1.3 and 1.4 pre-releases add
//!   `usage/usage_lib.py`, which posts usage to NVIDIA unless
//!   `TRTLLM_NO_USAGE_STATS=1`, `TELEMETRY_DISABLED=1` or `DO_NOT_TRACK=1`,
//!   so all three are set on every line.
//! - Custom engines: unknown programs, so the general convention.
//!
//! `DO_NOT_TRACK=1` (<https://donottrack.sh>) is set for every engine.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

pub use capyctl_config::remote_roles::EngineTelemetry;

/// vLLM's three opt-outs and the general convention.
pub const VLLM: &[(&str, &str)] = &[
    ("DO_NOT_TRACK", "1"),
    ("VLLM_DO_NOT_TRACK", "1"),
    ("VLLM_NO_USAGE_STATS", "1"),
];

/// SGLang, llama.cpp, TensorFold and custom engines: the general convention.
pub const DO_NOT_TRACK: &[(&str, &str)] = &[("DO_NOT_TRACK", "1")];

/// ADR 0038 §10: TensorRT-LLM's opt-outs (1.3 and 1.4 `usage/usage_lib.py`)
/// and the general convention. A TensorRT-LLM profile or deployment cannot
/// set them (`capyctl_config::tensorrt_llm::REFUSED_ENV_NAMES` and the
/// `TRTLLM_` family).
pub const TENSORRT_LLM: &[(&str, &str)] = &[
    ("DO_NOT_TRACK", "1"),
    ("TRTLLM_NO_USAGE_STATS", "1"),
    ("TELEMETRY_DISABLED", "1"),
];

/// Whether this role's host allows the engines' own usage reporting.
static ALLOWED: AtomicBool = AtomicBool::new(false);

/// Set by the host or standalone role at start, from its document.
pub fn set_host_setting(setting: EngineTelemetry) {
    ALLOWED.store(setting == EngineTelemetry::Allow, Ordering::SeqCst);
}

/// The host's setting for this role: `off` until a role says otherwise.
pub fn host_setting() -> EngineTelemetry {
    if ALLOWED.load(Ordering::SeqCst) {
        EngineTelemetry::Allow
    } else {
        EngineTelemetry::Off
    }
}

/// Write `opt_outs` into a launch environment under `off`, replacing any
/// value; under `allow` the environment is left as it is.
pub fn apply(
    env: &mut BTreeMap<String, String>,
    opt_outs: &[(&str, &str)],
    setting: EngineTelemetry,
) {
    if setting == EngineTelemetry::Off {
        for (name, value) in opt_outs {
            env.insert((*name).to_owned(), (*value).to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T21 T49 (owner decision 2026-10-22, ADR 0038 §10): `off` writes every
    // opt-out over any value; `allow` writes nothing. Every opt-out is a name
    // the profile and the deployment cannot set (TensorRT-LLM's through its
    // own refused names).
    #[test]
    fn off_writes_the_opt_outs_and_allow_leaves_the_environment() {
        let before = BTreeMap::from([("DO_NOT_TRACK".to_owned(), "0".to_owned())]);
        for opt_outs in [VLLM, DO_NOT_TRACK, TENSORRT_LLM] {
            let mut env = before.clone();
            apply(&mut env, opt_outs, EngineTelemetry::Off);
            for (name, value) in opt_outs {
                assert_eq!(env[*name], *value, "{name}");
                assert!(
                    capyctl_config::engine_env::is_owned(name)
                        || (opt_outs == TENSORRT_LLM
                            && capyctl_config::tensorrt_llm::refused_env_name(name)),
                    "{name}"
                );
            }
            assert_eq!(env.len(), opt_outs.len());
            let mut env = before.clone();
            apply(&mut env, opt_outs, EngineTelemetry::Allow);
            assert_eq!(env, before);
        }
    }
}

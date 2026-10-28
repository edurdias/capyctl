//! ADR 0038 §3, §4, §14: the one TensorRT-LLM launch builder the host agent
//! and the embedded coordinator both use (SPEC §3: the paths cannot drift).
//! It reads the checkpoint where it is, so the context fit, the KV geometry
//! behind `max_tokens`, the parsers and the path options' symlinks are all
//! read on the machine that launches.
use std::path::{Path, PathBuf};

use capyctl_config::effective::EffectiveDeployment;
use capyctl_config::engine_policy::{parse_options, sensitivity, Engine, Sensitivity};
use capyctl_config::tensorrt_llm::{checkpoint_kv_geometry, served_name_refusal, tokens_per_block};
use capyctl_domain::launch::LaunchSettings;

use super::args::PlanInputTensorrtLlm;

#[derive(Debug, thiserror::Error)]
pub enum TensorrtLlmPlanError {
    #[error(
        "the frozen profile declares TensorRT-LLM but carries another family's launch settings"
    )]
    OtherFamily,
    #[error("the deployment serves no route")]
    NoRoute,
    #[error("{0}")]
    Unresolved(String),
    /// ADR 0038 §3: the reason leads with `served_name_unsupported`.
    #[error("{0}")]
    ServedName(String),
    #[error("no context length could be fitted for the TensorRT-LLM launch")]
    NoContext,
    /// ADR 0038 §4: the reason leads with `kv_geometry_unsupported`.
    #[error("{0}")]
    Geometry(String),
    /// ADR 0038 §4: the KV cache grant holds no token of the checkpoint.
    #[error("the KV cache grant of {0} bytes holds no token of this checkpoint")]
    NoPool(i64),
    #[error("the selected GPU cannot be pinned")]
    UnpinnableDevice,
    /// ADR 0014 §8, ADR 0038 §3: a path option's value resolves outside the
    /// approved paths through a symlink (the launch-time half of the check).
    #[error("{0} names a path outside the approved paths")]
    PathNotApproved(String),
}

/// ADR 0038 §3: the plan of one launch of `effective`, its engine on `port`,
/// its log at `engine_log`, its directory `launch_dir` (CapyCTL-owned, per
/// launch, under the role's runtime directory; created by
/// [`super::prepare_launch_dir`] at Initialize).
pub fn plan_from_effective(
    effective: &EffectiveDeployment,
    port: u16,
    engine_log: String,
    launch_dir: &Path,
) -> Result<PlanInputTensorrtLlm, TensorrtLlmPlanError> {
    let profile = &effective.profile;
    let LaunchSettings::TensorrtLlm(settings) = &effective.engine_config else {
        return Err(TensorrtLlmPlanError::OtherFamily);
    };
    let common = &settings.common;
    // SPEC §3: the served name is the deployment's route; ADR 0038 §3 links
    // the checkpoint under it, so it must be one path component.
    let served_model_name = effective
        .routes
        .first()
        .cloned()
        .ok_or(TensorrtLlmPlanError::NoRoute)?;
    if let Some(reason) = served_name_refusal(&served_model_name) {
        return Err(TensorrtLlmPlanError::ServedName(reason));
    }
    let checkpoint = PathBuf::from(
        effective
            .model
            .require_resolved_path()
            .map_err(|error| TensorrtLlmPlanError::Unresolved(error.to_string()))?,
    );
    // ADR 0014 §5, ADR 0038 §3: an undeclared context is fitted to the KV
    // grant from the checkpoint read here, held to whole KV blocks.
    let max_seq_len = capyctl_config::context_fit::fit_for_effective(effective)
        .tokens
        .filter(|tokens| *tokens > 0)
        .ok_or(TensorrtLlmPlanError::NoContext)?;
    // ADR 0038 §4: `max_tokens = floor(kv_bytes / bytes_per_token)` from the
    // checkpoint's geometry; a geometry CapyCTL cannot compute is refused.
    let geometry = checkpoint_kv_geometry(
        &checkpoint,
        common.dtype.as_deref(),
        common.kv_cache_dtype.as_deref(),
    )
    .map_err(TensorrtLlmPlanError::Geometry)?;
    let max_tokens = geometry
        .max_tokens(settings.memory.kv_cache_bytes)
        .ok_or(TensorrtLlmPlanError::NoPool(settings.memory.kv_cache_bytes))?;
    // ADR 0038 §14 (ADR 0024): parsers chosen by model family from the
    // checkpoint read here, unless the deployment named or turned them off.
    let (tool_parser, reasoning_parser) = capyctl_config::parsers::parsers_for_effective(effective)
        .map(|parsers| (parsers.tool_call.name, parsers.reasoning.name))
        .unwrap_or_default();
    // ADR 0014 §8, ADR 0038 §3: `--chat_template`'s file must still lie
    // inside an approved path once symlinks resolve.
    let approved: Vec<PathBuf> = profile
        .security
        .approved_paths
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect();
    let options = parse_options(&settings.extra_args)
        .map_err(|error| TensorrtLlmPlanError::Unresolved(error.to_string()))?;
    for option in options {
        if let Some(Sensitivity::Path { .. }) = sensitivity(Engine::TensorrtLlm, &option.name) {
            let inside = option.value.as_deref().is_some_and(|value| {
                std::fs::canonicalize(value)
                    .is_ok_and(|real| approved.iter().any(|root| real.starts_with(root)))
            });
            if !inside {
                return Err(TensorrtLlmPlanError::PathNotApproved(option.name));
            }
        }
    }
    Ok(PlanInputTensorrtLlm {
        engine_bin: profile.executable.clone(),
        engine_path_extra: Path::new(&profile.executable)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(display),
        cuda_home: profile.cuda_home.clone(),
        build_env: effective.engine_env.values(),
        // ADR 0038 §2: the profile's build fingerprint is the checked version.
        version: profile.build_fingerprint.clone(),
        served_model_name,
        checkpoint_dir: display(&checkpoint),
        port,
        max_seq_len,
        max_batch_size: settings.max_batch_size(),
        max_tokens,
        tokens_per_block: tokens_per_block(&settings.llm_api_options),
        dtype: common.dtype.clone(),
        kv_cache_dtype: common.kv_cache_dtype.clone(),
        cuda_graphs: common.cuda_graphs,
        llm_api_options: settings.llm_api_options.clone(),
        reasoning_parser,
        tool_parser,
        trust_remote_code: common.trust_remote_code,
        engine_args: profile.args.clone(),
        extra_args: settings.extra_args.clone(),
        launch_dir: display(launch_dir),
        engine_log: Some(engine_log),
        cuda_namespaces: effective
            .cuda_namespaces()
            .map_err(|_| TensorrtLlmPlanError::UnpinnableDevice)?
            .unwrap_or_default(),
    })
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

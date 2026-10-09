//! ADR 0029 §5, §6, §9: the one llama.cpp launch builder the host agent and
//! the embedded coordinator both use (SPEC §3: the paths cannot drift). It
//! reads the checkpoint where it is, so the GGUF and the projector are picked
//! and the path options' symlinks resolved on the machine that launches.
use std::path::{Path, PathBuf};

use capyctl_config::checkpoint_layout::{pick_gguf, projector_file};
use capyctl_config::effective::EffectiveDeployment;
use capyctl_config::engine_policy::{parse_options, sensitivity, Engine, Sensitivity};
use capyctl_config::llamacpp::{path_values, DEFAULT_CACHE_TYPE};
use capyctl_domain::launch::LaunchSettings;

use super::args::PlanInputLlamacpp;

/// ADR 0029 §6: the private directories `XDG_CONFIG_HOME` and `LLAMA_CACHE`
/// point at (`<state>/engines/llamacpp/{config,cache}`, created by the host's
/// engine cache).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamacppDirs {
    pub config: PathBuf,
    pub cache: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LlamacppPlanError {
    #[error("the frozen profile declares llama.cpp but carries another family's launch settings")]
    OtherFamily,
    #[error("the deployment serves no route")]
    NoRoute,
    #[error("{0}")]
    Unresolved(String),
    #[error("a llama.cpp deployment states context_length")]
    NoContext,
    /// ADR 0029 §9: the checkpoint holds no GGUF to render, several without
    /// `gguf_file`, or not the file the deployment names.
    #[error("{0}")]
    Checkpoint(String),
    #[error("the selected GPU cannot be pinned")]
    UnpinnableDevice,
    /// ADR 0014 §8, ADR 0029 §8: a path option's value resolves outside the
    /// approved paths through a symlink (the launch-time half of the check).
    #[error("{0} names a path outside the approved paths")]
    PathNotApproved(String),
}

pub fn plan_from_effective(
    effective: &EffectiveDeployment,
    port: u16,
    engine_log: String,
    dirs: &LlamacppDirs,
) -> Result<PlanInputLlamacpp, LlamacppPlanError> {
    let profile = &effective.profile;
    let LaunchSettings::Llamacpp(settings) = &effective.engine_config else {
        return Err(LlamacppPlanError::OtherFamily);
    };
    let served_model_name = effective
        .routes
        .first()
        .cloned()
        .ok_or(LlamacppPlanError::NoRoute)?;
    let checkpoint = PathBuf::from(
        effective
            .model
            .require_resolved_path()
            .map_err(|error| LlamacppPlanError::Unresolved(error.to_string()))?,
    );
    // ADR 0029 §9: one GGUF model, or the one `gguf_file` names, inside the
    // checkpoint; the projector likewise.
    let gguf = pick_gguf(&checkpoint, settings.gguf_file.as_deref())
        .map_err(LlamacppPlanError::Checkpoint)?;
    let mmproj_file = settings
        .mmproj_file
        .as_deref()
        .map(|file| projector_file(&checkpoint, file))
        .transpose()
        .map_err(LlamacppPlanError::Checkpoint)?
        .map(|relative| display(&checkpoint.join(relative)));
    // ADR 0014 §8, ADR 0029 §8: every path a path option among the extras
    // names (the draft model included) must still lie inside an approved
    // path once symlinks resolve.
    let approved: Vec<PathBuf> = profile
        .security
        .approved_paths
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect();
    let options = parse_options(&settings.extra_args)
        .map_err(|error| LlamacppPlanError::Unresolved(error.to_string()))?;
    for option in options {
        if let Some(Sensitivity::Path { .. }) = sensitivity(Engine::Llamacpp, &option.name) {
            let value = option.value.unwrap_or_default();
            let inside = path_values(&option.name, &value).is_some_and(|paths| {
                paths.iter().all(|path| {
                    std::fs::canonicalize(path)
                        .is_ok_and(|real| approved.iter().any(|root| real.starts_with(root)))
                })
            });
            if !inside {
                return Err(LlamacppPlanError::PathNotApproved(option.name));
            }
        }
    }
    Ok(PlanInputLlamacpp {
        engine_bin: profile.executable.clone(),
        engine_path_extra: Path::new(&profile.executable)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(display),
        cuda_home: profile.cuda_home.clone(),
        build_env: effective.engine_env.values(),
        model_file: display(&checkpoint.join(gguf.file)),
        mmproj_file,
        served_model_name,
        port,
        context_length: settings
            .common
            .context_length
            .ok_or(LlamacppPlanError::NoContext)?,
        slots: settings.slots(),
        n_gpu_layers: settings.n_gpu_layers,
        cache_type: settings
            .common
            .kv_cache_dtype
            .clone()
            .unwrap_or_else(|| DEFAULT_CACHE_TYPE.to_owned()),
        engine_args: profile.args.clone(),
        extra_args: settings.extra_args.clone(),
        config_dir: display(&dirs.config),
        cache_dir: display(&dirs.cache),
        engine_log: Some(engine_log),
        cuda_namespace: effective
            .cuda_namespace()
            .map_err(|_| LlamacppPlanError::UnpinnableDevice)?,
    })
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

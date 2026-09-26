//! Shared vLLM launch-plan construction for embedded and remote host execution.
//!
//! Both callers supply already-resolved local policy and grant no launch
//! authority by calling this: the embedded coordinator builds a plan for the
//! engine it owns, and the host agent builds one from its own approved host
//! document. One builder keeps the live-proven S1 recipe identical on both
//! paths (Spec §3), so neither can drift into a runtime nobody reviewed.

use std::path::Path;

use mllm_config::effective::EffectiveDeployment;
use mllm_domain::launch::{LaunchSettings, VllmLaunchSettings};

use crate::policy::ParkPolicy;
use crate::vllm::args::{device_utilization_pct, GrantedBudget, PlanInputVllm};

/// Reserved (ADR 0014 §3): the gate vLLM checks free memory against at start.
/// Explicit KV bytes size the pool, so the gate only has to pass.
pub const GPU_UTILIZATION_GATE_PCT: u8 = 10;

/// Why a vLLM plan could not be built. Each message names the launch shape,
/// never a credential.
#[derive(Debug, thiserror::Error)]
pub enum VllmPlanError {
    #[error("the frozen profile declares vLLM but carries another family's launch settings")]
    OtherFamily,
    #[error("the deployment serves no route")]
    NoRoute,
    /// SPEC §13.3: a launch needs a directory on disk; an unresolved model
    /// source is refused rather than invented.
    #[error("{0}")]
    Unresolved(String),
    /// Discrete GPU design §7: the host has several GPUs and the selected one
    /// has neither a published UUID nor a `gpuN` index to pin it by.
    #[error("the selected GPU cannot be pinned")]
    UnpinnableDevice,
}

/// The startup flags that put vLLM into the development mode its park controls
/// live behind, with the eager checkpoint loader this project qualified on Spark:
/// mmap-backed tensor copies during weight restoration are what the eager strategy
/// avoids. Rendered only when the profile asks for sleep mode and the host has not
/// opted out of deep park (Spec §3).
pub fn sleep_flags(settings: &VllmLaunchSettings, deep_park_enabled: bool) -> Vec<String> {
    // ADR 0014 §3: sleep mode is reserved and derived at resolution from the
    // host's deep-park switch and the deployment's residency; the host switch is
    // checked again here so an opted-out host never renders development mode.
    if settings.enable_sleep_mode && deep_park_enabled {
        vec![
            "--enable-sleep-mode".into(),
            "--safetensors-load-strategy".into(),
            "eager".into(),
        ]
    } else {
        Vec::new()
    }
}

/// SPEC §9.1 / T21 / ADR 0012: deep-park paths are available unless the
/// resolved host policy opts out. The profile carries that decision; it is
/// never re-derived from anything ambient. SPEC §6.2: `restart_only`
/// prohibits sleep calls, so a deployment that declared it gets no park path
/// even on a host that leaves deep parking on.
pub fn park_policy(effective: &EffectiveDeployment) -> ParkPolicy {
    if effective.profile.security.deep_park.is_enabled() && effective.residency.parks() {
        ParkPolicy::Enabled
    } else {
        ParkPolicy::Disabled
    }
}

/// Discrete GPU design §6 (ADR 0019): on a device domain vLLM's utilization is
/// the device request's share of the card the launching host observed
/// ([`device_utilization_pct`]); vLLM checks at start that this share is free.
/// Everywhere else, and before a host has stated the card's total (a plan built
/// to check authority, not to launch), the low unified gate. A launch on a
/// device domain states the total first (`EffectiveDeployment::with_device_total`)
/// or is refused.
///
/// The share is the memory request, not the device domain's charge: the
/// charge also carries the CUDA context and graphs vLLM holds beyond its
/// utilization budget (ADR 0019), which vLLM must not be told to allocate.
fn utilization_pct(effective: &EffectiveDeployment, settings: &VllmLaunchSettings) -> u8 {
    match (
        effective.ready_device_allocation(),
        settings.memory.device_total_bytes,
    ) {
        (Some(_), Some(total)) => device_utilization_pct(settings.memory.request_bytes, total),
        _ => GPU_UTILIZATION_GATE_PCT,
    }
}

/// The launch plan for one vLLM start, built from the frozen effective
/// configuration and the binding's own leased port (Spec §3). `engine_log` and
/// `runtime_dir` are properties of the installation that runs the engine, not of
/// the deployment that was admitted. Nothing here is read from the environment.
pub fn plan_from_effective(
    effective: &EffectiveDeployment,
    port: u16,
    engine_log: String,
    runtime_dir: String,
) -> Result<PlanInputVllm, VllmPlanError> {
    let profile = &effective.profile;
    let LaunchSettings::Vllm(settings) = &effective.engine_config else {
        return Err(VllmPlanError::OtherFamily);
    };
    let common = &settings.common;
    // Spec §3: the served name is the deployment's route, so clients request
    // the route the deployment serves rather than a checkpoint path.
    let served_model_name = effective
        .routes
        .first()
        .cloned()
        .ok_or(VllmPlanError::NoRoute)?;
    let model_path = effective
        .model
        .require_resolved_path()
        .map_err(|error| VllmPlanError::Unresolved(error.to_string()))?
        .to_owned();
    // ADR 0014 §8: host-fixed arguments and the deployment's extras stay
    // apart, so the entry can gate exactly what the extras resolve to.
    let engine_args = profile.args.clone();
    Ok(PlanInputVllm {
        engine_bin: profile.executable.clone(),
        // The engine's own bin directory has to be on PATH: the JIT compile
        // step shells out to the tools that were installed beside it.
        engine_path_extra: Path::new(&profile.executable)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.to_string_lossy().into_owned()),
        cuda_home: profile.cuda_home.clone(),
        build_env: crate::engine_env::build_overrides(&profile.env),
        model_path,
        port,
        served_model_name,
        // Reserved: tensor and pipeline parallelism come from placement, and
        // multi-rank groups are parked (TP2 deferred), so a vLLM launch is one rank.
        tensor_parallel_size: 1,
        pipeline_parallel_size: 1,
        // ADR 0014 §2: typed fields render in vLLM's own spelling; an omitted
        // field renders nothing and the engine's default applies.
        dtype: common.dtype.clone(),
        quantization: common.quantization.clone(),
        kv_cache_dtype: common.kv_cache_dtype.clone(),
        block_size_tokens: settings.block_size_tokens,
        // ADR 0014 §5 (owner decision 2026-09-25): an undeclared context is
        // fitted to the KV cache grant from the checkpoint's configuration,
        // read here where the checkpoint is; a host-fixed `--max-model-len`
        // wins and nothing is passed.
        context_length: mllm_config::context_fit::fit_for_effective(effective).tokens,
        max_concurrent_requests: common.max_concurrent_requests,
        max_num_batched_tokens: settings.max_num_batched_tokens,
        enforce_eager: common.cuda_graphs == Some(false),
        language_model_only: common.language_model_only,
        trust_remote_code: common.trust_remote_code,
        // Reserved: CPU offload and swap are not offered to deployments.
        cpu_offload_bytes: 0,
        // ADR 0014 §5: the KV cache is the deployment's declared or derived
        // value, already bounded by the memory request admission reserves.
        // On a unified domain the utilization gate stays low because the
        // explicit KV bytes size the pool, and the gate must pass while a
        // previous deployment's memory is still being released.
        granted: GrantedBudget {
            kv_cache_bytes: Some(settings.memory.kv_cache_bytes),
            gpu_utilization_pct: Some(utilization_pct(effective, settings)),
            swap_space_bytes: None,
        },
        engine_args,
        extra_args: settings.extra_args.clone(),
        // ADR 0014 §8, SPEC §8.2: the host's approvals, which the entry applies
        // to the destinations the extras resolve to.
        extra_approvals: Some(mllm_config::engine_policy::extra_approvals_document(
            &profile.security.approved_options,
            &profile.security.approved_paths,
            profile.security.trust_remote_code,
        )),
        // SPEC §3: deep park is one switch. A host that opted out never
        // launches a development-mode engine, whatever the profile asked for.
        sleep_flags: sleep_flags(settings, profile.security.deep_park.is_enabled()),
        // SPEC §13.3: the engine key travels in the child's environment. The
        // renderer must never see one, so it can never reach argv.
        api_key: None,
        engine_log: Some(engine_log),
        runtime_dir: Some(runtime_dir),
        // Discrete GPU design §7 (controller ruling): with a choice of GPU the
        // selected one is always pinned; one that cannot be is refused.
        cuda_namespace: effective
            .cuda_namespace()
            .map_err(|_| VllmPlanError::UnpinnableDevice)?,
    })
}

//! Shared vLLM launch-plan construction for embedded and remote host execution.
//!
//! Both callers supply already-resolved local policy and grant no launch
//! authority by calling this: the embedded coordinator builds a plan for the
//! engine it owns, and the host agent builds one from its own approved host
//! document. One builder keeps the live-proven S1 recipe identical on both
//! paths (Spec §3), so neither can drift into a runtime nobody reviewed.

use std::path::Path;

use capyctl_config::effective::EffectiveDeployment;
use capyctl_domain::launch::{LaunchSettings, SafetensorsLoadStrategy, VllmLaunchSettings};

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
/// live behind, with the deployment's checkpoint loader. Rendered only when the
/// profile asks for sleep mode and the host has not opted out of deep park
/// (Spec §3).
///
/// ADR 0014 §4 (amended 2026-10-07, owner decision 1): the loader defaults to
/// `eager`, which this project qualified on Spark with vLLM 0.29 (mmap-backed
/// tensor copies during weight restoration are what it avoids; deep wake 57 s
/// to 7.5 s on Qwen3-4B). A deployment may choose `lazy` through
/// `engine_config.vllm.safetensors_load_strategy`: on vLLM 0.30 NVFP4
/// checkpoints eager keeps more memory once loaded and no longer wakes faster.
pub fn sleep_flags(settings: &VllmLaunchSettings, deep_park_enabled: bool) -> Vec<String> {
    // ADR 0014 §3: sleep mode is reserved and derived at resolution from the
    // host's deep-park switch and the deployment's residency; the host switch is
    // checked again here so an opted-out host never renders development mode.
    if settings.enable_sleep_mode && deep_park_enabled {
        let strategy = settings
            .safetensors_load_strategy
            .unwrap_or(SafetensorsLoadStrategy::Eager);
        vec![
            "--enable-sleep-mode".into(),
            "--safetensors-load-strategy".into(),
            strategy.as_str().into(),
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
    // ADR 0024 (owner decision 2026-10-03): parsers chosen by model family
    // from the checkpoint read here, unless the deployment named or turned
    // them off, or its extras or the host-fixed args already pass them.
    let parsers = capyctl_config::parsers::parsers_for_effective(effective);
    let enable_auto_tool_choice = parsers.as_ref().is_some_and(|parsers| {
        capyctl_config::parsers::vllm_auto_tool_choice(parsers, &profile.args, &settings.extra_args)
    });
    let (tool_call_parser, reasoning_parser) = parsers
        .map(|parsers| (parsers.tool_call.name, parsers.reasoning.name))
        .unwrap_or_default();
    // SPEC §3: deep park is one switch. A host that opted out never
    // launches a development-mode engine, whatever the profile asked for.
    let sleep = sleep_flags(settings, profile.security.deep_park.is_enabled());
    Ok(PlanInputVllm {
        engine_bin: profile.executable.clone(),
        // The engine's own bin directory has to be on PATH: the JIT compile
        // step shells out to the tools that were installed beside it.
        engine_path_extra: Path::new(&profile.executable)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.to_string_lossy().into_owned()),
        cuda_home: profile.cuda_home.clone(),
        build_env: effective.engine_env.values(),
        model_path,
        port,
        served_model_name,
        // Reserved: tensor and pipeline parallelism come from placement. A
        // single-rank launch is one rank; a group member takes both from the
        // group plan through `with_group` (ADR 0028 §10).
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
        context_length: capyctl_config::context_fit::fit_for_effective(effective).tokens,
        // Owner decision 2026-10-02: undeclared, vLLM runs as many sequences
        // as CapyCTL keeps in flight for the deployment; a host-fixed
        // `--max-num-seqs` wins and nothing is passed.
        max_concurrent_requests: common.max_concurrent_requests.or_else(|| {
            capyctl_config::context_fit::vllm_default_max_num_seqs(
                &effective.engine_config,
                &profile.args,
            )
        }),
        max_num_batched_tokens: settings.max_num_batched_tokens,
        // ADR 0014 §4 (amended 2026-10-07): outside sleep mode a declared
        // loader renders as a typed field; under sleep mode `sleep_flags`
        // renders it, declared or defaulted, beside the switch it belongs to.
        safetensors_load_strategy: if sleep.is_empty() {
            settings
                .safetensors_load_strategy
                .map(|strategy| strategy.as_str().to_owned())
        } else {
            None
        },
        enforce_eager: common.cuda_graphs == Some(false),
        language_model_only: common.language_model_only,
        trust_remote_code: common.trust_remote_code,
        tool_call_parser,
        reasoning_parser,
        enable_auto_tool_choice,
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
        extra_approvals: Some(capyctl_config::engine_policy::extra_approvals_document(
            &profile.security.approved_options,
            &profile.security.approved_paths,
            profile.security.trust_remote_code,
        )),
        sleep_flags: sleep,
        // SPEC §13.3: the engine key travels in the child's environment. The
        // renderer must never see one, so it can never reach argv.
        api_key: None,
        engine_log: Some(engine_log),
        runtime_dir: Some(runtime_dir),
        // Discrete GPU design §7 (review decision): with a choice of GPU the
        // selected one is always pinned; one that cannot be is refused.
        cuda_namespace: effective
            .cuda_namespace()
            .map_err(|_| VllmPlanError::UnpinnableDevice)?,
        // ADR 0028 §10: single-rank unless `with_group` sets it.
        group: None,
    })
}

/// ADR 0028 §10: a group member's launch plan. Tensor and pipeline
/// parallelism come from the group plan instead of the single-rank `1` pin;
/// everything else stays as `plan_from_effective` froze it.
pub fn with_group(
    mut plan: PlanInputVllm,
    group: capyctl_domain::group::GroupMemberArgs,
) -> PlanInputVllm {
    plan.tensor_parallel_size = group.tensor_parallel;
    plan.pipeline_parallel_size = group.pipeline_parallel;
    plan.group = Some(group);
    plan
}

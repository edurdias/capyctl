//! Shared frozen native recipe construction for embedded and remote host execution.
//! Both callers supply already-resolved local policy; this grants no launch authority.
use crate::sglang::pinned::NATIVE_SGLANG_CONTRACT;
use crate::traits::RuntimeError;
use capyctl_config::effective::EffectiveDeployment;
use capyctl_config::engine_policy::Engine;
use capyctl_domain::launch::{
    LaunchSettings, NativeDeviceSelection, NativeLaunch, NativeLaunchMetadata, SglangLaunchSettings,
};
use sha2::{Digest, Sha256};

pub fn frozen_from_effective(
    effective: &EffectiveDeployment,
    binding_id: &str,
    incarnation: &str,
    endpoint: &str,
    served_name: String,
    inference_ref: String,
    admin_ref: String,
) -> Result<NativeLaunch, RuntimeError> {
    let profile = &effective.profile;
    let declared: &SglangLaunchSettings = match (&profile.engine, &effective.engine_config) {
        (Engine::Sglang, LaunchSettings::Sglang(settings)) => settings,
        _ => {
            return Err(RuntimeError::Uncertain(
                "initialize work does not name the native SGLang engine".into(),
            ));
        }
    };
    // ADR 0014 §5 (owner decision 2026-09-25): an undeclared context is fitted
    // to the KV cache grant from the checkpoint's configuration, read here
    // where the checkpoint is, and rendered as `--context-length`. The digest
    // below covers the value the entry is given.
    let mut settings = declared.clone();
    settings.common.context_length =
        capyctl_config::context_fit::fit_for_effective(effective).tokens;
    // ADR 0014 amendment A14 (owner decision 2026-10-03): `memory.kv_cache` is
    // SGLang's KV pool, in tokens, and a hybrid model's recurrent state is
    // sized for its running requests beside it; a state that does not fit
    // the memory request is refused here, before anything starts.
    let pool = capyctl_config::context_fit::sglang_pool_for_launch(
        &settings,
        &profile.args,
        effective
            .model
            .resolved_path
            .as_deref()
            .map(std::path::Path::new),
    )
    .map_err(RuntimeError::Refused)?;
    settings.max_total_tokens = settings.max_total_tokens.or(pool.max_total_tokens);
    if let Some(running) = pool.max_running_requests {
        settings.common.max_concurrent_requests = Some(running);
    }
    settings.max_mamba_cache_size = pool.max_mamba_cache_size;
    settings.static_allowance_bytes = pool.static_allowance;
    // The static pool holds the weights, the KV cache, the state and SGLang's
    // own allocations (unified memory only): what it takes beyond the request
    // less the margin comes out of the margin.
    if let Some(static_bytes) = pool.static_bytes {
        settings.memory.margin_bytes = settings.memory.request_bytes - static_bytes;
    }
    // ADR 0024 (owner decision 2026-10-03): the parsers chosen by model
    // family from the checkpoint read here, unless the deployment named or
    // turned them off or its extras already pass them. The digest covers them.
    let parsers = capyctl_config::parsers::parsers_for_effective(effective);
    (settings.tool_call_parser, settings.reasoning_parser) = parsers
        .map(|parsers| (parsers.tool_call.name, parsers.reasoning.name))
        .unwrap_or_default();
    let settings = &settings;
    // SPEC §3: the launch carries exactly one reviewed logical placement. The
    // native startup still resolves and corroborates it independently.
    let [device] = effective.selected_devices.as_slice() else {
        return Err(RuntimeError::Uncertain(
            "initialize work must carry exactly one selected device".into(),
        ));
    };
    let memory_domain = &effective
        .host
        .devices
        .get(&device.id)
        .ok_or_else(|| RuntimeError::Uncertain("selected device is not a host device".into()))?
        .domain;
    // The service-authorized physical UUID the guarded launcher sets the
    // child's CUDA namespace from. Absent when the host published no
    // inventory, which leaves the namespace unset and the placement gate
    // fail-closed (runtime/sglang_device.py's guarded-service obligation).
    let physical_gpu_uuid = effective
        .host
        .devices
        .get(&device.id)
        .and_then(|policy| policy.physical_gpu_uuid.clone());
    // Discrete GPU design §7 (review decision): with a choice of GPU and no
    // published UUID, the selected GPU is pinned by its PCI-ordered index; one
    // that cannot be pinned either way is refused, never handed every GPU.
    let cuda_pci_index = match effective
        .cuda_namespace()
        .map_err(|_| RuntimeError::Uncertain("the selected GPU cannot be pinned".into()))?
    {
        Some(capyctl_config::effective::CudaNamespace::PciIndex(index)) => Some(index),
        _ => None,
    };
    // SPEC §13.3: a launch needs a directory on disk; an unresolved source is
    // refused rather than invented.
    let checkpoint_root = effective
        .model
        .require_resolved_path()
        .map_err(|_| {
            RuntimeError::Uncertain("initialize work resolves to no checkpoint root".into())
        })?
        .to_owned();
    let digest = hex::encode(Sha256::digest(
        serde_json::to_vec(settings)
            .map_err(|_| RuntimeError::Uncertain("settings encoding failed".into()))?,
    ));
    let metadata = NativeLaunchMetadata {
        engine: "sglang".into(),
        // ADR 0014 §9: no checkpoint recipe pin. The contract names the
        // descriptor shape, and the checkpoint is identified by the
        // deployment's declared fingerprint until WE3 records a digest.
        recipe: NATIVE_SGLANG_CONTRACT.into(),
        checkpoint_revision: effective.model.content_fingerprint.clone(),
        binding_id: binding_id.into(),
        incarnation: incarnation.into(),
        endpoint: format!("http://{}", endpoint),
        served_name,
        rendered_settings_digest: digest,
        // The host's published inventory digest (SPEC §3: reviewed logical
        // placement) travels to the entry so its composition can assert
        // placement against freshly collected inventory. Absent when the host
        // published none, which leaves the gate unasserted and fail-closed.
        placement_digest: effective.host.device_inventory_digest.clone(),
        device: NativeDeviceSelection {
            host_id: effective.host.name.clone(),
            hardware_fingerprint: effective.host.hardware_fingerprint.clone(),
            device_id: device.id.clone(),
            memory_domain: memory_domain.clone(),
            physical_gpu_uuid,
            cuda_pci_index,
        },
    };
    Ok(NativeLaunch::from_frozen_store(
        metadata,
        checkpoint_root,
        profile.executable.clone(),
        inference_ref,
        admin_ref,
        settings.clone(),
    )
    .with_toolchain(
        profile.cuda_home.clone(),
        crate::engine_env::build_overrides(&profile.env),
    ))
}

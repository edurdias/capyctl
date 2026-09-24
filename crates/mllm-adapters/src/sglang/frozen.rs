//! Shared frozen native recipe construction for embedded and remote host execution.
//! Both callers supply already-resolved local policy; this grants no launch authority.
use crate::traits::RuntimeError;
use crate::sglang::pinned::NATIVE_SGLANG_CONTRACT;
use mllm_config::effective::EffectiveDeployment;
use mllm_config::engine_policy::Engine;
use mllm_domain::launch::{
    LaunchSettings, NativeDeviceSelection, NativeLaunch, NativeLaunchMetadata,
    SglangLaunchSettings,
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
    let settings: &SglangLaunchSettings = match (&profile.engine, &effective.engine_config) {
        (Engine::Sglang, LaunchSettings::Sglang(settings)) => settings,
        _ => {
            return Err(RuntimeError::Uncertain(
                "initialize work does not name the native SGLang engine".into(),
            ));
        }
    };
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
        },
    };
    Ok(NativeLaunch::from_frozen_store(
        metadata,
        checkpoint_root,
        profile.executable.clone(),
        inference_ref,
        admin_ref,
        settings.clone(),
    ))
}

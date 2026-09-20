//! Production construction of the frozen native launch and the shared private
//! descriptor. The builder reads only frozen, validated arm input; it grants no
//! send authority and never places checkpoint paths or credential references in
//! an error (closed errors).

use crate::coordinator::CoordinatorError;
use mllm_adapters::traits::RuntimeError;
use mllm_config::effective::sglang::{
    NATIVE_CHECKPOINT_REVISION, NATIVE_SGLANG_RECIPE, NATIVE_SGLANG_SOURCE_REVISION,
};
use mllm_config::engine_policy::Engine;
use mllm_domain::completion::StepExecutionContext;
use mllm_domain::launch::{
    NativeDeviceSelection, NativeLaunch, NativeLaunchMetadata, ProfileLaunchSettings,
    SglangLaunchSettings,
};
use mllm_store::ordinary_lifecycle::worker::InitializeWork;
use sha2::{Digest, Sha256};

/// Builds the frozen native launch for an armed ordinary initialize.
///
/// The pinned SGLang recipe constants identify the build and checkpoint the
/// native contract was written against; the store work must name that engine.
/// The served name is the deployment's own route name, which the caller takes
/// from `work.effective().routes.first()` and refuses to omit. Anything else
/// is refused closed rather than adapted.
pub fn frozen_from_work(
    work: &InitializeWork,
    served_name: String,
    inference_ref: String,
    admin_ref: String,
) -> Result<NativeLaunch, CoordinatorError> {
    let effective = work.effective();
    let profile = &effective.profile;
    let settings: &SglangLaunchSettings = match (&profile.engine, &profile.launch_settings) {
        (Engine::Sglang, ProfileLaunchSettings::Sglang(settings)) => settings,
        _ => {
            return Err(CoordinatorError::Service(
                "initialize work does not name the pinned native SGLang engine".into(),
            ))
        }
    };
    // SPEC §3: the launch carries exactly one reviewed logical placement. The
    // native startup still resolves and corroborates it independently.
    let [device] = effective.selected_devices.as_slice() else {
        return Err(CoordinatorError::Service(
            "initialize work must carry exactly one selected device".into(),
        ));
    };
    let memory_domain = &effective
        .host
        .devices
        .get(&device.id)
        .ok_or_else(|| CoordinatorError::Service("selected device is not a host device".into()))?
        .domain;
    // SPEC §13.3: a launch needs a directory on disk; an unresolved source is
    // refused rather than invented.
    let checkpoint_root = effective
        .model
        .require_resolved_path()
        .map_err(|_| {
            CoordinatorError::Service("initialize work resolves to no checkpoint root".into())
        })?
        .to_owned();
    let digest = hex::encode(Sha256::digest(
        serde_json::to_vec(settings)
            .map_err(|_| CoordinatorError::Service("settings encoding failed".into()))?,
    ));
    let metadata = NativeLaunchMetadata {
        engine: "sglang".into(),
        recipe: NATIVE_SGLANG_RECIPE.into(),
        source_revision: NATIVE_SGLANG_SOURCE_REVISION.into(),
        checkpoint_revision: NATIVE_CHECKPOINT_REVISION.into(),
        binding_id: work.binding_id().into(),
        incarnation: work.incarnation().into(),
        endpoint: format!("http://{}", work.endpoint()),
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

/// The private launch descriptor `NativeLaunchHandoff::arm` sends on fd 3.
/// Wire contract with runtime/sglang_entry.py; renamed with the ordinary
/// native launch design. Never log the output: it carries the checkpoint root.
pub fn private_descriptor(
    session_id: &str,
    execution: &StepExecutionContext,
    checkpoint_root: &str,
    public_settings: &serde_json::Value,
    placement_digest: Option<&str>,
) -> Result<Vec<u8>, RuntimeError> {
    let mut descriptor = serde_json::json!({
        "schema_version": 2,
        "kind": "sglang_private_launch",
        "checkpoint_root": checkpoint_root,
        "public_settings": public_settings,
        "launch_scope": {
            "session_id": session_id,
            "deployment_id": execution.token.deployment_id,
            "operation_id": execution.token.operation_id,
            "step_id": execution.token.step_id,
            "revision": execution.token.revision,
            "generation": execution.token.generation,
            "binding_id": execution.binding_id,
            "incarnation": execution.incarnation,
            "issued_at_ms": execution.issued_at_ms,
            "deadline_ms": execution.deadline_ms,
        },
    });
    if let Some(digest) = placement_digest {
        descriptor
            .as_object_mut()
            .ok_or_else(|| RuntimeError::Uncertain("descriptor encoding failed".into()))?
            .insert("placement_digest".into(), serde_json::json!(digest));
    }
    serde_json::to_vec(&descriptor)
        .map_err(|_| RuntimeError::Uncertain("descriptor encoding failed".into()))
}

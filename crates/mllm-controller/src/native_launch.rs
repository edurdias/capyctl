//! Production construction of the frozen native launch and the shared private
//! descriptor. The builder reads only frozen, validated arm input; it grants no
//! send authority and never places checkpoint paths or credential references in
//! an error (closed errors).

use crate::coordinator::CoordinatorError;
use mllm_adapters::traits::RuntimeError;
use mllm_domain::completion::StepExecutionContext;
use mllm_domain::launch::NativeLaunch;
use mllm_store::ordinary_lifecycle::worker::InitializeWork;

/// Builds the frozen native launch for an armed ordinary initialize.
///
/// The SGLang descriptor contract constants name the entry's wire shape only
/// (ADR 0008: no pinned build is audited); the store work must name that engine.
/// The served name is the deployment's own route name, which the caller takes
/// from `work.effective().routes.first()` and refuses to omit. Anything else
/// is refused closed rather than adapted.
pub fn frozen_from_work(
    work: &InitializeWork,
    served_name: String,
    inference_ref: String,
    admin_ref: String,
) -> Result<NativeLaunch, CoordinatorError> {
    mllm_adapters::sglang::frozen_from_effective(
        work.effective(),
        work.binding_id(),
        work.incarnation(),
        work.endpoint(),
        served_name,
        inference_ref,
        admin_ref,
    )
    .map_err(|error| CoordinatorError::Service(error.to_string()))
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

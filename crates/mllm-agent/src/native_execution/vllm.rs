//! SPEC §§3, 6.1, 9.1, 13.3: remote single-rank vLLM on the host agent.
//!
//! The controller sends a frozen deployment document and a leased port, never
//! argv or environment. The agent resolves the launch from its own approved
//! host document through the same builder the embedded coordinator uses
//! (`mllm_adapters::vllm::plan_from_effective`, the live-proven S1 recipe), and
//! launches it with the same `VllmAdapter` Initialize step, through the durable
//! journal's gated process tools. The per-launch engine key is the host's
//! protected native inference credential; it reaches the child only through
//! `VLLM_API_KEY` and is never journaled, rendered on argv, or logged.
//!
//! The launch tail (ingress, Initialize, durable readiness, gate) and the fresh
//! readiness probe are engine-agnostic and live in the parent module; this
//! module supplies only what is vLLM's own: the recipe, its guard and adapter.
use super::NativeHostExecution;
use crate::{ingress_identity::NativeCredentials, journal::JournalError, session::SessionError};
use mllm_adapters::vllm::{
    park_policy, plan_from_effective, PlanInputVllm, VllmAdapter, VLLM_ENTRY,
};
use mllm_config::effective::EffectiveDeployment;
use mllm_protocol::execution::SingleLaunchPlan;
use std::path::Path;

/// mllm's own middleware that keys vLLM's development routes (Spec §3). It is
/// imported by the engine over `PYTHONPATH` from the host runtime directory.
const GUARD_MODULE: &str = "mllm_vllm_guard.py";

/// SPEC §9.1 / T21: a development-mode engine without its guard would serve
/// sleep, wake and collective routes unkeyed. A regular file only: a symlink
/// would let something outside the approved runtime directory be imported.
fn guard_present(runtime_dir: &Path) -> bool {
    regular_file(&runtime_dir.join(GUARD_MODULE))
}

fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

impl NativeHostExecution {
    /// The launch plan for this host, from locally resolved policy only.
    pub(super) fn vllm_plan(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
    ) -> Result<PlanInputVllm, JournalError> {
        let launch = plan_from_effective(
            effective,
            plan.service_port,
            // One engine log per incarnation, beside the SGLang engine logs.
            self.log_dir
                .join(format!("{}.log", plan.incarnation))
                .to_string_lossy()
                .into_owned(),
            self.runtime_dir.to_string_lossy().into_owned(),
        )
        .map_err(|_| JournalError::Unauthorized)?;
        // SPEC §9.1 / T21: refuse a development-mode launch this host cannot
        // guard, at authorization time, before anything durable is recorded.
        if !launch.sleep_flags.is_empty() && !guard_present(&self.runtime_dir) {
            return Err(JournalError::Unauthorized);
        }
        // ADR 0014 §6 / Q11: every vLLM launch runs through the protected entry,
        // which refuses reserved overrides with vLLM's own parser. A host
        // without it (a regular file, never a symlink out of the approved
        // directory) is refused before anything durable is recorded.
        if !regular_file(&self.runtime_dir.join(VLLM_ENTRY)) {
            return Err(JournalError::Unauthorized);
        }
        Ok(launch)
    }

    /// Authorization-time admission of a resolved vLLM launch: the plan must
    /// build from the approved profile, and development mode needs its guard.
    pub(super) fn admit_vllm(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
    ) -> Result<(), JournalError> {
        self.vllm_plan(effective, plan).map(|_| ())
    }

    /// The vLLM adapter for this launch's loopback engine. SPEC §13.3: the engine
    /// guards `/v1` with exactly the key ingress presents, so routed inference,
    /// the launch readiness probe and a later fresh probe all share it.
    pub(super) fn vllm_adapter(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        keys: &NativeCredentials,
        served: &str,
    ) -> Result<VllmAdapter, SessionError> {
        let endpoint: reqwest::Url = format!("http://{}", Self::endpoint(plan))
            .parse()
            .map_err(|_| SessionError)?;
        Ok(VllmAdapter::new(
            endpoint,
            None,
            effective.profile.build_fingerprint.clone(),
            park_policy(effective),
            served.into(),
        )
        .with_engine_key(hex::encode(keys.inference))
        // SPEC §9.1 / T21: the development routes are keyed with the launch's
        // admin credential, apart from the inference key ingress presents.
        .with_admin_key(hex::encode(keys.admin)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    // T21 T37: only a regular guard module in the runtime directory counts.
    #[test]
    fn the_guard_must_be_a_regular_file_in_the_runtime_directory() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path();
        assert!(!guard_present(runtime));
        let elsewhere = directory.path().join("elsewhere.py");
        std::fs::write(&elsewhere, "").unwrap();
        std::os::unix::fs::symlink(&elsewhere, runtime.join(GUARD_MODULE)).unwrap();
        assert!(!guard_present(runtime));
        std::fs::remove_file(runtime.join(GUARD_MODULE)).unwrap();
        std::fs::create_dir(runtime.join(GUARD_MODULE)).unwrap();
        assert!(!guard_present(runtime));
        std::fs::remove_dir(runtime.join(GUARD_MODULE)).unwrap();
        std::fs::write(runtime.join(GUARD_MODULE), "").unwrap();
        std::fs::set_permissions(
            runtime.join(GUARD_MODULE),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(guard_present(runtime));
    }
}

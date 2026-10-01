//! Turning a frozen binding into the adapter spec its engine family needs.
//!
//! The coordinator deliberately knows nothing about credentials or launch plans, so
//! this supplies them. It reads the binding's own frozen profile rather than any
//! ambient configuration: the profile is what the deployment was admitted against,
//! and reading anything else would let a runtime drift from what was qualified.
//!
//! Only families that can be built honestly are built. A family whose production
//! prerequisites are missing is refused by name, because constructing it with
//! plausible placeholders would produce a runtime that looks configured and is not.

use std::path::PathBuf;

use capyctl_adapters::resolve::AdapterSpec;
use capyctl_adapters::vllm::args::PlanInputVllm;
use capyctl_config::engine_policy::Engine;
use capyctl_store::ordinary_lifecycle::worker::InitializeWork;

use crate::coordinator::{CoordinatorError, EngineBindings};

/// Builds adapter specs from frozen bindings.
pub struct ProfileBindings {
    log_dir: PathBuf,
    runtime_dir: PathBuf,
    /// ADR 0014 §7 (WE3): this host's checkpoint digests and stat cache.
    checkpoints: std::sync::Arc<capyctl_agent::checkpoint::CheckpointVerifier>,
    /// SPEC §9.2 (W5): the saver observation source a memory-saver SGLang
    /// launch is parked and restored on. Without one its Park is refused.
    saver: Option<std::sync::Arc<dyn capyctl_agent::native_execution::SaverResidency>>,
    /// SPEC §8.2 / T21 (owner decision 2026-09-25): the private root SGLang
    /// launches keep their file rendezvous in, as on a host. Without one the
    /// entry falls back to its own temporary directory.
    rendezvous: Option<capyctl_agent::rendezvous::RendezvousRoot>,
    /// ADR 0023 §3: the private root TensorFold launches build extensions in.
    engine_cache: Option<capyctl_agent::engine_cache::EngineCacheRoot>,
    /// Discrete GPU design §6 (ADR 0019): the total memory of each discrete
    /// GPU on this host, by driver index, as sampled at boot. An engine on a
    /// device domain is sized against its card's total; a card's total does not
    /// change while the host runs.
    device_totals: std::collections::BTreeMap<u32, i64>,
}

impl ProfileBindings {
    /// `log_dir` is where the launcher writes each engine's own output, and
    /// `runtime_dir` holds capyctl's guard middleware. Neither is part of the frozen
    /// effective configuration, because both are properties of this installation
    /// rather than of the deployment that was admitted.
    ///
    /// Nothing else is taken: every other input to a launch comes from the frozen
    /// profile the deployment was admitted against.
    pub fn new(log_dir: PathBuf, runtime_dir: PathBuf) -> Self {
        Self {
            log_dir,
            runtime_dir,
            checkpoints: std::sync::Arc::new(
                capyctl_agent::checkpoint::CheckpointVerifier::in_memory(),
            ),
            saver: None,
            rendezvous: None,
            engine_cache: None,
            device_totals: std::collections::BTreeMap::new(),
        }
    }

    /// Discrete GPU design §6: the total memory of each discrete GPU by driver
    /// index. Without it a launch on a device domain is refused rather than
    /// sized against the wrong memory.
    pub fn with_device_totals(mut self, totals: std::collections::BTreeMap<u32, i64>) -> Self {
        self.device_totals = totals;
        self
    }

    /// This launch's own copy of the frozen effective configuration, stating the
    /// total of the card it runs on when it runs on a device domain.
    fn sized(
        &self,
        work: &InitializeWork,
    ) -> Result<capyctl_config::effective::EffectiveDeployment, CoordinatorError> {
        work.effective()
            .clone()
            .with_device_total(|index| self.device_totals.get(&index).copied())
            .map_err(|error| CoordinatorError::Service(error.to_string()))
    }

    /// ADR 0023 §3: TensorFold launches build their extensions under this
    /// private root (`<dir>/tensorfold/<version>/torch_extensions`). Without
    /// one a TensorFold launch is refused.
    pub fn with_engine_cache_root(mut self, dir: PathBuf) -> Self {
        self.engine_cache = Some(capyctl_agent::engine_cache::EngineCacheRoot::new(dir));
        self
    }

    /// SPEC §8.2 / T21 (owner decision 2026-09-25): SGLang launches keep their
    /// rendezvous in `<dir>/<incarnation>` (the role creates `dir` 0700), and
    /// each launch's directory is removed once its group is proved gone.
    pub fn with_rendezvous_root(mut self, dir: PathBuf) -> Self {
        self.rendezvous = Some(capyctl_agent::rendezvous::RendezvousRoot::new(dir));
        self
    }

    /// SPEC §9.2 (W5): memory-saver SGLang launches enroll their saver
    /// observation in this private directory (0700, this service user), and
    /// their park and restore are observed through it, as on a remote host.
    pub fn with_saver_observation(self, dir: PathBuf) -> Self {
        self.with_saver_residency(std::sync::Arc::new(
            capyctl_agent::native_execution::EnrolledSaver::new(dir),
        ))
    }

    /// The saver source itself (tests supply a fake one).
    pub fn with_saver_residency(
        mut self,
        saver: std::sync::Arc<dyn capyctl_agent::native_execution::SaverResidency>,
    ) -> Self {
        self.saver = Some(saver);
        self
    }

    /// ADR 0014 §7, owner decision Q9: keep the checkpoint stat cache in this
    /// private directory so a restart does not hash unchanged checkpoints again.
    pub fn with_checkpoint_cache(mut self, dir: PathBuf) -> Self {
        self.checkpoints =
            std::sync::Arc::new(capyctl_agent::checkpoint::CheckpointVerifier::with_cache_dir(dir));
        self
    }

    /// The verifier this host measures checkpoints with, shared with the
    /// digest supervisor so both use one cache.
    pub fn checkpoints(&self) -> std::sync::Arc<capyctl_agent::checkpoint::CheckpointVerifier> {
        self.checkpoints.clone()
    }

    fn refuse(what: impl std::fmt::Display) -> CoordinatorError {
        CoordinatorError::Service(format!("cannot build a vLLM launch plan: {what}"))
    }

    /// The launch plan for one vLLM start, built from the frozen effective
    /// configuration and the binding's own leased endpoint (Spec §3). Nothing here
    /// is read from the environment: a plan that differed from what the deployment
    /// was admitted against would be a runtime nobody reviewed.
    fn vllm_plan(
        &self,
        work: &InitializeWork,
        effective: &capyctl_config::effective::EffectiveDeployment,
    ) -> Result<PlanInputVllm, CoordinatorError> {
        let endpoint = crate::port::engine_url(work.endpoint()).ok_or_else(|| {
            Self::refuse(format!("endpoint names no address: {}", work.endpoint()))
        })?;
        let port = endpoint
            .port()
            .ok_or_else(|| Self::refuse("the leased endpoint names no port"))?;
        // Spec §3: the one shared builder the remote host agent also renders
        // through, so the embedded and remote recipes cannot drift apart.
        capyctl_adapters::vllm::plan_from_effective(
            effective,
            port,
            self.log_dir
                .join(&work.fence().deployment_id)
                .join(format!("{}.log", work.incarnation()))
                .to_string_lossy()
                .into_owned(),
            self.runtime_dir.to_string_lossy().into_owned(),
        )
        .map_err(Self::refuse)
    }

    /// ADR 0023 §3: the embedded TensorFold plan through the shared builder,
    /// with the version's private extensions directory, and whether an
    /// earlier start left a build there.
    fn tensorfold_plan(
        &self,
        work: &InitializeWork,
        effective: &capyctl_config::effective::EffectiveDeployment,
    ) -> Result<(capyctl_adapters::tensorfold::PlanInputTensorfold, bool), CoordinatorError> {
        let refuse = |what: String| {
            CoordinatorError::Service(format!("cannot build a TensorFold launch plan: {what}"))
        };
        let endpoint = crate::port::engine_url(work.endpoint())
            .ok_or_else(|| refuse(format!("endpoint names no address: {}", work.endpoint())))?;
        let port = endpoint
            .port()
            .ok_or_else(|| refuse("the leased endpoint names no port".into()))?;
        // ADR 0023 §3, SPEC §13.3: without a private root nothing is launched.
        let cache = self
            .engine_cache
            .as_ref()
            .ok_or_else(|| refuse("no private engine cache".into()))?;
        let dir = cache
            .torch_extensions(&effective.profile.build_fingerprint)
            .map_err(|error| refuse(error.to_string()))?;
        let built = capyctl_agent::engine_cache::has_build(&dir);
        let plan = capyctl_adapters::tensorfold::plan_from_effective(
            effective,
            port,
            self.log_dir
                .join(&work.fence().deployment_id)
                .join(format!("{}.log", work.incarnation()))
                .to_string_lossy()
                .into_owned(),
            Some(dir.to_string_lossy().into_owned()),
        )
        .map_err(|error| refuse(error.to_string()))?;
        Ok((plan, built))
    }
}

impl EngineBindings for ProfileBindings {
    /// SPEC §8.2 / T21: a signalled stop never runs the entry's exit handler,
    /// so the gone launch's rendezvous directory is removed here, on the same
    /// verified cleanup evidence a host removes it on.
    fn launch_gone(&self, incarnation: &str) {
        if let Some(root) = &self.rendezvous {
            root.retire(incarnation);
        }
    }

    fn checkpoint_verifier(
        &self,
    ) -> Option<std::sync::Arc<capyctl_agent::checkpoint::CheckpointVerifier>> {
        Some(self.checkpoints.clone())
    }

    fn spec(&self, work: &InitializeWork) -> Result<AdapterSpec, CoordinatorError> {
        let effective = work.effective();
        let profile = &effective.profile;
        match profile.engine {
            Engine::Vllm => {
                // The frozen binding records the authority the lease reserved, not a
                // URL; the adapter talks HTTP to it over loopback (Spec §3).
                let endpoint = crate::port::engine_url(work.endpoint()).ok_or_else(|| {
                    CoordinatorError::Service(format!(
                        "frozen binding endpoint names no address: {}",
                        work.endpoint()
                    ))
                })?;
                Ok(AdapterSpec::Vllm {
                    endpoint,
                    // The adapter talks to the engine with the same per-launch key
                    // the child is given, and the driver factory stores it before
                    // the builder is constructed.
                    api_key: None,
                    fingerprint: profile.build_fingerprint.clone(),
                    // SPEC §9.1 / T21 / ADR 0012: deep parking is on unless the
                    // host opts out, and SPEC §6.2 keeps sleep calls away from a
                    // restart-only deployment. The same rule the host agent uses,
                    // so the embedded and remote paths cannot disagree.
                    policy: capyctl_adapters::vllm::park_policy(effective),
                    // Readiness is the served id appearing in the engine's model
                    // list, so the adapter must use the route the deployment serves
                    // rather than the checkpoint path.
                    model_id: effective
                        .routes
                        .first()
                        .cloned()
                        .unwrap_or_else(|| effective.name.clone()),
                    // Discrete GPU design §6: sized against the card's total.
                    launch: Some(self.vllm_plan(work, &self.sized(work)?)?),
                    // SPEC §13.3: local-only is not unauthenticated. One fresh key
                    // per launch, hex so it survives an environment variable; the
                    // driver factory seals it under the binding before the builder
                    // is handed it.
                    engine_key: Some(hex::encode(capyctl_store::secrets::new_engine_key())),
                    // SPEC §9.1 / T21, ADR 0012: a second fresh key for the
                    // admin role, sealed beside the inference key like SGLang's
                    // two roles. The guard keys the development and control
                    // routes with it alone, so the inference key ingress holds
                    // cannot sleep, wake or reload the engine. The remote host
                    // agent keys its vLLM launches the same way.
                    admin_key: Some(hex::encode(capyctl_store::secrets::new_engine_key())),
                    // ADR 0010, discrete GPU design §5: `host_backed` parks at
                    // sleep level 1, fixed by the frozen residency.
                    residency: effective.residency,
                })
            }
            Engine::Sglang => {
                // The pinned native builder refuses anything the frozen profile
                // does not name, and maps its refusals onto this error type.
                // The credential references name the two fresh keys this spec
                // carries, so the store rows the factory seals can be traced
                // back to the launch that used them.
                let binding = work.binding_id();
                let inference_ref = format!("sglang-inference-{binding}");
                let admin_ref = format!("sglang-admin-{binding}");
                // The served name is the deployment's route, the same rule the
                // vLLM branch follows (Spec §3): clients request the route the
                // deployment serves, never a derived binding artifact.
                let served_name = effective.routes.first().cloned().ok_or_else(|| {
                    CoordinatorError::Service("the deployment serves no route".into())
                })?;
                // Discrete GPU design §6: sized against the card's total.
                let frozen = Box::new(crate::native_launch::frozen_for_launch(
                    work,
                    &self.sized(work)?,
                    served_name,
                    inference_ref.clone(),
                    admin_ref.clone(),
                )?);
                // SPEC §9.2 (W5): a memory-saver launch is parked and restored
                // through the saver observation it enrolls; a restart-only one
                // has nothing to observe and never parks.
                let observer = match (&self.saver, frozen.settings().memory_saver) {
                    (Some(saver), true) => Some(std::sync::Arc::new(
                        capyctl_agent::native_execution::LaunchSglangObserver::new(saver.clone()),
                    )
                        as std::sync::Arc<dyn capyctl_adapters::sglang::SglangRuntimeObserver>),
                    _ => None,
                };
                Ok(AdapterSpec::Sglang {
                    frozen,
                    // SPEC §13.3: one fresh key per role, hex so it survives the
                    // factory's decode-and-seal; the resolved-spawn factory
                    // stores both under the binding before the builder runs.
                    inference: hex::encode(capyctl_store::secrets::new_engine_key()),
                    admin: hex::encode(capyctl_store::secrets::new_engine_key()),
                    observer,
                    // The wrapper is capyctl's own protected entrypoint in this
                    // installation's runtime directory, the same directory that
                    // holds the vLLM guard middleware. Rendering revalidates the
                    // path immediately before use, so a directory that does not
                    // carry it refuses the launch rather than half-running.
                    wrapper: Some(self.runtime_dir.join("sglang_entry.py")),
                    // The engine's own output, one file per incarnation, next
                    // to every other engine's log.
                    log: Some(
                        self.log_dir
                            .join(&work.fence().deployment_id)
                            .join(format!("{}.log", work.incarnation()))
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    // The coordinator session is not the bindings' to know; the
                    // resolved-spawn factory threads it under the owner lock.
                    session: None,
                    // ADR 0014 §8, SPEC §8.2: the host's approvals, which the
                    // entry applies to what the extras resolve to.
                    extra_approvals: Some(capyctl_config::engine_policy::extra_approvals_document(
                        &effective.profile.security.approved_options,
                        &effective.profile.security.approved_paths,
                        effective.profile.security.trust_remote_code,
                    )),
                    // SPEC §8.2 / T21: the launch's rendezvous directory in
                    // the private root, never the entry's `/tmp` fallback
                    // while the root is private.
                    rendezvous: self
                        .rendezvous
                        .as_ref()
                        .and_then(|root| root.launch_dir(work.incarnation())),
                })
            }
            Engine::Tensorfold => {
                let endpoint = crate::port::engine_url(work.endpoint()).ok_or_else(|| {
                    CoordinatorError::Service(format!(
                        "frozen binding endpoint names no address: {}",
                        work.endpoint()
                    ))
                })?;
                let (launch, extensions_built) = self.tensorfold_plan(work, &self.sized(work)?)?;
                Ok(AdapterSpec::Tensorfold {
                    endpoint,
                    fingerprint: profile.build_fingerprint.clone(),
                    model_id: effective
                        .routes
                        .first()
                        .cloned()
                        .unwrap_or_else(|| effective.name.clone()),
                    launch: Some(launch),
                    extensions_built,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests;

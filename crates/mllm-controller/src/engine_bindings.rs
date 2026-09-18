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

use std::path::{Path, PathBuf};

use mllm_adapters::resolve::AdapterSpec;
use mllm_adapters::vllm::args::{GrantedBudget, PlanInputVllm};
use mllm_adapters::ParkPolicy;
use mllm_config::engine_policy::Engine;
use mllm_domain::launch::{ProfileLaunchSettings, VllmLaunchSettings};
use mllm_store::ordinary_lifecycle::worker::InitializeWork;

use crate::coordinator::{CoordinatorError, EngineBindings};

/// The startup flags that put vLLM into the development mode its park controls
/// live behind, with the eager checkpoint loader this project qualified on Spark:
/// mmap-backed tensor copies during weight restoration are what the eager strategy
/// avoids. Rendered only when the profile asks for sleep mode and the host has not
/// opted out of deep park (Spec §3).
fn sleep_flags(settings: &VllmLaunchSettings, deep_park_enabled: bool) -> Vec<String> {
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

/// Builds adapter specs from frozen bindings.
pub struct ProfileBindings {
    log_dir: PathBuf,
    runtime_dir: PathBuf,
}

impl ProfileBindings {
    /// `log_dir` is where the launcher writes each engine's own output, and
    /// `runtime_dir` holds mllm's guard middleware. Neither is part of the frozen
    /// effective configuration, because both are properties of this installation
    /// rather than of the deployment that was admitted.
    ///
    /// Nothing else is taken: every other input to a launch comes from the frozen
    /// profile the deployment was admitted against.
    pub fn new(log_dir: PathBuf, runtime_dir: PathBuf) -> Self {
        Self {
            log_dir,
            runtime_dir,
        }
    }

    fn missing(family: &str, what: &str) -> CoordinatorError {
        CoordinatorError::Service(format!(
            "cannot build a {family} runtime yet: {what}. Refusing rather than \
             constructing one that looks configured and is not"
        ))
    }

    fn refuse(what: impl std::fmt::Display) -> CoordinatorError {
        CoordinatorError::Service(format!("cannot build a vLLM launch plan: {what}"))
    }

    /// The launch plan for one vLLM start, built from the frozen effective
    /// configuration and the binding's own leased endpoint (Spec §3). Nothing here
    /// is read from the environment: a plan that differed from what the deployment
    /// was admitted against would be a runtime nobody reviewed.
    fn vllm_plan(&self, work: &InitializeWork) -> Result<PlanInputVllm, CoordinatorError> {
        let effective = work.effective();
        let profile = &effective.profile;
        let ProfileLaunchSettings::Vllm(settings) = &profile.launch_settings else {
            return Err(Self::refuse(
                "the frozen profile declares vLLM but carries another family's launch settings",
            ));
        };
        let endpoint = crate::port::engine_url(work.endpoint()).ok_or_else(|| {
            Self::refuse(format!("endpoint names no address: {}", work.endpoint()))
        })?;
        let port = endpoint
            .port()
            .ok_or_else(|| Self::refuse("the leased endpoint names no port"))?;
        let served_model_name = effective
            .routes
            .first()
            .cloned()
            .ok_or_else(|| Self::refuse("the deployment serves no route"))?;
        let model_path = effective
            .model
            .require_resolved_path()
            .map_err(Self::refuse)?
            .to_owned();
        let budget = &settings.requested_budget;
        Ok(PlanInputVllm {
            engine_bin: profile.executable.clone(),
            // The engine's own bin directory has to be on PATH: the JIT compile
            // step shells out to the tools that were installed beside it.
            engine_path_extra: Path::new(&profile.executable)
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(|parent| parent.to_string_lossy().into_owned()),
            model_path,
            port,
            served_model_name,
            tensor_parallel_size: settings.tensor_parallel_size,
            pipeline_parallel_size: settings.pipeline_parallel_size,
            kv_cache_dtype: settings.kv_cache_dtype.clone(),
            block_size_tokens: settings.block_size_tokens,
            cpu_offload_bytes: settings.cpu_offload_bytes,
            // A requested budget is a request; deploy-time validation already
            // refused one larger than the Ready allocation the deployment
            // declared, so the engine's pool is never sized above what admission
            // accounted for. Zero means the profile asked for no such bound.
            granted: GrantedBudget {
                kv_cache_bytes: (budget.kv_cache_bytes > 0).then_some(budget.kv_cache_bytes),
                gpu_utilization_pct: (budget.gpu_utilization_pct > 0)
                    .then_some(budget.gpu_utilization_pct),
                swap_space_bytes: (budget.swap_space_bytes > 0).then_some(budget.swap_space_bytes),
            },
            engine_args: profile.args.clone(),
            // SPEC §3: deep park is one switch. A host that opted out never
            // launches a development-mode engine, whatever the profile asked for.
            sleep_flags: sleep_flags(settings, profile.security.deep_park.is_enabled()),
            // SPEC §13.3: the engine key travels in the child's environment. The
            // renderer must never see one, so it can never reach argv.
            api_key: None,
            engine_log: Some(
                self.log_dir
                    .join(&work.fence().deployment_id)
                    .join(format!("{}.log", work.incarnation()))
                    .to_string_lossy()
                    .into_owned(),
            ),
            runtime_dir: Some(self.runtime_dir.to_string_lossy().into_owned()),
        })
    }
}

impl EngineBindings for ProfileBindings {
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
                    // Deep-park paths stay denied unless the host policy opts in
                    // (SPEC §9.1, T21). The profile carries that decision; it is not
                    // re-derived from anything ambient.
                    policy: if profile.security.deep_park.is_enabled() {
                        ParkPolicy::Enabled
                    } else {
                        ParkPolicy::Disabled
                    },
                    // Readiness is the served id appearing in the engine's model
                    // list, so the adapter must use the route the deployment serves
                    // rather than the checkpoint path.
                    model_id: effective
                        .routes
                        .first()
                        .cloned()
                        .unwrap_or_else(|| effective.name.clone()),
                    launch: Some(self.vllm_plan(work)?),
                    // SPEC §13.3: local-only is not unauthenticated. One fresh key
                    // per launch, hex so it survives an environment variable; the
                    // driver factory seals it under the binding before the builder
                    // is handed it.
                    engine_key: Some(hex::encode(mllm_store::secrets::new_engine_key())),
                })
            }
            Engine::Sglang => Err(Self::missing(
                "SGLang",
                "its controls need a resolved admin credential and a trusted \
                 observation socket, and neither is wired in production",
            )),
        }
    }
}

#[cfg(test)]
mod tests;

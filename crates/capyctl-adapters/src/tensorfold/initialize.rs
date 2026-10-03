//! ADR 0023 §4: the TensorFold Initialize step: render, spawn through the
//! director's tool, wait for `/health` and the model list while watching the
//! process, probe once, and report the API process.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use capyctl_domain::completion::{EffectObservation, ExecutionIdentities, Milestone, Presence};

use super::{
    adapter::TensorfoldAdapter,
    args::{engine_environment, render_command},
};
use crate::traits::{EngineAdapter, MemberRef, Readiness, RuntimeCommand, RuntimeError};
use crate::vllm::args::redact_text;

const READINESS_POLL: Duration = Duration::from_millis(500);
const BUILDER_MARGIN_MS: i64 = 2_000;

fn now_ms() -> Result<i64, RuntimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RuntimeError::Uncertain("system clock is before the epoch".into()))
        .map(|d| d.as_millis() as i64)
}

pub(super) async fn initialize(
    adapter: &TensorfoldAdapter,
    command: &RuntimeCommand,
) -> Result<EffectObservation, RuntimeError> {
    let context = &command.context;
    let (plan, tools) = adapter.launch_parts()?;
    if !matches!(
        context.launch_settings,
        Some(capyctl_domain::launch::LaunchSettings::Tensorfold(_))
    ) || !matches!(context.identities, ExecutionIdentities::OwnedLaunch)
    {
        return Err(RuntimeError::Unsupported);
    }
    adapter.claim_incarnation(&context.binding_id, &context.incarnation)?;
    let mut cmd =
        render_command(&plan).map_err(|e| RuntimeError::Uncertain(format!("render: {e}")))?;
    let (toolchain, limits) = crate::engine_env::toolchain_environment(
        plan.cuda_home.as_deref(),
        &plan.build_env,
        crate::engine_env::mem_available_bytes(),
        crate::engine_env::cpu_count(),
    );
    capyctl_domain::role_log::notice(
        capyctl_domain::role_log::Level::Notice,
        &format!("{limits} (binding {})", context.binding_id),
    );
    cmd.env = engine_environment(
        &cmd.env,
        &plan,
        &|name| std::env::var(name).ok(),
        &toolchain,
    );
    let incarnation = context.incarnation.clone();
    let spawn_tools = tools.clone();
    let api = tokio::task::spawn_blocking(move || spawn_tools.spawn_durable(&incarnation, &cmd))
        .await
        .map_err(|_| RuntimeError::Uncertain("spawn task failed".into()))??;
    // ADR 0014 amendment A12: note any kernel build until the step ends.
    let builds = crate::kernel_builds::BuildWatch::start(tools.clone(), api.clone());
    // ADR 0023 §4: the ordinary bound once a build exists, the
    // whole (first-build) deadline otherwise.
    let deadline = context.deadline_ms.saturating_sub(BUILDER_MARGIN_MS);
    let warm = adapter.extensions_built();
    let stop_at = if warm {
        deadline.min(now_ms()?.saturating_add(plan.warm_startup_ms))
    } else {
        deadline
    };
    let member = MemberRef {
        deployment_id: context.token.deployment_id.clone(),
        member_id: context.binding_id.clone(),
    };
    loop {
        match adapter.check_readiness(&member).await {
            Ok(Readiness::Ready) => break,
            Ok(Readiness::Initializing) => {}
            Err(e) => {
                return Err(RuntimeError::Uncertain(redact_text(&format!(
                    "readiness: {e}"
                ))))
            }
        }
        let presence_tools = tools.clone();
        let watched = api.clone();
        match tokio::task::spawn_blocking(move || presence_tools.present(&watched))
            .await
            .map_err(|_| RuntimeError::Uncertain("presence task failed".into()))?
        {
            Presence::Alive => {}
            Presence::Gone => {
                let tail = crate::launch_failure::log_tail(plan.engine_log.as_deref());
                return Err(RuntimeError::LaunchFailed(format!(
                    "{}; log tail:\n{tail}",
                    crate::launch_failure::summary(&tail, None)
                )));
            }
            Presence::Unknown => {
                return Err(RuntimeError::Uncertain(
                    "engine presence could not be established during readiness".into(),
                ))
            }
        }
        if now_ms()? >= stop_at {
            return Err(RuntimeError::Uncertain(if warm && stop_at < deadline {
                "the ordinary startup bound passed with the engine alive (its kernels were already built)".into()
            } else {
                "readiness deadline reached with the engine alive (the first start builds CUDA kernels)".into()
            }));
        }
        tokio::time::sleep(READINESS_POLL).await;
    }
    let body = json!({
        "model": plan.served_model_name,
        "messages": [{"role": "user", "content": "Say ready."}],
        "max_tokens": 8,
        "temperature": 0,
    });
    // ADR 0023 §4: the probe is part of startup, so the startup bound (the warm
    // bound, or the whole deadline on a first build) is its only bound.
    let budget = Duration::from_millis(u64::try_from(stop_at - now_ms()?).unwrap_or(0));
    let answer = crate::forward::startup_probe(adapter, &body, budget)
        .await
        .ok_or_else(|| {
            RuntimeError::Uncertain("probe deadline reached with the engine alive".into())
        })?
        .map_err(|e| {
            RuntimeError::Uncertain(redact_text(&format!(
                "engine listed the model but did not answer: {e}"
            )))
        })?;
    if !crate::forward::probe_answered(&answer) {
        return Err(RuntimeError::Uncertain(
            "engine answered with empty content".into(),
        ));
    }
    let group_tools = tools.clone();
    let led_by = api.clone();
    let identities = tokio::task::spawn_blocking(move || group_tools.observe_group(&led_by))
        .await
        .map_err(|_| RuntimeError::Uncertain("group task failed".into()))??;
    // ADR 0023 §6: TensorFold 0.6.0 serves from one process.
    if identities.first().map(|i| i.role.as_str()) != Some("api") {
        return Err(RuntimeError::Uncertain(
            "engine group has no API process".into(),
        ));
    }
    Ok(EffectObservation {
        token: context.token.clone(),
        binding_id: context.binding_id.clone(),
        incarnation: context.incarnation.clone(),
        identities,
        observed_at_ms: now_ms()?,
        receipt: format!(
            "tensorfold {} ready on {}; probe answered",
            adapter.fingerprint(),
            adapter.endpoint()
        ),
        facts: vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ],
        kernel_builds: builds.finish(),
    })
}

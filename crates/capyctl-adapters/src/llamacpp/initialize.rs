//! ADR 0029 §6, §10: the llama.cpp Initialize step: refuse a machine-wide
//! `config.ini`, render, spawn through the director's tool, wait for `/health`
//! and the model list while watching the process, compare the slot settings
//! llama-server reports with the rendered ones, probe once, and report the
//! process.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use capyctl_config::llamacpp::{slot_pool_tokens, system_config_refusal};
use capyctl_domain::completion::{EffectObservation, ExecutionIdentities, Milestone, Presence};

use super::{
    adapter::LlamacppAdapter,
    args::{engine_environment, render_command, PlanInputLlamacpp},
    http::Read,
};
use crate::traits::{EngineAdapter, MemberRef, Readiness, RuntimeCommand, RuntimeError};
use crate::vllm::args::redact_text;

const READINESS_POLL: Duration = Duration::from_millis(500);
const BUILDER_MARGIN_MS: i64 = 2_000;

/// ADR 0029 §6: the closed reason a launch on a machine with
/// `/etc/llama.cpp/config.ini` is refused with, before anything starts.
pub const ENGINE_CONFIG_FILE: &str = "engine_config_file";

/// ADR 0029 §10, SPEC §8.2: the prefix of the failure a launch whose engine
/// reports other slot settings than were rendered ends with.
pub const EFFECTIVE_ARGS_MISMATCH: &str = "effective_args_mismatch";

fn now_ms() -> Result<i64, RuntimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RuntimeError::Uncertain("system clock is before the epoch".into()))
        .map(|d| d.as_millis() as i64)
}

pub(super) async fn initialize(
    adapter: &LlamacppAdapter,
    command: &RuntimeCommand,
) -> Result<EffectObservation, RuntimeError> {
    let context = &command.context;
    let (plan, tools) = adapter.launch_parts()?;
    if !matches!(
        context.launch_settings,
        Some(capyctl_domain::launch::LaunchSettings::Llamacpp(_))
    ) || !matches!(context.identities, ExecutionIdentities::OwnedLaunch)
    {
        return Err(RuntimeError::Unsupported);
    }
    // ADR 0029 §6: llama-server would fill every option the command leaves
    // unset from a machine-wide `config.ini` CapyCTL cannot see, so a launch
    // beside one is refused before any effect.
    if system_config_refusal(adapter.system_root()).is_some() {
        return Err(RuntimeError::Refused(ENGINE_CONFIG_FILE.into()));
    }
    adapter.claim_incarnation(&context.binding_id, &context.incarnation)?;
    let mut cmd =
        render_command(&plan).map_err(|e| RuntimeError::Uncertain(format!("render: {e}")))?;
    cmd.env = engine_environment(&cmd.env, &plan, &|name| std::env::var(name).ok());
    let incarnation = context.incarnation.clone();
    let spawn_tools = tools.clone();
    let api = tokio::task::spawn_blocking(move || spawn_tools.spawn_durable(&incarnation, &cmd))
        .await
        .map_err(|_| RuntimeError::Uncertain("spawn task failed".into()))??;
    // ADR 0029 §9: the undeclared Initialize window (ADR 0014 A1 and A7)
    // bounds the load and the probe alike.
    let stop_at = context.deadline_ms.saturating_sub(BUILDER_MARGIN_MS);
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
            return Err(RuntimeError::Uncertain(
                "readiness deadline reached with the engine alive".into(),
            ));
        }
        tokio::time::sleep(READINESS_POLL).await;
    }
    verify_rendered(adapter, &plan).await?;
    let body = json!({
        "model": plan.served_model_name,
        "messages": [{"role": "user", "content": "Say ready."}],
        "max_tokens": 8,
        "temperature": 0,
    });
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
    // ADR 0029 §10: a non-empty `content` or `reasoning_content` answers.
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
    // ADR 0029 §7: one single-model llama-server process serves.
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
            "llamacpp {} ready on {}; slots verified; probe answered",
            adapter.fingerprint(),
            adapter.endpoint()
        ),
        facts: vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ],
        kernel_builds: Vec::new(),
    })
}

/// ADR 0029 §10, SPEC §8.2: there is no in-process parser to ask, so the
/// reserved values are verified after llama-server parsed them: `/props`
/// reports the rendered slot count with `/metrics` and `/slots` on, and
/// `/v1/models` reports each slot's window as rendered (`pad256` of the
/// context), capped at the model's training context as llama-server caps it.
/// Any difference fails the launch `effective_args_mismatch`; the coordinator
/// then stops it on its recorded processes.
async fn verify_rendered(
    adapter: &LlamacppAdapter,
    plan: &PlanInputLlamacpp,
) -> Result<(), RuntimeError> {
    let mismatch = |what: String| {
        RuntimeError::Uncertain(format!(
            "{EFFECTIVE_ARGS_MISMATCH}: llama-server reports {what}"
        ))
    };
    let unread =
        |e: crate::traits::AdapterError| RuntimeError::Uncertain(redact_text(&format!("{e}")));
    let Read::Answer(props) = adapter.http().props().await.map_err(unread)? else {
        return Err(mismatch("no /props".into()));
    };
    if props.total_slots != Some(u64::from(plan.slots)) {
        return Err(mismatch(format!(
            "total_slots {:?}, capyctl rendered --parallel {}",
            props.total_slots, plan.slots
        )));
    }
    if props.endpoint_metrics != Some(true) || props.endpoint_slots != Some(true) {
        return Err(mismatch(format!(
            "endpoint_metrics {:?} and endpoint_slots {:?}, capyctl rendered --metrics --slots",
            props.endpoint_metrics, props.endpoint_slots
        )));
    }
    let Read::Answer(Some(model)) = adapter
        .http()
        .served_model(adapter.model_id())
        .await
        .map_err(unread)?
    else {
        return Err(mismatch("no /v1/models entry for the served name".into()));
    };
    let window = slot_pool_tokens(plan.context_length, 1)
        .map(u64::from)
        .ok_or_else(|| mismatch("a context capyctl cannot render".into()))?;
    let expected = model.n_ctx_train.map(|train| window.min(train));
    if expected.is_none() || model.n_ctx != expected {
        return Err(mismatch(format!(
            "n_ctx {:?} (n_ctx_train {:?}), capyctl rendered a slot window of {window}",
            model.n_ctx, model.n_ctx_train
        )));
    }
    Ok(())
}

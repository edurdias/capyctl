//! ADR 0031 §4, §5, §7: the custom engine's Initialize step: render the
//! manifest's command, spawn it through the director's tool (its own process
//! group, the launch journal), wait until the leased port accepts a
//! connection and the health path answers 200 while watching the process,
//! probe the API until it answers, then check that every owned process
//! listens on loopback only, and report the processes.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use capyctl_domain::completion::{EffectObservation, ExecutionIdentities, Milestone, Presence};

use super::{
    adapter::CustomAdapter,
    args::{engine_environment, render_command},
    listeners::{check_loopback, ListenerCheck},
};
use crate::traits::{EngineAdapter, MemberRef, Readiness, RuntimeCommand, RuntimeError};
use crate::vllm::args::redact_text;

const READINESS_POLL: Duration = Duration::from_millis(500);
const BUILDER_MARGIN_MS: i64 = 2_000;
/// How long an exposed launch's processes get to leave on SIGTERM before
/// SIGKILL (ADR 0031 §7: the launch is stopped, with gone evidence).
const EXPOSED_GRACE: Duration = Duration::from_secs(5);

fn now_ms() -> Result<i64, RuntimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RuntimeError::Uncertain("system clock is before the epoch".into()))
        .map(|d| d.as_millis() as i64)
}

pub(super) async fn initialize(
    adapter: &CustomAdapter,
    command: &RuntimeCommand,
) -> Result<EffectObservation, RuntimeError> {
    let context = &command.context;
    let (plan, tools) = adapter.launch_parts()?;
    if !matches!(
        context.launch_settings,
        Some(capyctl_domain::launch::LaunchSettings::Custom(_))
    ) || !matches!(context.identities, ExecutionIdentities::OwnedLaunch)
    {
        return Err(RuntimeError::Unsupported);
    }
    // ADR 0031 §2: an unset placeholder or a reserved environment name is
    // refused before anything starts.
    let mut cmd = render_command(&plan).map_err(|error| RuntimeError::Refused(error.code()))?;
    cmd.env = engine_environment(&cmd.env, &plan, &|name| std::env::var(name).ok());
    adapter.claim_incarnation(&context.binding_id, &context.incarnation)?;
    let incarnation = context.incarnation.clone();
    let spawn_tools = tools.clone();
    // ADR 0031 §4, SPEC §8.1: executed directly, never through a shell, in
    // its own process group, recorded before it runs.
    let api = tokio::task::spawn_blocking(move || spawn_tools.spawn_durable(&incarnation, &cmd))
        .await
        .map_err(|_| RuntimeError::Uncertain("spawn task failed".into()))??;
    // ADR 0031 §5: the undeclared Initialize window (ADR 0014 A1) bounds the
    // load and the probe alike.
    let stop_at = context.deadline_ms.saturating_sub(BUILDER_MARGIN_MS);
    let member = MemberRef {
        deployment_id: context.token.deployment_id.clone(),
        member_id: context.binding_id.clone(),
    };
    let present = |identity: capyctl_domain::completion::ProcessIdentity| {
        let tools = tools.clone();
        async move {
            tokio::task::spawn_blocking(move || tools.present(&identity))
                .await
                .map_err(|_| RuntimeError::Uncertain("presence task failed".into()))
        }
    };
    // SPEC §8.1, T13: an engine that exits (a command that daemonizes and
    // returns included) is a failed launch, whatever then answers on the port.
    let exited = || {
        let tail = crate::launch_failure::log_tail(plan.engine_log.as_deref());
        RuntimeError::LaunchFailed(format!(
            "{}; log tail:\n{tail}",
            crate::launch_failure::summary(&tail, None)
        ))
    };
    let mut last_probe: Option<String> = None;
    loop {
        match present(api.clone()).await? {
            Presence::Alive => {}
            Presence::Gone => return Err(exited()),
            Presence::Unknown => {
                return Err(RuntimeError::Uncertain(
                    "engine presence could not be established during readiness".into(),
                ))
            }
        }
        let ready = adapter
            .check_readiness(&member)
            .await
            .map_err(|e| RuntimeError::Uncertain(redact_text(&format!("readiness: {e}"))))?;
        if ready == Readiness::Ready {
            let budget = Duration::from_millis(u64::try_from(stop_at - now_ms()?).unwrap_or(0));
            match probe(adapter, &plan.values.served_name, budget).await {
                Ok(()) => break,
                Err(reason) => last_probe = Some(reason),
            }
        }
        if now_ms()? >= stop_at {
            return Err(RuntimeError::Uncertain(redact_text(&match last_probe {
                Some(reason) => format!(
                    "readiness deadline reached with the engine alive; the last probe: {reason}"
                ),
                None => "readiness deadline reached with the engine alive".into(),
            })));
        }
        tokio::time::sleep(READINESS_POLL).await;
    }
    if present(api.clone()).await? == Presence::Gone {
        return Err(exited());
    }
    let group_tools = tools.clone();
    let led_by = api.clone();
    let identities = tokio::task::spawn_blocking(move || group_tools.observe_group(&led_by))
        .await
        .map_err(|_| RuntimeError::Uncertain("group task failed".into()))??;
    // ADR 0031 §4: one program, its process the group's leader.
    if identities.first().map(|i| i.role.as_str()) != Some("api") {
        return Err(RuntimeError::Uncertain(
            "engine group has no API process".into(),
        ));
    }
    // ADR 0031 §7: after the probe passed and before admission opens.
    let root = adapter.proc_root().to_path_buf();
    let checked = identities.clone();
    let check_tools = tools.clone();
    let check = tokio::task::spawn_blocking(move || {
        check_loopback(&root, &checked, &|identity| check_tools.present(identity))
    })
    .await
    .map_err(|_| RuntimeError::Uncertain("listener task failed".into()))?;
    match check {
        Ok(()) => {}
        Err(exposed @ ListenerCheck::Exposed(_)) => {
            // ADR 0031 §7: the launch is stopped, with gone evidence, and
            // fails naming the address. Where these tools cannot stop it
            // (a host agent's launch tools confer no cleanup authority),
            // the caller stops it before it reports the failure.
            let stop_tools = tools.clone();
            let stopping = identities.clone();
            let stopped = tokio::task::spawn_blocking(move || {
                stop_tools.terminate_owned(&stopping, EXPOSED_GRACE)
            })
            .await
            .map_err(|_| RuntimeError::Uncertain("stop task failed".into()))?;
            return Err(RuntimeError::LaunchFailed(format!(
                "{exposed}{}",
                if stopped.is_ok() {
                    "; the engine was stopped"
                } else {
                    ""
                }
            )));
        }
        Err(unreadable) => return Err(RuntimeError::Uncertain(unreadable.to_string())),
    }
    Ok(EffectObservation {
        token: context.token.clone(),
        binding_id: context.binding_id.clone(),
        incarnation: context.incarnation.clone(),
        identities,
        observed_at_ms: now_ms()?,
        receipt: format!(
            "custom engine {} ready on {}; probe answered; listeners on loopback",
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

/// ADR 0031 §5: the existing probe of the profile's API, unchanged: the
/// fixed decision of ADR 0030 §3 for `decisions` (never a chat probe in its
/// place), the bounded startup chat probe for `chat`; the served name as
/// `model`, the key when the manifest uses one. The reason it failed, if it
/// did.
async fn probe(adapter: &CustomAdapter, served: &str, budget: Duration) -> Result<(), String> {
    if adapter.decisions() {
        return match tokio::time::timeout(budget, adapter.decide(served, budget)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(format!("the decision probe was not answered: {e}")),
            Err(_) => Err("the decision probe outlasted the startup window".into()),
        };
    }
    let body = json!({
        "model": served,
        "messages": [{"role": "user", "content": "Say ready."}],
        "max_tokens": 8,
        "temperature": 0,
    });
    match crate::forward::startup_probe(adapter, &body, budget).await {
        None => Err("the chat probe outlasted the startup window".into()),
        Some(Err(e)) => Err(format!("the chat probe was not answered: {e}")),
        // ADR 0023 §6: a non-empty content or reasoning answers.
        Some(Ok(answer)) if crate::forward::probe_answered(&answer) => Ok(()),
        Some(Ok(_)) => Err("the engine answered the chat probe with empty content".into()),
    }
}

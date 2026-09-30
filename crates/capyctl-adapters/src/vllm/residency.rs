//! SPEC §§9.1, 10, 13.2: vLLM's persisted residency steps for a park.
//!
//! A level-2 (`deep`) park discards weights and KV while the process group
//! stays owned. A level-1 (`host_backed`, discrete GPU design §5) park copies
//! the weights to pinned host RAM and drops KV; its `Park` calls
//! `POST /sleep?level=1`, and its `ReloadWeights` makes no engine call (the
//! weights wake already copied them back), so the step sequence is the same.
//! Restoration follows SPEC §9.1 in order, one persisted step each, so every
//! step carries its own evidence and a failure names where it stopped:
//!
//! | step              | engine call                                      | milestone             |
//! |-------------------|--------------------------------------------------|-----------------------|
//! | `Park`            | `POST /sleep?level=2`, then `/is_sleeping` true  | `MemoryReleased`      |
//! | `Restore`         | `POST /wake_up?tags=weights`                     | `AllocationsRestored` |
//! | `ReloadWeights`   | `POST /collective_rpc {"method":"reload_weights"}` | `WeightsUsable`     |
//! | `InvalidateCache` | `POST /wake_up?tags=kv_cache`, `POST /reset_prefix_cache`, `/is_sleeping` false | `CacheValid` |
//!
//! | `Probe`           | one authenticated chat completion answered       | `ModelUsable`         |
//!
//! None of the first four is model readiness: the caller proves a usable model
//! with the fresh `Probe` after the last step (SPEC §6.1). Waking allocations
//! alone is not successful restoration (SPEC §9.1).
//!
//! Error contract. `Unsupported` means no engine call was made: the policy
//! gate, the step's shape, its deadline or the adapter's fence refused it.
//! `Uncertain` means an engine call was dispatched and its effect is unknown;
//! the adapter then refuses every further residency step, so a partial
//! restoration is never repeated blindly (SPEC §13.2, T20).
use crate::traits::{MemberRef, RuntimeAction, RuntimeCommand, RuntimeError};
use crate::vllm::{
    adapter::VllmAdapter,
    http::{HttpError, WakeTag},
};
use capyctl_domain::completion::{EffectObservation, ExecutionIdentities, Milestone};

/// ADR 0010, ADR 0019: the park level follows the deployment's declared
/// residency, fixed at launch; it is never chosen at park time. `host_backed`
/// is refused on unified pools by resolution (ADR 0010 decision 5), so level 1
/// runs only where device and host memory are distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParkLevel {
    HostBacked = 1,
    Deep = 2,
}

fn now_ms() -> Result<i64, RuntimeError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .ok_or(RuntimeError::Unsupported)
}

fn uncertain(what: &str, error: HttpError) -> RuntimeError {
    // The HTTP client never quotes credentials; the reason names the step.
    RuntimeError::Uncertain(format!("vLLM {what} outcome unknown: {error}"))
}

/// Shape of one residency step. Any failure here happens before an engine call.
fn admit(command: &RuntimeCommand, now: i64) -> Result<(), RuntimeError> {
    let c = &command.context;
    let ExecutionIdentities::Retained(identities) = &c.identities else {
        return Err(RuntimeError::Unsupported);
    };
    if identities.is_empty()
        || !identities.iter().any(|identity| identity.role == "api")
        || c.issued_at_ms < 0
        || c.issued_at_ms > now
        || c.deadline_ms <= now
        || c.launch_settings.is_some()
        || c.completion_target.is_some()
        || c.token.revision < 1
        || c.token.generation < 1
        || [
            &c.binding_id,
            &c.incarnation,
            &c.token.deployment_id,
            &c.token.operation_id,
            &c.token.step_id,
        ]
        .iter()
        .any(|value| value.is_empty())
    {
        return Err(RuntimeError::Unsupported);
    }
    Ok(())
}

pub(super) async fn execute(
    adapter: &VllmAdapter,
    command: &RuntimeCommand,
) -> Result<EffectObservation, RuntimeError> {
    // SPEC §9.1 / T21: every sleep, wake and collective call needs the host's
    // deep-park policy; an opted-out host never reaches the engine.
    adapter
        .deep_park_enabled()
        .then_some(())
        .ok_or(RuntimeError::Unsupported)?;
    let now = now_ms()?;
    admit(command, now)?;
    adapter.begin_residency_step(&command.context.token.step_id)?;
    let budget = std::time::Duration::from_millis((command.context.deadline_ms - now) as u64);
    let result = match tokio::time::timeout(budget, step(adapter, command)).await {
        Ok(result) => result,
        Err(_) => Err(RuntimeError::Uncertain(
            "vLLM residency step outlived its deadline".into(),
        )),
    };
    adapter.end_residency_step(result.is_ok());
    result
}

async fn step(
    adapter: &VllmAdapter,
    command: &RuntimeCommand,
) -> Result<EffectObservation, RuntimeError> {
    let c = &command.context;
    let http = adapter.http();
    let member = MemberRef {
        deployment_id: c.token.deployment_id.clone(),
        member_id: c.binding_id.clone(),
    };
    let fact = match command.action {
        RuntimeAction::Park => {
            http.sleep(adapter.park_level() as u8)
                .await
                .map_err(|e| uncertain("sleep", e))?;
            // The acknowledgement alone is not the post-condition.
            if !http
                .is_sleeping()
                .await
                .map_err(|e| uncertain("sleep state", e))?
            {
                return Err(RuntimeError::Uncertain(
                    "vLLM acknowledged sleep but does not report sleeping".into(),
                ));
            }
            adapter.mark_parked(&member, true);
            Milestone::MemoryReleased
        }
        RuntimeAction::Restore => {
            http.wake_tag(WakeTag::Weights)
                .await
                .map_err(|e| uncertain("weight wake", e))?;
            Milestone::AllocationsRestored
        }
        RuntimeAction::ReloadWeights => {
            if adapter.park_level() == ParkLevel::HostBacked {
                // Level 1 kept the weights in pinned host RAM and the weights
                // wake copied them back: there is nothing to reload (vLLM
                // sleep mode docs; discrete GPU design §5). The fresh probe
                // after the last step still proves the model usable.
                Milestone::WeightsUsable
            } else {
                // Collective control, invoked exactly once through the lead
                // (SPEC §11).
                http.collective_rpc()
                    .await
                    .map_err(|e| uncertain("reload_weights", e))?;
                Milestone::WeightsUsable
            }
        }
        RuntimeAction::InvalidateCache => {
            http.wake_tag(WakeTag::KvCache)
                .await
                .map_err(|e| uncertain("KV wake", e))?;
            // Discarded KV must never be reused through stale prefix metadata.
            http.reset_prefix_cache()
                .await
                .map_err(|e| uncertain("prefix cache reset", e))?;
            if http
                .is_sleeping()
                .await
                .map_err(|e| uncertain("sleep state", e))?
            {
                return Err(RuntimeError::Uncertain(
                    "vLLM still reports sleeping after both wakes".into(),
                ));
            }
            adapter.mark_parked(&member, false);
            Milestone::CacheValid
        }
        RuntimeAction::Probe => {
            // SPEC §6.1: liveness of an HTTP server is not model readiness. A
            // woken engine counts as usable only once it answers a completion
            // through the same authenticated path inference uses.
            if http
                .is_sleeping()
                .await
                .map_err(|e| uncertain("sleep state", e))?
            {
                return Err(RuntimeError::Uncertain(
                    "vLLM still reports sleeping at the readiness probe".into(),
                ));
            }
            let body = serde_json::json!({
                "model": adapter.served_model(),
                "messages": [{"role": "user", "content": "Say ready."}],
                "max_tokens": 8,
                "temperature": 0,
            });
            let answer = crate::traits::ChatForward::forward_chat(adapter, &body)
                .await
                .map_err(|e| {
                    RuntimeError::Uncertain(crate::vllm::args::redact_text(&format!(
                        "vLLM did not answer the readiness probe after waking: {e:?}"
                    )))
                })?;
            if answer["choices"][0]["message"]["content"]
                .as_str()
                .is_none_or(str::is_empty)
            {
                return Err(RuntimeError::Uncertain(
                    "vLLM answered the readiness probe with empty content".into(),
                ));
            }
            Milestone::ModelUsable
        }
        _ => return Err(RuntimeError::Unsupported),
    };
    let ExecutionIdentities::Retained(identities) = &c.identities else {
        return Err(RuntimeError::Unsupported);
    };
    Ok(EffectObservation {
        token: c.token.clone(),
        binding_id: c.binding_id.clone(),
        incarnation: c.incarnation.clone(),
        // The adapter cannot observe processes. These are the identities the
        // caller retained; the caller re-proves them unchanged (SPEC §13.2).
        identities: identities.clone(),
        observed_at_ms: now_ms().map_err(|_| {
            RuntimeError::Uncertain("clock unavailable after a residency step".into())
        })?,
        receipt: format!("vllm-residency-v1:{:?}:{}", command.action, c.token.step_id),
        facts: vec![fact],
    })
}

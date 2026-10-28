//! ADR 0038 §3, §5, §9: the TensorRT-LLM Initialize step: render, create the
//! launch directory, spawn through the director's tool, then readiness in
//! order, each step bounded by the Initialize window (ADR 0014 A1 and A7):
//! the port accepts a connection; `/health` answers 200; `/version` reports
//! the profile's version, else `installation_drift`; `/v1/models` lists
//! exactly the served name, else `effective_args_mismatch`; the bounded chat
//! probe answers; one `/metrics` read holds an iteration entry whose built
//! KV pool holds one window (else `kv_cache_short`) and does not exceed
//! `max_tokens` by more than one block (else `effective_args_mismatch`);
//! then no descendant outside the process group (`engine_process_escaped`)
//! and no listener off loopback (`engine_listener_exposed`).
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use capyctl_domain::completion::{
    EffectObservation, ExecutionIdentities, Milestone, Presence, ProcessIdentity,
};

use super::{
    adapter::TensorrtLlmAdapter,
    args::{engine_environment, prepare_launch_dir, render_command, PlanInputTensorrtLlm},
    http::Read,
    ownership::{check_ownership, OwnershipFailure},
};
use crate::traits::{OwnedProcessLaunch, RuntimeCommand, RuntimeError};
use crate::vllm::args::redact_text;

const READINESS_POLL: Duration = Duration::from_millis(500);
const BUILDER_MARGIN_MS: i64 = 2_000;
/// How long a failed launch's processes get to leave on SIGTERM before
/// SIGKILL (ADR 0038 §4, §9: the launch is stopped, with gone evidence).
const STOP_GRACE: Duration = Duration::from_secs(5);
/// One connection attempt on the leased port.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// ADR 0038 §5, SPEC §8.2: the prefix of the failure a launch whose engine
/// reports other values than were rendered ends with.
pub const EFFECTIVE_ARGS_MISMATCH: &str = "effective_args_mismatch";
/// ADR 0038 §5, ADR 0008: the prefix of the failure a launch whose engine
/// reports another version than the profile registered ends with.
pub const INSTALLATION_DRIFT: &str = "installation_drift";
/// ADR 0038 §4, §16: the closed failure of a launch whose built KV pool
/// holds less than one request's window.
pub const KV_CACHE_SHORT: &str = "kv_cache_short";

fn now_ms() -> Result<i64, RuntimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RuntimeError::Uncertain("system clock is before the epoch".into()))
        .map(|d| d.as_millis() as i64)
}

fn remaining(stop_at: i64) -> Result<Duration, RuntimeError> {
    Ok(Duration::from_millis(
        u64::try_from(stop_at - now_ms()?).unwrap_or(0),
    ))
}

fn unread(e: crate::traits::AdapterError) -> RuntimeError {
    RuntimeError::Uncertain(redact_text(&format!("{e}")))
}

fn past_deadline() -> RuntimeError {
    RuntimeError::Uncertain("readiness deadline reached with the engine alive".into())
}

/// Stop the launch's `identities` with a verified exit and fail it `reason`.
/// ADR 0031 §7: where these tools cannot stop it (a host agent's launch
/// tools confer no cleanup authority), the caller stops the recorded
/// processes before it reports the failure.
async fn stop_and_fail(
    tools: &std::sync::Arc<dyn OwnedProcessLaunch>,
    identities: Vec<ProcessIdentity>,
    reason: String,
) -> RuntimeError {
    let tools = tools.clone();
    let stopped =
        tokio::task::spawn_blocking(move || tools.terminate_owned(&identities, STOP_GRACE)).await;
    RuntimeError::LaunchFailed(format!(
        "{reason}{}",
        if matches!(stopped, Ok(Ok(()))) {
            "; the engine was stopped"
        } else {
            ""
        }
    ))
}

pub(super) async fn initialize(
    adapter: &TensorrtLlmAdapter,
    command: &RuntimeCommand,
) -> Result<EffectObservation, RuntimeError> {
    let context = &command.context;
    let (plan, tools) = adapter.launch_parts()?;
    if !matches!(
        context.launch_settings,
        Some(capyctl_domain::launch::LaunchSettings::TensorrtLlm(_))
    ) || !matches!(context.identities, ExecutionIdentities::OwnedLaunch)
    {
        return Err(RuntimeError::Unsupported);
    }
    let mut cmd =
        render_command(&plan).map_err(|e| RuntimeError::Uncertain(format!("render: {e}")))?;
    cmd.env = engine_environment(
        &cmd.env,
        &plan,
        &|name| std::env::var(name).ok(),
        crate::engine_telemetry::host_setting(),
    );
    adapter.claim_incarnation(&context.binding_id, &context.incarnation)?;
    // ADR 0038 §3: the directory, the served-name link and the file the
    // command names exist before the engine starts.
    prepare_launch_dir(&plan).map_err(|e| RuntimeError::Uncertain(format!("{e}")))?;
    let incarnation = context.incarnation.clone();
    let spawn_tools = tools.clone();
    let api = tokio::task::spawn_blocking(move || spawn_tools.spawn_durable(&incarnation, &cmd))
        .await
        .map_err(|_| RuntimeError::Uncertain("spawn task failed".into()))??;
    let stop_at = context.deadline_ms.saturating_sub(BUILDER_MARGIN_MS);
    let present = |identity: ProcessIdentity| {
        let tools = tools.clone();
        async move {
            tokio::task::spawn_blocking(move || tools.present(&identity))
                .await
                .map_err(|_| RuntimeError::Uncertain("presence task failed".into()))
        }
    };
    let exited = || {
        let tail = crate::launch_failure::log_tail(plan.engine_log.as_deref());
        RuntimeError::LaunchFailed(format!(
            "{}; log tail:\n{tail}",
            crate::launch_failure::summary(&tail, None)
        ))
    };
    // ADR 0038 §5: the port accepts a connection, then `/health` answers 200,
    // while the process is watched.
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], plan.port));
    let mut last_error: Option<String> = None;
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
        let accepts = matches!(
            tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(address)).await,
            Ok(Ok(_))
        );
        if accepts {
            match adapter.http().health().await {
                Ok(Read::Answer(())) => break,
                Ok(Read::NotYet) => {}
                Err(e) => last_error = Some(e.to_string()),
            }
        }
        if now_ms()? >= stop_at {
            return Err(RuntimeError::Uncertain(redact_text(&match last_error {
                Some(reason) => {
                    format!("readiness deadline reached with the engine alive; /health: {reason}")
                }
                None => "readiness deadline reached with the engine alive".into(),
            })));
        }
        tokio::time::sleep(READINESS_POLL).await;
    }
    verify_served(adapter, &plan, stop_at).await?;
    // ADR 0038 §5: the existing bounded chat probe; non-empty `content` or
    // `reasoning_content` answers.
    let body = json!({
        "model": plan.served_model_name,
        "messages": [{"role": "user", "content": "Say ready."}],
        "max_tokens": 8,
        "temperature": 0,
    });
    let answer = crate::forward::startup_probe(adapter, &body, remaining(stop_at)?)
        .await
        .ok_or_else(past_deadline)?
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
    // ADR 0038 §9: one `trtllm-serve` process serves (helpers only while
    // kernels compile).
    if identities.first().map(|i| i.role.as_str()) != Some("api") {
        return Err(RuntimeError::Uncertain(
            "engine group has no API process".into(),
        ));
    }
    let pool = verify_pool(adapter, &plan, stop_at, &tools, &identities).await?;
    // ADR 0038 §9: after the probe and the pool, before admission opens.
    let root = adapter.proc_root().to_path_buf();
    let checked = identities.clone();
    let check_tools = tools.clone();
    let check = tokio::task::spawn_blocking(move || {
        check_ownership(&root, &checked, &|identity| check_tools.present(identity))
    })
    .await
    .map_err(|_| RuntimeError::Uncertain("ownership task failed".into()))?;
    match check {
        Ok(()) => {}
        Err(failure @ OwnershipFailure::Escaped(_)) => {
            // ADR 0038 §9: the escaped process is outside the group a stop
            // signals, so the launch fails only once it, the group and every
            // recorded process are proven gone; otherwise it stays uncertain
            // and keeps its accounting.
            let set = failure.stop_set(&identities);
            let stop_tools = tools.clone();
            let stopped =
                tokio::task::spawn_blocking(move || stop_tools.terminate_owned(&set, STOP_GRACE))
                    .await
                    .map_err(|_| {
                        RuntimeError::Uncertain(format!("{failure}; the stop task failed"))
                    })?;
            return Err(match stopped {
                Ok(()) => RuntimeError::LaunchFailed(format!(
                    "{failure}; the engine and the escaped process were stopped"
                )),
                Err(e) => RuntimeError::Uncertain(format!(
                    "{failure}; the escaped process is not proven gone: {e}"
                )),
            });
        }
        Err(failure @ OwnershipFailure::Exposed(_)) => {
            return Err(stop_and_fail(&tools, identities, failure.to_string()).await);
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
            "tensorrt-llm {} ready on {}; version and served name verified; KV pool of {pool} \
             tokens (max_tokens {}); probe answered; no process outside the group; listeners \
             on loopback",
            adapter.fingerprint(),
            adapter.endpoint(),
            plan.max_tokens,
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

/// ADR 0038 §5: `/version` reports the profile's version, else
/// `installation_drift`; `/v1/models` lists exactly the served name, else
/// `effective_args_mismatch`. The coordinator then stops the launch on its
/// recorded processes.
async fn verify_served(
    adapter: &TensorrtLlmAdapter,
    plan: &PlanInputTensorrtLlm,
    stop_at: i64,
) -> Result<(), RuntimeError> {
    let version = tokio::time::timeout(remaining(stop_at)?, adapter.http().version())
        .await
        .map_err(|_| past_deadline())?
        .map_err(unread)?;
    let reported = match version {
        Read::Answer(Some(reported)) => reported,
        Read::Answer(None) => "no version".into(),
        Read::NotYet => "nothing (not answering)".into(),
    };
    if reported != plan.version {
        return Err(RuntimeError::Uncertain(redact_text(&format!(
            "{INSTALLATION_DRIFT}: TensorRT-LLM reports version {reported:?}; the profile \
             registered {:?}",
            plan.version
        ))));
    }
    let models = tokio::time::timeout(remaining(stop_at)?, adapter.http().models())
        .await
        .map_err(|_| past_deadline())?
        .map_err(unread)?;
    let listed = match models {
        Read::Answer(ids) => ids,
        Read::NotYet => Vec::new(),
    };
    if listed != [plan.served_model_name.clone()] {
        return Err(RuntimeError::Uncertain(redact_text(&format!(
            "{EFFECTIVE_ARGS_MISMATCH}: /v1/models lists {listed:?}; capyctl serves exactly \
             {:?}",
            plan.served_model_name
        ))));
    }
    Ok(())
}

/// ADR 0038 §4, §5: one `/metrics` read after the probe must hold an
/// iteration entry (proving iteration statistics are on), and the pool its
/// newest entry reports, `maxNumBlocks × tokensPerBlock`, must hold one
/// window and exceed `max_tokens` by at most one block. A short pool stops
/// the launch and fails it `kv_cache_short`. The built pool, in tokens.
async fn verify_pool(
    adapter: &TensorrtLlmAdapter,
    plan: &PlanInputTensorrtLlm,
    stop_at: i64,
    tools: &std::sync::Arc<dyn OwnedProcessLaunch>,
    identities: &[ProcessIdentity],
) -> Result<u64, RuntimeError> {
    let mismatch = |what: String| {
        RuntimeError::Uncertain(format!("{EFFECTIVE_ARGS_MISMATCH}: TensorRT-LLM {what}"))
    };
    let read = tokio::time::timeout(remaining(stop_at)?, adapter.http().metrics())
        .await
        .map_err(|_| past_deadline())?
        .map_err(unread)?;
    let entries = match read {
        Read::Answer(entries) if !entries.is_empty() => entries,
        _ => {
            return Err(mismatch(
                "/metrics returned no iteration entry after the probe; capyctl rendered \
                 enable_iter_perf_stats: true"
                    .into(),
            ))
        }
    };
    let Some(pool) = entries
        .iter()
        .rev()
        .find_map(|entry| entry.kv_cache_stats?.pool_tokens())
    else {
        return Err(mismatch(
            "/metrics reports no kvCacheStats with maxNumBlocks and tokensPerBlock".into(),
        ));
    };
    if pool < u64::from(plan.max_seq_len) {
        return Err(stop_and_fail(
            tools,
            identities.to_vec(),
            format!(
                "{KV_CACHE_SHORT}: TensorRT-LLM built a KV pool of {pool} tokens, less than one \
                 request's window of {} tokens (max_seq_len); capyctl rendered max_tokens {}",
                plan.max_seq_len, plan.max_tokens
            ),
        )
        .await);
    }
    let ceiling = plan
        .max_tokens
        .saturating_add(u64::from(plan.tokens_per_block));
    if pool > ceiling {
        return Err(mismatch(format!(
            "built a KV pool of {pool} tokens, more than max_tokens {} and one block of {}",
            plan.max_tokens, plan.tokens_per_block
        )));
    }
    Ok(pool)
}

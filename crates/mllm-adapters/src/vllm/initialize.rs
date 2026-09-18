//! The vLLM Initialize step (Spec §4). The builder does the whole step: render,
//! spawn through the director's tool, wait for readiness while watching the
//! process, probe once, enumerate the group, and report identities and facts.
//!
//! The builder never learns where identities are recorded: the tool the director
//! hands it makes the spawn durable, so a step that dies mid-launch still leaves
//! an owner behind an identity somebody else wrote down.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use mllm_domain::completion::{
    EffectObservation, ExecutionIdentities, Milestone, Presence, ProcessIdentity,
};

use crate::traits::{
    ChatForward, EngineAdapter, MemberRef, Readiness, RuntimeCommand, RuntimeError,
};
use crate::vllm::adapter::VllmAdapter;
use crate::vllm::args::{redact_text, render_command};

/// How often the builder asks the engine whether it is serving the model. vLLM's
/// startup is minutes of weight staging on this hardware, so a tighter poll only
/// buys load on the engine's own event loop.
const READINESS_POLL: Duration = Duration::from_millis(500);

/// How far ahead of the coordinator's bound the builder gives up. The builder's
/// reason — deadline, dead process, unanswered probe — is worth more than a bare
/// timeout, so it must be the one that arrives first (Spec §4).
const BUILDER_MARGIN_MS: i64 = 2_000;

/// Lines of engine log quoted when a launch fails.
const LOG_TAIL_LINES: usize = 20;

/// The most log bytes read for a tail. An engine that logged a gigabyte before
/// dying must not be read into memory to explain itself.
const LOG_TAIL_BYTES: u64 = 64 * 1024;

pub(super) async fn initialize(
    adapter: &VllmAdapter,
    command: &RuntimeCommand,
) -> Result<EffectObservation, RuntimeError> {
    let context = &command.context;
    // Unsupported when any part is missing: a builder without its plan, its tools
    // or its key cannot launch anything, and must not half-run the step.
    let (mut plan, tools, key) = adapter.launch_parts()?;
    if !matches!(
        context.launch_settings,
        Some(mllm_domain::launch::ProfileLaunchSettings::Vllm(_))
    ) || !matches!(context.identities, ExecutionIdentities::OwnedLaunch)
    {
        return Err(RuntimeError::Unsupported);
    }
    // One launch per incarnation: a repeat would start a second engine holding the
    // same device memory while the first is still recorded as owned.
    adapter.claim_incarnation(&context.binding_id, &context.incarnation)?;

    // Spec §3: the key rides the environment, never argv.
    plan.api_key = None;
    let mut cmd =
        render_command(&plan).map_err(|e| RuntimeError::Uncertain(format!("render: {e}")))?;
    cmd.env.insert("VLLM_API_KEY".into(), key.clone());
    if let Some(extra) = &plan.engine_path_extra {
        // The engine's runtime PATH carries its own venv bin: the JIT compile step
        // needs the venv's tools.
        let system = std::env::var("PATH").unwrap_or_default();
        cmd.env.insert("PATH".into(), format!("{extra}:{system}"));
    }
    if let Some(log) = &plan.engine_log {
        cmd.env.insert("MLLM_ENGINE_LOG".into(), log.clone());
    }

    // The tool is synchronous on purpose (mllm-launchers has no runtime), so every
    // call into it leaves the async threads free.
    let incarnation = context.incarnation.clone();
    let spawn_tools = tools.clone();
    let api = tokio::task::spawn_blocking(move || spawn_tools.spawn_durable(&incarnation, &cmd))
        .await
        .map_err(|_| RuntimeError::Uncertain("spawn task failed".into()))??;

    // Spec §4: the builder ends before the coordinator's bound so its own error wins.
    let stop_at = context.deadline_ms.saturating_sub(BUILDER_MARGIN_MS);
    let member = MemberRef {
        deployment_id: context.token.deployment_id.clone(),
        member_id: context.binding_id.clone(),
    };
    loop {
        match adapter.check_readiness(&member).await {
            Ok(Readiness::Ready) => break,
            Ok(Readiness::Initializing) => {}
            // Spec §3: both exits from this step are journaled, so both pass
            // redaction. Display, not Debug, so the reason carries one prefix
            // rather than nesting this one inside the error's own.
            Err(e) => {
                return Err(RuntimeError::Uncertain(redact_text(&format!(
                    "readiness: {e}"
                ))))
            }
        }
        // Spec §4 step 4: a process that left is the answer, and waiting out the
        // deadline would only delay it.
        let presence_tools = tools.clone();
        let watched = api.clone();
        match tokio::task::spawn_blocking(move || presence_tools.present(&watched))
            .await
            .map_err(|_| RuntimeError::Uncertain("presence task failed".into()))?
        {
            Presence::Alive => {}
            Presence::Gone => {
                return Err(RuntimeError::Uncertain(format!(
                    "engine exited before readiness; log tail:\n{}",
                    log_tail(plan.engine_log.as_deref(), LOG_TAIL_LINES)
                )))
            }
            // Unknown is retained, never absent: the step fails but says why.
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

    // SPEC §6.1: the served model in the list is not a model that answers. One
    // probe, through the same authenticated path inference will use.
    let body = json!({
        "model": plan.served_model_name,
        "messages": [{"role": "user", "content": "Say ready."}],
        "max_tokens": 8,
        "temperature": 0,
    });
    // Spec §4: every wait is bounded by the context deadline, and the builder's own
    // waits end first so its reason, not a bare coordinator timeout, is what gets
    // recorded. The chat client carries its own much longer bounds, so a model that
    // lists itself and then stalls on its first completion would otherwise hand the
    // outcome to `drive`.
    let probe_budget = Duration::from_millis(u64::try_from(stop_at - now_ms()?).unwrap_or(0));
    let answer = tokio::time::timeout(probe_budget, adapter.forward_chat(&body))
        .await
        .map_err(|_| {
            RuntimeError::Uncertain("probe deadline reached with the engine alive".into())
        })?
        .map_err(|e| {
            RuntimeError::Uncertain(redact_text(&format!(
                "engine listed the model but did not answer: {e:?}"
            )))
        })?;
    if answer["choices"][0]["message"]["content"]
        .as_str()
        .is_none_or(str::is_empty)
    {
        return Err(RuntimeError::Uncertain(
            "engine answered with empty content".into(),
        ));
    }

    let group_tools = tools.clone();
    let led_by = api.clone();
    let identities = tokio::task::spawn_blocking(move || group_tools.observe_group(&led_by))
        .await
        .map_err(|_| RuntimeError::Uncertain("group task failed".into()))??;
    // A group without its API process, or without a single worker, is not the
    // deployment this step was asked to build; reporting it would record an
    // ownership set that does not cover the processes holding the device.
    if identities.first().map(|i| i.role.as_str()) != Some("api") || identities.len() < 2 {
        return Err(RuntimeError::Uncertain(format!(
            "engine group incomplete: {}",
            roles(&identities)
        )));
    }

    Ok(EffectObservation {
        token: context.token.clone(),
        binding_id: context.binding_id.clone(),
        incarnation: context.incarnation.clone(),
        identities,
        observed_at_ms: now_ms()?,
        // The receipt carries provenance, never a credential.
        receipt: format!(
            "vllm {} ready on {}; probe answered",
            adapter.fingerprint(),
            adapter.endpoint()
        ),
        facts: vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ],
    })
}

fn roles(identities: &[ProcessIdentity]) -> String {
    identities
        .iter()
        .map(|i| format!("{}:{}", i.role, i.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The last `lines` lines of the engine's log, bounded and redacted. The tail is
/// the engine's own account of why it left, and it is quoted into an error that
/// reaches a journal, so it is passed through redaction first (Spec §3).
fn log_tail(path: Option<&str>, lines: usize) -> String {
    let Some(path) = path else {
        return "(no engine log was configured for this launch)".into();
    };
    let text = match read_tail_bytes(path) {
        Ok(text) => text,
        Err(e) => return format!("(engine log {path} could not be read: {e})"),
    };
    let tail: Vec<&str> = text
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() {
        return format!("(engine log {path} is empty)");
    }
    redact_text(&tail.join("\n"))
}

fn read_tail_bytes(path: &str) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > LOG_TAIL_BYTES {
        file.seek(SeekFrom::Start(len - LOG_TAIL_BYTES))?;
    }
    let mut buffer = Vec::new();
    file.take(LOG_TAIL_BYTES).read_to_end(&mut buffer)?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// Wall-clock milliseconds. A clock that cannot answer leaves the step uncertain
/// rather than stamping evidence with a time nobody read.
fn now_ms() -> Result<i64, RuntimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RuntimeError::Uncertain("system clock is before the epoch".into()))
        .map(|d| d.as_millis() as i64)
}

//! Trusted local Fake composition. No management evidence ingestion route.
use mllm_adapters::traits::ChatForward;
use mllm_domain::qualification::{
    CandidateRequestObservation, CandidateResponseObservation, CandidateTerminal,
};
use mllm_store::candidate_creation::progression::CandidateProbeDispatch;

/// Called only after a New cleanup arm and after predecessor command tasks exit.
/// InspectOwnedGone never repeats termination after a lost reply or restart.
pub fn collect_cleanup(
    engine: &mllm_adapters::fake::FakeEngine,
    context: &mllm_store::candidate_creation::cleanup::CleanupExecutionContext,
    observed_at_ms: i64,
) -> Result<mllm_domain::completion::CleanupEvidence, mllm_store::lifecycle::LifecycleError> {
    if observed_at_ms < context.issued_at_ms || observed_at_ms > context.deadline_ms {
        return Err(mllm_store::lifecycle::LifecycleError::Invalid);
    }
    engine
        .qualification_cleanup(
            &context.binding_id,
            &context.incarnation,
            &context.identities,
            context.mode == mllm_store::candidate_creation::cleanup::CleanupMode::TerminateOwned,
            observed_at_ms,
        )
        .map_err(|_| mllm_store::lifecycle::LifecycleError::Conflict)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamPayload {
    model: String,
    choices: Vec<StreamChoice>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamChoice {
    index: u32,
    delta: StreamDelta,
    finish_reason: Option<String>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamDelta {
    content: Option<String>,
}

pub async fn collect_security_control(
    engine: &mllm_adapters::fake::FakeEngine,
    dispatch: mllm_store::candidate_creation::progression::CandidateSecurityControlDispatch,
) -> Result<
    mllm_domain::qualification::CandidateSecurityControlObservation,
    mllm_store::lifecycle::LifecycleError,
> {
    engine
        .qualification_security_control(&mllm_adapters::traits::RuntimeCommand {
            action: mllm_adapters::traits::RuntimeAction::Park,
            context: dispatch.context().clone(),
        })
        .map_err(|_| mllm_store::lifecycle::LifecycleError::Conflict)
}

/// Trusted service timestamp is sampled after the terminal unauthorized check.
/// The legacy fixture collector above intentionally retains its synthetic time.
pub async fn collect_security_control_with_clock(
    engine: &mllm_adapters::fake::FakeEngine,
    dispatch: mllm_store::candidate_creation::progression::CandidateSecurityControlDispatch,
    clock: &(dyn Fn() -> Result<i64, mllm_store::lifecycle::LifecycleError> + Send + Sync),
) -> Result<
    mllm_domain::qualification::CandidateSecurityControlObservation,
    mllm_store::lifecycle::LifecycleError,
> {
    let mut observation = collect_security_control(engine, dispatch).await?;
    observation.effect.observed_at_ms = clock()?;
    Ok(observation)
}

/// Reads the retained Fake state locally; never issues an engine command.
pub fn collect_parked_status(
    engine: &mllm_adapters::fake::FakeEngine,
    context: &mllm_domain::completion::StepExecutionContext,
) -> Result<
    mllm_domain::qualification::CandidateParkedStatusObservation,
    mllm_store::lifecycle::LifecycleError,
> {
    engine
        .qualification_parked_status(context)
        .map_err(|_| mllm_store::lifecycle::LifecycleError::Conflict)
}

pub async fn collect_probe(
    engine: &mllm_adapters::fake::FakeEngine,
    dispatch: CandidateProbeDispatch,
) -> Result<CandidateRequestObservation, mllm_store::lifecycle::LifecycleError> {
    let fixture_time = dispatch.context().issued_at_ms;
    collect_probe_with_clock(engine, dispatch, &|| Ok(fixture_time)).await
}

/// Service collection samples the trusted clock only after terminal observation.
/// A failed clock leaves the durable probe lease unsettled; no time is invented.
pub async fn collect_probe_with_clock(
    engine: &mllm_adapters::fake::FakeEngine,
    dispatch: CandidateProbeDispatch,
    clock: &(dyn Fn() -> Result<i64, mllm_store::lifecycle::LifecycleError> + Send + Sync),
) -> Result<CandidateRequestObservation, mllm_store::lifecycle::LifecycleError> {
    use mllm_store::lifecycle::LifecycleError;
    let identities = engine
        .qualification_members(dispatch.context())
        .map_err(|_| LifecycleError::Conflict)?;
    let body: serde_json::Value =
        serde_json::from_str(dispatch.request()).map_err(|_| LifecycleError::Invalid)?;
    let (terminal, response) = if let Some(endpoint) = dispatch.security_endpoint() {
        engine
            .qualification_security_request(dispatch.context(), endpoint, &body)
            .map_err(|_| LifecycleError::Conflict)?
    } else if body["stream"] == true {
        let mut chunks = Vec::new();
        let mut bytes = 0_usize;
        let mut invalid = false;
        let end = engine
            .forward_chat_stream(&body, &mut |text| {
                bytes = bytes.saturating_add(text.len());
                if invalid || bytes > 1048576 || chunks.len() >= 4096 {
                    invalid = true;
                    return;
                }
                let Ok(v) = serde_json::from_str::<StreamPayload>(&text) else {
                    invalid = true;
                    return;
                };
                if v.choices.len() != 1 || v.choices[0].index != 0 {
                    invalid = true;
                    return;
                }
                let c = v.choices.into_iter().next().unwrap();
                chunks.push(mllm_domain::qualification::CandidateStreamChunk {
                    index: chunks.len() as u32,
                    model: v.model,
                    content: c.delta.content.unwrap_or_default(),
                    finish_reason: c.finish_reason,
                });
            })
            .await;
        if invalid {
            (
                CandidateTerminal::Uncertain,
                CandidateResponseObservation::NoResponse,
            )
        } else {
            (
                if matches!(end, Ok(mllm_adapters::traits::StreamEnded::Completed)) {
                    CandidateTerminal::Completed
                } else {
                    CandidateTerminal::Uncertain
                },
                CandidateResponseObservation::Streaming {
                    chunks,
                    completed: matches!(end, Ok(mllm_adapters::traits::StreamEnded::Completed)),
                },
            )
        }
    } else {
        match engine.forward_chat(&body).await {
            Ok(value) if value.to_string().len() <= 1048576 => {
                let read = |v: &serde_json::Value| v.as_str().unwrap_or("").to_owned();
                (
                    CandidateTerminal::Completed,
                    CandidateResponseObservation::Nonstreaming {
                        model: read(&value["model"]),
                        content: read(&value["choices"][0]["message"]["content"]),
                        finish_reason: read(&value["choices"][0]["finish_reason"]),
                    },
                )
            }
            Err(mllm_adapters::traits::AdapterError::PolicyDenied) => (
                CandidateTerminal::FailedTerminal,
                CandidateResponseObservation::NoResponse,
            ),
            _ => (
                CandidateTerminal::Uncertain,
                CandidateResponseObservation::NoResponse,
            ),
        }
    };
    let c = dispatch.context();
    Ok(CandidateRequestObservation {
        request_operation_id: dispatch.request_operation_id().into(),
        lease_id: dispatch.ticket().id().into(),
        token: c.token.clone(),
        binding_id: c.binding_id.clone(),
        incarnation: c.incarnation.clone(),
        identities,
        observed_at_ms: clock()?,
        receipt: format!(
            "qualification-fake-v1:request:{}",
            dispatch.request_operation_id()
        ),
        terminal,
        response,
    })
}

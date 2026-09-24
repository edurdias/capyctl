//! Bounded checks for a collected marker response. Transport and provenance
//! validation remain the caller's responsibility; this is not routing evidence.
use crate::f2_correctness::{MarkerCase, MarkerResponse};
use serde::{de::IgnoredAny, Deserialize};

#[derive(Deserialize)]
struct Response {
    model: String,
    object: Option<String>,
    choices: Vec<Choice>,
    error: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct Choice {
    index: u32,
    message: Message,
    finish_reason: String,
}

#[derive(Deserialize)]
struct Message {
    role: String,
    content: String,
    tool_calls: Option<Vec<IgnoredAny>>,
    function_call: Option<IgnoredAny>,
    refusal: Option<String>,
    reasoning_content: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectedError {
    Invalid,
}

/// Validate one complete non-streaming JSON body, bounded before parsing.
/// Unknown metadata is ignored. Known fields reject duplicates. The caller must
/// independently validate HTTP status, content type, complete transport, and
/// model binding provenance. No response content is retained in diagnostics.
pub fn check_collected_marker(
    case: MarkerCase,
    expected_model: &str,
    body: &[u8],
) -> Result<(), CollectedError> {
    if body.len() > 1_048_576
        || expected_model.is_empty()
        || expected_model.len() > 256
        || expected_model.bytes().any(|b| b.is_ascii_control())
    {
        return Err(CollectedError::Invalid);
    }
    let response: Response = serde_json::from_slice(body).map_err(|_| CollectedError::Invalid)?;
    if response.model != expected_model
        || response.error.is_some()
        || response
            .object
            .as_deref()
            .is_some_and(|s| s != "chat.completion")
        || response.choices.len() != 1
    {
        return Err(CollectedError::Invalid);
    }
    let choice = &response.choices[0];
    let message = &choice.message;
    if choice.index != 0
        || message.role != "assistant"
        || message.tool_calls.as_ref().is_some_and(|v| !v.is_empty())
        || message.function_call.is_some()
        || message.refusal.as_ref().is_some_and(|s| !s.is_empty())
        || message
            .reasoning_content
            .as_ref()
            .is_some_and(|s| !s.is_empty())
    {
        return Err(CollectedError::Invalid);
    }
    let mut marker = MarkerResponse::new(case);
    marker
        .push(&message.content)
        .map_err(|_| CollectedError::Invalid)?;
    marker
        .finish_reason(&choice.finish_reason)
        .map_err(|_| CollectedError::Invalid)?;
    marker.terminal().map_err(|_| CollectedError::Invalid)?;
    marker.complete().map_err(|_| CollectedError::Invalid)
}

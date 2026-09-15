//! Marker validation for already-decoded SSE data events, not an SSE parser.
use crate::f2_correctness::{MarkerCase, MarkerResponse};
use serde::{de::IgnoredAny, Deserialize};

#[derive(Deserialize)]
struct Chunk {
    model: String,
    object: String,
    choices: Vec<Choice>,
    error: Option<IgnoredAny>,
}
#[derive(Deserialize)]
struct Choice {
    index: u32,
    delta: Delta,
    finish_reason: Option<String>,
}
#[derive(Deserialize)]
struct Delta {
    role: Option<String>,
    content: Option<String>,
    tool_calls: Option<Vec<IgnoredAny>>,
    function_call: Option<IgnoredAny>,
    refusal: Option<String>,
    reasoning_content: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamedError;

/// Strict single-choice text stream. Usage-only events are not supported.
/// No Debug implementation: generated content must not enter diagnostics.
/// The caller validates SSE framing, UTF-8, status, content type, complete
/// transport and binding provenance independently. No I/O or authority here.
pub struct StreamedMarker {
    model: String,
    marker: MarkerResponse,
    bytes: usize,
    finished: bool,
    terminal: bool,
    failed: bool,
}
impl StreamedMarker {
    pub fn new(case: MarkerCase, model: &str) -> Result<Self, StreamedError> {
        if model.is_empty() || model.len() > 256 || model.bytes().any(|b| b.is_ascii_control()) {
            return Err(StreamedError);
        }
        Ok(Self {
            model: model.into(),
            marker: MarkerResponse::new(case),
            bytes: 0,
            finished: false,
            terminal: false,
            failed: false,
        })
    }
    /// Each call contains exactly one decoded SSE data event, without `data:`.
    /// Enforce 64 KiB per event, 1 MiB total, and the marker's 256-chunk cap.
    /// Every failure is latched; ignoring an error cannot restore success.
    pub fn push_data(&mut self, data: &str) -> Result<(), StreamedError> {
        if self.failed {
            return Err(StreamedError);
        }
        self.failed = true;
        if self.terminal || data.len() > 65_536 || data.len() > 1_048_576 - self.bytes {
            return Err(StreamedError);
        }
        self.bytes += data.len();
        if data == "[DONE]" {
            self.marker.terminal().map_err(|_| StreamedError)?;
            self.terminal = true;
        } else {
            if self.finished {
                return Err(StreamedError);
            }
            let chunk: Chunk = serde_json::from_str(data).map_err(|_| StreamedError)?;
            if chunk.model != self.model
                || chunk.object != "chat.completion.chunk"
                || chunk.error.is_some()
                || chunk.choices.len() != 1
            {
                return Err(StreamedError);
            }
            let choice = &chunk.choices[0];
            let delta = &choice.delta;
            if choice.index != 0
                || delta.role.as_deref().is_some_and(|r| r != "assistant")
                || delta.tool_calls.as_ref().is_some_and(|v| !v.is_empty())
                || delta.function_call.is_some()
                || delta.refusal.as_ref().is_some_and(|s| !s.is_empty())
                || delta
                    .reasoning_content
                    .as_ref()
                    .is_some_and(|s| !s.is_empty())
            {
                return Err(StreamedError);
            }
            self.marker
                .push(delta.content.as_deref().unwrap_or(""))
                .map_err(|_| StreamedError)?;
            if let Some(reason) = &choice.finish_reason {
                self.marker
                    .finish_reason(reason)
                    .map_err(|_| StreamedError)?;
                self.finished = true;
            }
        }
        self.failed = false;
        Ok(())
    }
    pub fn complete(self) -> Result<(), StreamedError> {
        if self.failed {
            return Err(StreamedError);
        }
        self.marker.complete().map_err(|_| StreamedError)
    }
}

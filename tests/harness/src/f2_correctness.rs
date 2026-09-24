//! Bounded public marker corpus and response-content checks for F2C.
//!
//! This module performs no I/O and parses no HTTP/JSON/SSE. The runner must
//! validate wire shapes, transport completion, route/binding/checkpoint/recipe
//! and operation identity independently. Text equality is not routing evidence.
//! Each engine must pass its own baseline before the corpus supports cache or
//! post-wake comparisons. Prompts and generated content must not enter artifacts.

/// Fixed categories only; never retain response text in an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorrectnessError {
    CaseOutOfRange,
    ContentMismatch,
    FinishReason,
    Incomplete,
    ProtocolOrder,
    ContentLimit,
    ChunkLimit,
    /// A reasoning model's trace reached the corpus because the engine ran without a
    /// reasoning parser. Distinguished from `ContentMismatch` because the model is
    /// answering correctly and the recipe, not the model, needs changing.
    ReasoningTrace,
}

/// Terminators emitted by reasoning models when their trace is not separated into a
/// dedicated field. Only their presence is tested; content is never inspected
/// further, logged, or copied into an error.
const REASONING_TERMINATORS: &[&str] = &["</think>", "</reasoning>", "<|end_thinking|>"];

fn carries_reasoning(text: &str) -> bool {
    REASONING_TERMINATORS.iter().any(|t| text.contains(t))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkerCase(u16);

impl MarkerCase {
    /// A run has at most 4096 distinct public cases. Reuse an index only for an
    /// intentional repeated prompt; assign unused indices to fresh-marker cases.
    pub fn new(index: u16) -> Result<Self, CorrectnessError> {
        if index >= 4096 {
            return Err(CorrectnessError::CaseOutOfRange);
        }
        Ok(Self(index))
    }
    pub fn marker(self) -> String {
        format!("F2_MARKER_{:04}", self.0)
    }
    pub fn prompt(self) -> String {
        format!(
            "Reply with exactly this marker, without explanation: {}",
            self.marker()
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Content,
    Finished,
    Terminal,
}

/// Accumulates at most 128 UTF-8 bytes across at most 256 content chunks.
/// No Debug implementation: accidentally received private content must not log.
/// A failed check is latched even if the caller ignores its immediate error.
pub struct MarkerResponse {
    case: MarkerCase,
    content: String,
    chunks: u16,
    stage: Stage,
    failure: Option<CorrectnessError>,
}
impl MarkerResponse {
    pub fn new(case: MarkerCase) -> Self {
        Self {
            case,
            content: String::with_capacity(128),
            chunks: 0,
            stage: Stage::Content,
            failure: None,
        }
    }

    fn fail(&mut self, error: CorrectnessError) -> Result<(), CorrectnessError> {
        let error = *self.failure.get_or_insert(error);
        self.content.clear();
        Err(error)
    }

    fn require(&mut self, stage: Stage) -> Result<(), CorrectnessError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.stage != stage {
            return self.fail(CorrectnessError::ProtocolOrder);
        }
        Ok(())
    }

    pub fn push(&mut self, text: &str) -> Result<(), CorrectnessError> {
        self.require(Stage::Content)?;
        if self.chunks >= 256 {
            return self.fail(CorrectnessError::ChunkLimit);
        }
        // Checked before the size bound: a trace usually exceeds the content cap, so
        // the limit would otherwise mask the real cause with a size complaint.
        if carries_reasoning(text) {
            return self.fail(CorrectnessError::ReasoningTrace);
        }
        if text.len() > 128 - self.content.len() {
            return self.fail(CorrectnessError::ContentLimit);
        }
        self.chunks += 1;
        self.content.push_str(text);
        Ok(())
    }

    /// Only natural stop is valid for the exact-marker corpus. A token-limit or
    /// content-filter finish is a failed case even if the visible marker matches.
    pub fn finish_reason(&mut self, reason: &str) -> Result<(), CorrectnessError> {
        self.require(Stage::Content)?;
        if reason != "stop" {
            return self.fail(CorrectnessError::FinishReason);
        }
        self.stage = Stage::Finished;
        Ok(())
    }

    /// Record the validated protocol terminal, not an HTTP success status.
    pub fn terminal(&mut self) -> Result<(), CorrectnessError> {
        self.require(Stage::Finished)?;
        self.stage = Stage::Terminal;
        Ok(())
    }

    /// Consume only after the transport reader has independently completed.
    /// Normalize outer ASCII space, tab, CR and LF only. Never remove internal
    /// whitespace, case-fold, strip quotes or hide Unicode contamination.
    pub fn complete(self) -> Result<(), CorrectnessError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.stage != Stage::Terminal {
            return Err(CorrectnessError::Incomplete);
        }
        if carries_reasoning(&self.content) {
            return Err(CorrectnessError::ReasoningTrace);
        }
        if self.content.trim_matches([' ', '\t', '\r', '\n']) != self.case.marker() {
            return Err(CorrectnessError::ContentMismatch);
        }
        Ok(())
    }
}

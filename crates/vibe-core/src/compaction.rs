//! Turning a conversation that no longer fits into one that does.
//!
//! The reference splits compaction the way it split checkpoints: a pure half
//! that is total functions over strings and message lists, and an orchestrating
//! half that drives injected callables and never touches disk itself. This port
//! keeps the split, which is what lets the calculation ship and be measured
//! against a committed corpus before any provider work exists.
//!
//! [`tokens`] holds the arithmetic every budget decision rests on and
//! [`context`] the selection, the envelope and its parser.
//!
//! Reference: `vibe/core/compaction/` and `vibe/core/utils/tokens.py` at the
//! pinned commit.

pub mod context;
pub mod tokens;

use serde::{Deserialize, Serialize};

/// Why a summarization produced nothing usable.
///
/// The reference names exactly these two, as the `CompactionFailureReason`
/// literal type, and reports one of them on the failure telemetry record. A
/// model that answered with a tool call instead of a summary is a distinct,
/// reported outcome rather than a generic error.
///
/// Reference: `vibe/core/compaction/manager.py` at the pinned commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionFailureReason {
    /// The model called a tool instead of answering with a summary.
    ToolCall,
    /// The answer carried no summary, or an empty one.
    EmptySummary,
}

impl CompactionFailureReason {
    /// The value the reference publishes for this reason, which is what the
    /// failure telemetry record carries.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ToolCall => "tool_call",
            Self::EmptySummary => "empty_summary",
        }
    }
}

/// A compaction that produced no transcript.
///
/// `reason` is present only when the summarization itself is what failed; a
/// transport error, a refused request or an unavailable compactor carries none,
/// which is the distinction the reference draws by raising
/// `CompactionFailedError` for the first and letting the rest propagate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionFailure {
    pub reason: Option<CompactionFailureReason>,
    pub message: String,
}

impl CompactionFailure {
    /// A failure the summarizer classified.
    #[must_use]
    pub fn classified(reason: CompactionFailureReason, message: impl Into<String>) -> Self {
        Self {
            reason: Some(reason),
            message: message.into(),
        }
    }
}

impl From<String> for CompactionFailure {
    fn from(message: String) -> Self {
        Self {
            reason: None,
            message,
        }
    }
}

impl From<&str> for CompactionFailure {
    fn from(message: &str) -> Self {
        Self::from(message.to_owned())
    }
}

impl std::fmt::Display for CompactionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// How a compaction ended, which is the status the reference's auto-compaction
/// telemetry record carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStatus {
    Success,
    Failure,
    Cancelled,
}

impl CompactionStatus {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Cancelled => "cancelled",
        }
    }
}

#[cfg(test)]
mod compaction_parity_tests;
#[cfg(test)]
mod context_tests;
#[cfg(test)]
mod tokens_tests;

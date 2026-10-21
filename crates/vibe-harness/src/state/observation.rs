//! What a step commits, as the projector reads it.

use serde_json::{Map, Value, json};

use super::{PublicError, TokenUsage};

/// One reasoning part of a committed answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReasoningPart {
    pub text: Vec<String>,
    pub summary: Vec<String>,
    pub redacted: bool,
}

/// What a tool call came back with, in the harness's own result shape.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResult {
    Success {
        content: Vec<Value>,
        structured_content: Option<Value>,
        meta: Option<Map<String, Value>>,
    },
    Failure {
        content: Vec<Value>,
        code: String,
        message: String,
        details: Option<Value>,
        retryable: bool,
        meta: Option<Map<String, Value>>,
    },
}

impl ToolResult {
    /// A failure with no content of its own.
    #[must_use]
    pub fn failed(code: &str, message: impl Into<String>) -> Self {
        Self::Failure {
            content: Vec::new(),
            code: code.to_owned(),
            message: message.into(),
            details: None,
            retryable: false,
            meta: None,
        }
    }

    #[must_use]
    pub fn meta(&self) -> Option<&Map<String, Value>> {
        match self {
            Self::Success { meta, .. } | Self::Failure { meta, .. } => meta.as_ref(),
        }
    }

    pub fn meta_mut(&mut self) -> &mut Option<Map<String, Value>> {
        match self {
            Self::Success { meta, .. } | Self::Failure { meta, .. } => meta,
        }
    }

    #[must_use]
    pub fn content(&self) -> &[Value] {
        match self {
            Self::Success { content, .. } | Self::Failure { content, .. } => content,
        }
    }

    /// The result as `model_dump(exclude_none=True)` writes it.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        match self {
            Self::Success {
                content,
                structured_content,
                meta,
            } => {
                out.insert("type".into(), json!("success"));
                out.insert("content".into(), json!(content));
                if let Some(structured) = structured_content {
                    out.insert("structured_content".into(), structured.clone());
                }
                if let Some(meta) = meta {
                    out.insert("_meta".into(), Value::Object(meta.clone()));
                }
            }
            Self::Failure {
                content,
                code,
                message,
                details,
                retryable,
                meta,
            } => {
                out.insert("type".into(), json!("failure"));
                out.insert("content".into(), json!(content));
                if let Some(meta) = meta {
                    out.insert("_meta".into(), Value::Object(meta.clone()));
                }
                let mut error = Map::new();
                error.insert("code".into(), json!(code));
                error.insert("message".into(), json!(message));
                if let Some(details) = details {
                    error.insert("details".into(), details.clone());
                }
                error.insert("retryable".into(), json!(retryable));
                out.insert("error".into(), Value::Object(error));
            }
        }
        Value::Object(out)
    }
}

/// One thing a step committed.
#[derive(Debug, Clone, PartialEq)]
pub enum Observation {
    TurnStarted {
        turn_id: String,
        /// The content as the user sent it, public content blocks.
        content: Vec<Value>,
        injected: bool,
        queue_item_id: Option<String>,
        client_message_id: Option<String>,
        user_display_content: Option<Value>,
    },
    TurnSteeringReceived {
        turn_id: String,
        content: Vec<Value>,
        client_message_id: Option<String>,
        user_display_content: Option<Value>,
    },
    TurnSteered {
        turn_id: String,
        client_message_id: Option<String>,
    },
    AssistantMessageCommitted {
        turn_id: String,
        action_id: String,
        reasoning: Vec<ReasoningPart>,
        /// Text, image and resource blocks, already public.
        content: Vec<Value>,
        usage: Option<TokenUsage>,
    },
    ToolResultCommitted {
        turn_id: String,
        action_id: String,
        result: ToolResult,
    },
    ToolDiscoveryFinished {
        turn_id: String,
        call_id: String,
        mode: String,
        tool_count: u64,
        connector_notice_count: u64,
    },
    ContextCompacted {
        turn_id: Option<String>,
        compaction_id: String,
        trigger: String,
        attempt: u64,
        summary_len: usize,
        usage: Option<TokenUsage>,
    },
    ContextCompactionFailed {
        turn_id: Option<String>,
        compaction_id: String,
        trigger: String,
        attempt: u64,
        code: String,
        reason: Option<String>,
    },
    TurnCompleted {
        turn_id: String,
    },
    TurnInterrupted {
        turn_id: String,
        reason: Option<String>,
    },
    TurnFailed {
        turn_id: String,
        error: PublicError,
    },
}

/// The turn a transition leaves the session with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionTurn {
    Running(String),
    Failed(String),
    Settled,
}

/// One running tool call, as a start entry names it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolAction {
    pub action_id: String,
    pub turn_id: String,
    /// The harness name, `file_system.bash` or `vibe.todo`.
    pub tool_name: String,
    pub arguments: Value,
}

/// A fragment of a completion that is still streaming.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompletionDelta {
    pub action_id: String,
    pub turn_id: String,
    pub text: String,
    pub reasoning: String,
    pub restart: bool,
}

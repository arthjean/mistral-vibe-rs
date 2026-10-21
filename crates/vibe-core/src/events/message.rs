//! The transcript the model reads, as the session record stores it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::PublicContentBlock;

/// Reference `ManualShellContext`: a command the user ran with `!`, as the
/// injected turn that reports it records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualShellRecord {
    pub operation_id: String,
    pub command: String,
    pub cwd: String,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub output_text: String,
    pub exit_code: i64,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub interrupted: bool,
    /// Milliseconds, fractional as the reference measures them.
    #[serde(default = "zero_duration")]
    pub duration_ms: serde_json::Number,
    pub created_at: u64,
}

fn zero_duration() -> serde_json::Number {
    serde_json::Number::from(0)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ModelMessage {
    System {
        content: String,
    },
    User {
        content: String,
        /// Whether the harness wrote this turn rather than the operator.
        ///
        /// The reference carries the same flag on every message and compaction
        /// reads it twice: an injected turn is not preserved through a
        /// compaction, and the envelope compaction writes is itself injected,
        /// which is what lets a second compaction tell its own envelope from a
        /// real turn. It defaults on read, so a transcript persisted before the
        /// flag existed loads as operator-authored, which is what it was.
        #[serde(default)]
        injected: bool,
        /// The identity the message's public entry carries, kept so a reload
        /// publishes the entry under the identifier it had live. Reference
        /// `LLMMessage.message_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        /// The images and resources the operator attached, kept so a reload
        /// shows them. Reference `LLMMessage.images` and `resources`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<PublicContentBlock>,
        /// What the client asked the message to be shown as. Reference
        /// `LLMMessage.user_display_content`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_display_content: Option<Value>,
        /// The words the operator typed, when attached resources were folded
        /// into `content`; a reload shows these instead. Reference
        /// `LLMMessage.input_text`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_text: Option<String>,
        /// The command an injected turn reports, when the user ran one with
        /// `!`: what a reload rebuilds its shell effect from. Reference
        /// `LLMMessage.manual_shell`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        manual_shell: Option<Box<ManualShellRecord>>,
        /// Whether this is the envelope a compaction appended. Reference
        /// `LLMMessage.context_boundary == "compaction"`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        compaction_boundary: bool,
    },
    Assistant {
        /// Reference `LLMMessage.message_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        /// Reference `LLMMessage.reasoning_message_id`: the identity of the
        /// reasoning entry this message's reasoning published.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_message_id: Option<String>,
        content: String,
        #[serde(default)]
        reasoning: Option<String>,
        /// Provider-native reasoning items, replayed verbatim to the dialect
        /// that produced them: an Anthropic thinking block with its signature,
        /// an OpenAI Responses reasoning item with its encrypted content.
        /// Reference `LLMMessage.reasoning_payloads`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reasoning_payloads: Vec<serde_json::Map<String, Value>>,
        #[serde(default)]
        tool_calls: Vec<ModelToolCall>,
        /// Whether an empty `content` is still written, as the reference
        /// writes it for the call pairs it injects itself (a skill load, a
        /// mentioned file) while a model reply without text writes none.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        keeps_empty_content: bool,
    },
    Tool {
        call_id: String,
        content: String,
        is_error: bool,
        /// The tool the call named. Reference `LLMMessage.name`.
        #[serde(default)]
        name: String,
        /// What a call that ran to an answer returned. Reference
        /// `LLMMessage.tool_result`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Box<PersistedToolResult>>,
    },
}

/// Reference `PersistedToolResult`: what a call that ran answered, kept so a
/// reload settles its effect as it settled live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedToolResult {
    /// The tool's typed result.
    pub output: Value,
    /// Seconds the call ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<serde_json::Number>,
    #[serde(default)]
    pub cancelled: bool,
    /// Reference `ToolResultPresentation`, camelCase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<Value>,
}

impl ModelMessage {
    /// A user turn the operator wrote.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
            injected: false,
            message_id: None,
            attachments: Vec::new(),
            manual_shell: None,
            compaction_boundary: false,
            input_text: None,
            user_display_content: None,
        }
    }

    /// A user turn the harness wrote: a policy injection, or the envelope a
    /// compaction leaves behind.
    #[must_use]
    pub fn injected_user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
            injected: true,
            message_id: None,
            attachments: Vec::new(),
            manual_shell: None,
            compaction_boundary: false,
            input_text: None,
            user_display_content: None,
        }
    }

    /// The envelope a compaction appends. Reference `CompactionManager.compact`.
    #[must_use]
    pub fn compaction_envelope(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
            injected: true,
            message_id: None,
            attachments: Vec::new(),
            manual_shell: None,
            compaction_boundary: true,
            input_text: None,
            user_display_content: None,
        }
    }

    /// A call's answer. Reference `create_tool_response_message`.
    #[must_use]
    pub fn tool(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::Tool {
            call_id: call_id.into(),
            content: content.into(),
            is_error,
            name: name.into(),
            result: None,
        }
    }

    /// Gives every user and assistant message an identity, as reference
    /// `LLMMessage` mints one for each message that is not a tool answer.
    pub fn ensure_identity(&mut self) {
        match self {
            Self::User { message_id, .. } | Self::Assistant { message_id, .. }
                if message_id.is_none() =>
            {
                *message_id = Some(crate::session_id::uuid_v4());
            }
            _ => {}
        }
    }

    /// The text this message carries, which every variant has.
    #[must_use]
    pub fn content(&self) -> &str {
        match self {
            Self::System { content }
            | Self::User { content, .. }
            | Self::Assistant { content, .. }
            | Self::Tool { content, .. } => content,
        }
    }

    /// Whether this is a user turn, which is where a round begins.
    #[must_use]
    pub fn is_user(&self) -> bool {
        matches!(self, Self::User { .. })
    }

    /// Whether the harness wrote this message. Only a user turn can be
    /// injected; every other role is authored by the model or the tool that
    /// answered.
    #[must_use]
    pub fn is_injected(&self) -> bool {
        matches!(self, Self::User { injected: true, .. })
    }
}

/// What a call left without an answer reads once the next turn fills it in.
/// Reference `get_user_cancellation_message(TOOL_NO_RESPONSE)`: the
/// cancellation tag is reproduced, the sentence inside it is this port's own.
pub const NO_RESPONSE_TOOL_RESULT: &str =
    "<user_cancellation>The tool call was interrupted before it answered</user_cancellation>";

/// Reference `AgentLoop._fill_missing_tool_responses`: every call an assistant
/// message asked for and no tool message answered gets an answer saying so,
/// after the answers it has, so the next request is well formed.
pub fn fill_missing_tool_responses(messages: &mut Vec<ModelMessage>) {
    if messages.len() < 2 {
        return;
    }
    let mut index = 0;
    while index < messages.len() {
        let calls = match &messages[index] {
            ModelMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                tool_calls.clone()
            }
            _ => {
                index += 1;
                continue;
            }
        };
        let mut answered = std::collections::BTreeSet::new();
        let mut next = index + 1;
        while let Some(ModelMessage::Tool { call_id, .. }) = messages.get(next) {
            answered.insert(call_id.clone());
            next += 1;
        }
        if answered.len() < calls.len() {
            let missing = calls.iter().filter(|call| !answered.contains(&call.id));
            for (insertion, call) in (next..).zip(missing) {
                messages.insert(
                    insertion,
                    ModelMessage::tool(&call.id, &call.name, NO_RESPONSE_TOOL_RESULT, true),
                );
            }
        }
        index += 1 + calls.len();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// How the call was presented when it ran. Reference
    /// `ToolCall.presentation`, a `ToolCallPresentation` in camelCase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<Value>,
}

impl ModelToolCall {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
            presentation: None,
        }
    }
}

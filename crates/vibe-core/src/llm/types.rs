//! The provider-neutral shapes a model call is made of.
//!
//! Reference `LLMMessage`, `LLMChunk`, `LLMUsage`, `StopInfo`, `ToolCall` and
//! `AvailableTool` (`vibe/core/types.py`). A message is what a backend sends
//! and what it reads back; a chunk is one piece of an answer, and chunks fold
//! into the answer with [`Chunk::merge`], which is the reference's `__add__`:
//! text and reasoning concatenate, reasoning payloads append, tool calls merge
//! by their index, usage sums, and the last stop and correlation identifier win.

use serde_json::{Map, Value, json};

/// Who a message speaks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }

    /// The role a wire string names.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "system" => Some(Self::System),
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

/// One call the model asked for, or one piece of it while it streams.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCall {
    pub id: Option<String>,
    /// The position that merges the pieces of one call; a piece without one
    /// cannot be merged.
    pub index: Option<u64>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// An image a user message carries, already read into base64.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub mime_type: String,
    pub data: String,
}

impl Image {
    /// The image as a `data:` URI, the form the chat dialects embed.
    #[must_use]
    pub fn data_uri(&self) -> String {
        format!("data:{};base64,{}", self.mime_type, self.data)
    }
}

/// One message of a conversation as a backend sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Option<String>,
    pub images: Vec<Image>,
    /// A message the harness added rather than the operator typed.
    pub injected: bool,
    pub reasoning_content: Option<String>,
    /// Provider-native reasoning items, replayed verbatim to the dialect that
    /// produced them.
    pub reasoning_payloads: Option<Vec<Map<String, Value>>>,
    pub tool_calls: Option<Vec<ToolCall>>,
    /// The tool a tool message answers for.
    pub name: Option<String>,
    pub tool_call_id: Option<String>,
}

impl Message {
    /// An empty message in `role`.
    #[must_use]
    pub const fn new(role: Role) -> Self {
        Self {
            role,
            content: None,
            images: Vec::new(),
            injected: false,
            reasoning_content: None,
            reasoning_payloads: None,
            tool_calls: None,
            name: None,
            tool_call_id: None,
        }
    }

    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System).with_content(content)
    }

    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User).with_content(content)
    }

    #[must_use]
    pub fn assistant() -> Self {
        Self::new(Role::Assistant)
    }

    #[must_use]
    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    /// The text of the message, empty when it has none.
    #[must_use]
    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or_default()
    }

    /// Folds `other` into this message. Not commutative: identity comes from
    /// `self`, and the first piece of a tool call keeps its identifier.
    ///
    /// # Errors
    ///
    /// Pieces that disagree on the role, the tool name or the call they answer,
    /// and a tool call piece without an index.
    pub fn merge(self, other: Self) -> Result<Self, MergeError> {
        if self.role != other.role {
            return Err(MergeError::Role);
        }
        if self.name != other.name {
            return Err(MergeError::Name);
        }
        if self.tool_call_id != other.tool_call_id {
            return Err(MergeError::ToolCallId);
        }
        let mut calls: Vec<ToolCall> = Vec::new();
        for call in self
            .tool_calls
            .into_iter()
            .flatten()
            .chain(other.tool_calls.into_iter().flatten())
        {
            let index = call.index.ok_or(MergeError::MissingIndex)?;
            match calls.iter_mut().find(|known| known.index == Some(index)) {
                None => calls.push(call),
                Some(known) => {
                    match (&known.name, &call.name) {
                        (Some(left), Some(right)) if !left.is_empty() && !right.is_empty() => {
                            if left != right {
                                return Err(MergeError::ToolName);
                            }
                        }
                        (left, Some(right))
                            if left.as_deref().is_none_or(str::is_empty) && !right.is_empty() =>
                        {
                            known.name = Some(right.clone());
                        }
                        _ => {}
                    }
                    let mut arguments = known.arguments.take().unwrap_or_default();
                    arguments.push_str(call.arguments.as_deref().unwrap_or_default());
                    known.arguments = Some(arguments);
                }
            }
        }
        let mut payloads = self.reasoning_payloads.unwrap_or_default();
        payloads.extend(other.reasoning_payloads.unwrap_or_default());
        Ok(Self {
            role: self.role,
            content: concatenated(self.content, other.content),
            images: if self.images.is_empty() {
                other.images
            } else {
                self.images
            },
            injected: self.injected,
            reasoning_content: concatenated(self.reasoning_content, other.reasoning_content),
            reasoning_payloads: (!payloads.is_empty()).then_some(payloads),
            tool_calls: (!calls.is_empty()).then_some(calls),
            name: self.name,
            tool_call_id: self.tool_call_id,
        })
    }
}

fn concatenated(left: Option<String>, right: Option<String>) -> Option<String> {
    let mut text = left.unwrap_or_default();
    text.push_str(right.as_deref().unwrap_or_default());
    (!text.is_empty()).then_some(text)
}

/// Why two pieces of an answer could not be folded together. Reference raises
/// `ValueError` from `LLMMessage.__add__`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MergeError {
    #[error("answer pieces disagree on their role")]
    Role,
    #[error("answer pieces disagree on their name")]
    Name,
    #[error("answer pieces answer different tool calls")]
    ToolCallId,
    #[error("a tool call piece carries no index")]
    MissingIndex,
    #[error("tool call pieces disagree on the tool name")]
    ToolName,
}

/// Token counts of one call. `cached_tokens` is the part of `prompt_tokens`
/// the provider served from its cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}

impl std::ops::Add for Usage {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            prompt_tokens: self.prompt_tokens.saturating_add(other.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_add(other.completion_tokens),
            cached_tokens: self.cached_tokens.saturating_add(other.cached_tokens),
        }
    }
}

/// Why an answer ended, with the provider's reasons for a refusal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StopInfo {
    pub reason: Option<String>,
    pub category: Option<String>,
    pub explanation: Option<String>,
}

impl StopInfo {
    #[must_use]
    pub fn reason(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn is_refusal(&self) -> bool {
        self.reason.as_deref() == Some("refusal")
    }
}

/// One piece of an answer, or a whole one.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub message: Message,
    pub usage: Option<Usage>,
    pub correlation_id: Option<String>,
    pub stop: Option<StopInfo>,
}

impl Chunk {
    /// A piece carrying only `message`.
    #[must_use]
    pub const fn of(message: Message) -> Self {
        Self {
            message,
            usage: None,
            correlation_id: None,
            stop: None,
        }
    }

    /// Folds `other` into this chunk.
    ///
    /// # Errors
    ///
    /// The messages cannot be merged; see [`Message::merge`].
    pub fn merge(self, other: Self) -> Result<Self, MergeError> {
        let usage = match (self.usage, other.usage) {
            (None, None) => None,
            (left, right) => Some(left.unwrap_or_default() + right.unwrap_or_default()),
        };
        Ok(Self {
            message: self.message.merge(other.message)?,
            usage,
            correlation_id: other.correlation_id.or(self.correlation_id),
            stop: other.stop.or(self.stop),
        })
    }
}

/// A tool the model may call. Reference `AvailableTool`.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl Tool {
    /// The chat-completions declaration, `{"type": "function", "function": ...}`.
    #[must_use]
    pub fn chat_declaration(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            },
        })
    }
}

/// How the model is told to pick a tool. Reference `StrToolChoice | AvailableTool`.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolChoice {
    Auto,
    None,
    Any,
    Required,
    /// This one tool, declared in full.
    Tool(Tool),
}

impl ToolChoice {
    /// The word a string choice is written as, `None` for a named tool.
    #[must_use]
    pub const fn keyword(&self) -> Option<&'static str> {
        match self {
            Self::Auto => Some("auto"),
            Self::None => Some("none"),
            Self::Any => Some("any"),
            Self::Required => Some("required"),
            Self::Tool(_) => None,
        }
    }
}

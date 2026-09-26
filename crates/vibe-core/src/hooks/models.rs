//! The values a hook run passes around: the declared hook, the invocation a
//! hook reads on stdin, what one execution returned, and what the chain yields
//! to the loop that runs it.
//!
//! Reference `vibe/core/hooks/models.py`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::detail::HookScope;
pub use crate::events::detail::HookSeverity;

/// When a hook runs. Reference `HookType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookType {
    PostAgent,
    PreTool,
    PostTool,
}

impl HookType {
    /// The type as a hook file spells it, which is also what a hook span and
    /// the invocation's `hook_event_name` carry.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PostAgent => "post_agent",
            Self::PreTool => "pre_tool",
            Self::PostTool => "post_tool",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "post_agent" => Some(Self::PostAgent),
            "pre_tool" => Some(Self::PreTool),
            "post_tool" => Some(Self::PostTool),
            _ => None,
        }
    }
}

impl From<HookType> for HookScope {
    fn from(value: HookType) -> Self {
        match value {
            HookType::PostAgent => Self::PostAgent,
            HookType::PreTool => Self::PreTool,
            HookType::PostTool => Self::PostTool,
        }
    }
}

/// The timeout a hook runs under when its declaration names none, in seconds.
/// Reference `_DEFAULT_HOOK_TIMEOUT`.
pub const DEFAULT_HOOK_TIMEOUT: f64 = 60.0;

/// One `[[hooks]]` entry. Reference `HookConfig`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HookConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub hook_type: HookType,
    pub command: String,
    /// The tool name pattern a tool hook is limited to, `*` when absent.
    #[serde(rename = "match")]
    pub matcher: Option<String>,
    /// Seconds, as the reference's float.
    pub timeout: f64,
    pub strict: bool,
    pub description: Option<String>,
}

/// Why a hook file, or one of its entries, was not loaded. Reference
/// `HookConfigIssue`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookConfigIssue {
    pub file: PathBuf,
    pub message: String,
}

/// Every hook a session loaded, and why the rest were not. Reference
/// `HookConfigResult`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HookConfigResult {
    pub hooks: Vec<HookConfig>,
    pub issues: Vec<HookConfigIssue>,
}

/// The session fields every invocation carries. Reference
/// `HookSessionContext`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookSessionContext {
    pub session_id: String,
    /// The session's resolved `messages.jsonl`, or empty when the session is
    /// not logged.
    pub transcript_path: String,
    pub cwd: String,
    pub parent_session_id: Option<String>,
}

/// How a tool call ended, as a post-tool hook reads it. Reference
/// `ToolStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Success,
    Failure,
    Cancelled,
}

/// What one hook reads on stdin. Reference `HookInvocation`, the union of
/// `PostAgentInvocation`, `PreToolInvocation` and `PostToolInvocation`.
#[derive(Debug, Clone, PartialEq)]
pub enum HookInvocation {
    PostAgent {
        context: HookSessionContext,
    },
    PreTool {
        context: HookSessionContext,
        tool_name: String,
        tool_call_id: String,
        tool_input: Value,
    },
    PostTool {
        context: HookSessionContext,
        tool_name: String,
        tool_call_id: String,
        tool_input: Value,
        tool_status: ToolStatus,
        tool_output: Option<Value>,
        tool_output_text: String,
        tool_error: Option<String>,
        duration_ms: f64,
    },
}

impl HookInvocation {
    #[must_use]
    pub const fn hook_type(&self) -> HookType {
        match self {
            Self::PostAgent { .. } => HookType::PostAgent,
            Self::PreTool { .. } => HookType::PreTool,
            Self::PostTool { .. } => HookType::PostTool,
        }
    }

    /// The tool a tool hook guards.
    #[must_use]
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            Self::PostAgent { .. } => None,
            Self::PreTool { tool_name, .. } | Self::PostTool { tool_name, .. } => Some(tool_name),
        }
    }

    /// The call a tool hook guards.
    #[must_use]
    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::PostAgent { .. } => None,
            Self::PreTool { tool_call_id, .. } | Self::PostTool { tool_call_id, .. } => {
                Some(tool_call_id)
            }
        }
    }

    /// The bytes written to the hook's stdin. Reference
    /// `invocation.model_dump_json()`: compact, the session fields first, then
    /// `hook_event_name`, then the type's own fields in declaration order.
    pub fn to_json(&self) -> serde_json::Result<Vec<u8>> {
        match self {
            Self::PostAgent { context } => serde_json::to_vec(&PostAgentWire {
                session_id: &context.session_id,
                transcript_path: &context.transcript_path,
                cwd: &context.cwd,
                parent_session_id: context.parent_session_id.as_deref(),
                hook_event_name: HookType::PostAgent.label(),
            }),
            Self::PreTool {
                context,
                tool_name,
                tool_call_id,
                tool_input,
            } => serde_json::to_vec(&PreToolWire {
                session_id: &context.session_id,
                transcript_path: &context.transcript_path,
                cwd: &context.cwd,
                parent_session_id: context.parent_session_id.as_deref(),
                hook_event_name: HookType::PreTool.label(),
                tool_name,
                tool_call_id,
                tool_input,
            }),
            Self::PostTool {
                context,
                tool_name,
                tool_call_id,
                tool_input,
                tool_status,
                tool_output,
                tool_output_text,
                tool_error,
                duration_ms,
            } => serde_json::to_vec(&PostToolWire {
                session_id: &context.session_id,
                transcript_path: &context.transcript_path,
                cwd: &context.cwd,
                parent_session_id: context.parent_session_id.as_deref(),
                hook_event_name: HookType::PostTool.label(),
                tool_name,
                tool_call_id,
                tool_input,
                tool_status: *tool_status,
                tool_output: tool_output.as_ref(),
                tool_output_text,
                tool_error: tool_error.as_deref(),
                duration_ms: *duration_ms,
            }),
        }
    }
}

#[derive(Serialize)]
struct PostAgentWire<'a> {
    session_id: &'a str,
    transcript_path: &'a str,
    cwd: &'a str,
    parent_session_id: Option<&'a str>,
    hook_event_name: &'static str,
}

#[derive(Serialize)]
struct PreToolWire<'a> {
    session_id: &'a str,
    transcript_path: &'a str,
    cwd: &'a str,
    parent_session_id: Option<&'a str>,
    hook_event_name: &'static str,
    tool_name: &'a str,
    tool_call_id: &'a str,
    tool_input: &'a Value,
}

#[derive(Serialize)]
struct PostToolWire<'a> {
    session_id: &'a str,
    transcript_path: &'a str,
    cwd: &'a str,
    parent_session_id: Option<&'a str>,
    hook_event_name: &'static str,
    tool_name: &'a str,
    tool_call_id: &'a str,
    tool_input: &'a Value,
    tool_status: ToolStatus,
    tool_output: Option<&'a Value>,
    tool_output_text: &'a str,
    tool_error: Option<&'a str>,
    duration_ms: f64,
}

/// What one hook process returned. Reference `HookExecutionResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookExecutionResult {
    pub hook_name: String,
    /// `None` only when the hook timed out; a hook a signal ended reports the
    /// negated signal, as Python's `returncode` does.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// What a hook run reports to the transcript. Reference `HookRunStartEvent`,
/// `HookRunEndEvent`, `HookStartEvent` and `HookEndEvent`.
///
/// `scope` and `tool_call_id` let a consumer route the events of concurrent
/// tool-call chains that interleave.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum HookEvent {
    RunStarted {
        scope: HookType,
        tool_name: Option<String>,
        tool_call_id: Option<String>,
    },
    RunCompleted {
        scope: HookType,
        tool_call_id: Option<String>,
    },
    Started {
        hook_name: String,
        scope: HookType,
        tool_call_id: Option<String>,
    },
    Completed {
        hook_name: String,
        status: HookSeverity,
        content: Option<String>,
        scope: HookType,
        tool_call_id: Option<String>,
    },
}

/// One item a hook chain yields. Reference `_HookYield`: the transcript
/// events, plus the decision values the loop acts on.
#[derive(Debug, Clone, PartialEq)]
pub enum HookYield {
    Event(HookEvent),
    /// A post-agent denial: `content` is injected as a retry user message.
    UserMessage(String),
    /// A pre-tool denial: `content` becomes the tool error the model reads.
    ToolDenial {
        hook_name: String,
        content: String,
    },
    /// A pre-tool rewrite, one per rewriting hook, validated by the loop as it
    /// arrives.
    ToolInputRewrite {
        hook_name: String,
        tool_input: Value,
    },
    /// A post-tool change: the cumulative model-bound output after it.
    TextReplacement(String),
}

//! The tool surface a session publishes, and how one call runs.
//!
//! A tool is a [`ToolSpec`] the model reads and a [`ToolHandler`] that answers
//! it; [`ToolRegistry`] (`registry`) holds the session's tools and executes a
//! call through validation, the handler and the output budget. What a call's
//! arguments and a spec's schemas must satisfy is `validate`, and the result
//! document a reference tool answers with is `document`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::engine::{ToolStreamItem, ToolStreamSink};
use crate::text::truncate_utf8;

pub mod document;
mod registry;
mod validate;

pub use registry::{RegistrationOutcome, ToolCondition, ToolRegistry};
pub(crate) use validate::validate_arguments;
pub use validate::{SchemaViolation, apply_defaults, coerce_and_validate};
use validate::{
    render_violations, validate_output_schema, validate_schema_object, validate_tool_name,
};

pub mod builtins;
pub mod config;
pub mod descriptions;
pub mod shell;

#[cfg(test)]
mod coercion_tests;

pub const DEFAULT_MAX_TOOL_OUTPUT_BYTES: usize = 1_048_576;
pub(crate) const MAX_TOOL_ERROR_BYTES: usize = 16_384;

pub type ToolHandlerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolExecutionOutput, ToolError>> + Send + 'a>>;
pub type OwnedToolHandlerFuture =
    Pin<Box<dyn Future<Output = Result<ToolExecutionOutput, ToolError>> + Send + 'static>>;

#[derive(Clone)]
pub struct ToolOutputSink {
    stream: Option<ToolStreamSink>,
    streamed_bytes: Arc<AtomicU64>,
    max_output_bytes: usize,
}

impl ToolOutputSink {
    fn new(stream: Option<ToolStreamSink>, max_output_bytes: usize) -> Self {
        Self {
            stream,
            streamed_bytes: Arc::new(AtomicU64::new(0)),
            max_output_bytes,
        }
    }

    #[must_use]
    pub fn discard(max_output_bytes: usize) -> Self {
        Self::new(None, max_output_bytes)
    }

    pub fn emit(&self, chunk: impl Into<String>) -> Result<(), ToolError> {
        let chunk = chunk.into();
        let chunk_bytes = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        let mut previous = self.streamed_bytes.load(Ordering::Acquire);
        while let Err(current) = self.streamed_bytes.compare_exchange_weak(
            previous,
            previous.saturating_add(chunk_bytes),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            previous = current;
        }
        let actual = previous.saturating_add(chunk_bytes);
        let limit = u64::try_from(self.max_output_bytes).unwrap_or(u64::MAX);
        if actual > limit {
            return Err(ToolError::OutputTooLarge {
                actual: usize::try_from(actual).unwrap_or(usize::MAX),
                limit: self.max_output_bytes,
            });
        }
        if let Some(stream) = &self.stream {
            stream(ToolStreamItem::Output(chunk)).map_err(ToolError::Execution)?;
        }
        Ok(())
    }

    /// Names the session a delegation opened for this call, which its effect
    /// carries from then on.
    pub fn link_child_session(&self, child_session_id: &str) -> Result<(), ToolError> {
        if let Some(stream) = &self.stream {
            stream(ToolStreamItem::ChildSession(child_session_id.to_owned()))
                .map_err(ToolError::Execution)?;
        }
        Ok(())
    }

    fn bytes(&self) -> usize {
        usize::try_from(self.streamed_bytes.load(Ordering::Acquire)).unwrap_or(usize::MAX)
    }

    #[must_use]
    pub fn remaining_bytes(&self) -> usize {
        self.max_output_bytes.saturating_sub(self.bytes())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAvailability {
    Available,
    Disabled,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPresentationKind {
    Generic,
    Read,
    Search,
    Diff,
    Shell,
    Mcp,
    Connector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    BuiltIn,
    Mcp,
    Connector,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub state: Value,
    pub availability: ToolAvailability,
    pub presentation: ToolPresentationKind,
    pub source: ToolSource,
    #[serde(default)]
    pub selection_priority: i32,
}

impl ToolSpec {
    pub fn validate(&self) -> Result<(), ToolError> {
        validate_tool_name(&self.name)?;
        validate_schema_object(&self.input_schema, "input")?;
        if let Some(schema) = &self.output_schema {
            validate_output_schema(schema)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInvocation {
    pub call_id: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionOutput {
    pub typed_result: Value,
    /// What a client renders instead of the typed result, or null when the
    /// tool publishes no second document.
    ///
    /// Reference `ToolUIData.project_result` returns `None` for every tool but
    /// `grep` and `edit`, and the app-server falls back to the typed result
    /// when it does, so null here means the same thing: the two documents are
    /// one.
    #[serde(default)]
    pub projected_result: Value,
    pub model_text: String,
    #[serde(default)]
    pub display: Value,
    #[serde(default)]
    pub chunks: Vec<String>,
    /// Set when the call never ran because its approval was refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<ToolSkip>,
    /// Set when the answer the call waited on was refused, which fails the
    /// whole turn with this message. Reference `CallbackRejectedError`
    /// (`vibe/app_server/_turns.py`) raises out of the tool and ends the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_failure: Option<String>,
    /// How the permission gate settled the call, when one guarded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ToolApproval>,
    /// Set when the call ran and failed after the gate let it through: the
    /// error the model reads, carried here rather than as an `Err` so the
    /// gate's answer still reaches the settled effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// A message the turn appends unseen once the step that ran the call has
    /// settled, before its next request. Reference
    /// `AgentLoop._pending_injected_messages`, which `_drain_pending_injections`
    /// appends at the end of the step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_injection: Option<String>,
    /// The `tools/call` result a proxied MCP or connector call came back
    /// with, untouched, for a caller that keeps its content blocks rather
    /// than the prose above. Never serialized.
    #[serde(skip)]
    pub remote_result: Option<Value>,
}

/// How a call's permission was settled. Reference `ToolResultEvent.decision`,
/// `approval_type` and `approval_source`, which `AgentLoop._should_execute_tool`
/// records on every call it gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolApproval {
    pub decision: ToolVerdict,
    pub approval_type: ToolApprovalType,
    pub approval_source: ToolApprovalSource,
}

impl ToolApproval {
    /// A call the operator declined when asked.
    #[must_use]
    pub fn declined() -> Self {
        Self {
            decision: ToolVerdict::Skip,
            approval_type: ToolApprovalType::Ask,
            approval_source: ToolApprovalSource::User,
        }
    }
}

/// Whether the gate let the call run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolVerdict {
    Execute,
    Skip,
}

/// The permission the call was held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalType {
    Always,
    Never,
    Ask,
}

/// What answered for the call: the tool's configuration, a rule an earlier
/// approval stored, the operator, an agent that bypasses the gate, or a
/// permission that refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalSource {
    Config,
    Smart,
    User,
    Bypass,
    Never,
}

/// A call the user declined. Reference `ToolResultEvent.skipped`: the reason
/// is what the model reads, and a decline that gave no reason cancels the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSkip {
    pub cancelled: bool,
}

impl ToolExecutionOutput {
    /// A result whose whole content is what the model reads.
    ///
    /// The typed result is the same string, because a tool that answers in
    /// prose has no structure to publish beside it.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            typed_result: Value::String(text.clone()),
            projected_result: Value::Null,
            model_text: text,
            display: Value::Null,
            chunks: Vec::new(),
            skip: None,
            turn_failure: None,
            approval: None,
            failure: None,
            pending_injection: None,
            remote_result: None,
        }
    }

    /// A result carrying `model_text`, with nothing typed or displayed yet.
    ///
    /// The two optional halves are added by [`Self::displayed_as`] and
    /// [`Self::typed`]. Every tool composes its output through these rather
    /// than through a struct literal, so a field added to the output reaches
    /// every tool at once instead of at thirty call sites.
    #[must_use]
    pub fn new(model_text: impl Into<String>) -> Self {
        Self {
            typed_result: Value::Null,
            projected_result: Value::Null,
            model_text: model_text.into(),
            display: Value::Null,
            chunks: Vec::new(),
            skip: None,
            turn_failure: None,
            approval: None,
            failure: None,
            pending_injection: None,
            remote_result: None,
        }
    }

    /// The same result, with what a client renders for it.
    #[must_use]
    pub fn displayed_as(mut self, display: Value) -> Self {
        self.display = display;
        self
    }

    /// The same result, with the structure a caller reads instead of the prose.
    #[must_use]
    pub fn typed(mut self, typed_result: Value) -> Self {
        self.typed_result = typed_result;
        self
    }

    /// The same result, with the document a client reads instead of the typed
    /// one.
    ///
    /// A tool that never calls this publishes one document for both readers,
    /// which is what the reference's default projection means.
    #[must_use]
    pub fn projected(mut self, projected_result: Value) -> Self {
        self.projected_result = projected_result;
        self
    }

    /// The same result, keeping the `tools/call` result it was read from.
    #[must_use]
    pub fn from_remote(mut self, result: Value) -> Self {
        self.remote_result = Some(result);
        self
    }
}

tokio::task_local! {
    /// What the call being polled reached: whether it passed its permission
    /// gate and its body started, and the decision that let it. Reference
    /// `tool_started` and `decision` in `_execute_tool_call`, which decide
    /// whether a cancelled call is shown to the post-tool hooks and what
    /// provenance its interrupted answer carries.
    static CALL_PROGRESS: Arc<CallProgress>;
}

/// How far one call got before it settled or was cancelled.
#[derive(Debug, Default)]
pub struct CallProgress {
    started: std::sync::atomic::AtomicBool,
    approval: std::sync::Mutex<Option<ToolApproval>>,
    awaiting_approval: std::sync::atomic::AtomicBool,
}

impl CallProgress {
    /// Whether the call's body started.
    #[must_use]
    pub fn started(&self) -> bool {
        self.started.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The decision the permission gate reached, once it reached one.
    #[must_use]
    pub fn approval(&self) -> Option<ToolApproval> {
        *crate::sync::lock(&self.approval)
    }

    /// Whether the call is waiting on the operator's answer to its approval
    /// request.
    #[must_use]
    pub fn awaiting_approval(&self) -> bool {
        self.awaiting_approval
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Records whether the call being polled is waiting on its approval request.
/// A call cancelled while waiting keeps the mark.
pub fn mark_awaiting_approval(waiting: bool) {
    let _ = CALL_PROGRESS.try_with(|progress| {
        progress
            .awaiting_approval
            .store(waiting, std::sync::atomic::Ordering::SeqCst);
    });
}

/// Records that the call being polled passed its gate under `approval` and
/// started running. A call polled outside [`track_call_start`] records
/// nothing.
pub fn mark_call_started(approval: ToolApproval) {
    let _ = CALL_PROGRESS.try_with(|progress| {
        *crate::sync::lock(&progress.approval) = Some(approval);
        progress
            .started
            .store(true, std::sync::atomic::Ordering::SeqCst);
    });
}

tokio::task_local! {
    /// The decision a caller reached before running the call being polled,
    /// which a guarded tool takes instead of asking again: the Unified Harness
    /// gates a call to a server's or connector's tool itself, then runs it
    /// through the session's registry.
    static GATED: ToolApproval;
}

/// Polls `future`, a call [`ToolRegistry::invoke`] runs, as one its caller
/// already let through under `approval`.
pub async fn run_gated<F: std::future::Future>(approval: ToolApproval, future: F) -> F::Output {
    GATED.scope(approval, future).await
}

/// The decision the caller of the call being polled already reached, if it
/// reached one.
#[must_use]
pub fn gated_approval() -> Option<ToolApproval> {
    GATED.try_with(|approval| *approval).ok()
}

/// Polls `future` with `progress` as what [`mark_call_started`] records into.
pub async fn track_call_start<F: std::future::Future>(
    progress: Arc<CallProgress>,
    future: F,
) -> F::Output {
    CALL_PROGRESS.scope(progress, future).await
}

/// What deciding a call without asking anyone or running it resolves to.
pub type PermissionFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<crate::policy::PermissionOutcome, crate::policy::PolicyError>>
            + Send
            + 'a,
    >,
>;

pub trait ToolHandler: Send + Sync {
    fn invoke<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
        output: ToolOutputSink,
    ) -> ToolHandlerFuture<'a>;

    /// What the policy in front of the handler makes of `invocation`, without
    /// asking anyone or running it. `None` for a handler that guards nothing
    /// of its own.
    ///
    /// A backend that executes the call elsewhere still decides it here, so
    /// both backends answer an approval off the same rules and the same
    /// session grants (reference `UnifiedPermissionResolver`, which binds each
    /// tool's `resolve_permission` and the session's `PermissionStore` to the
    /// unified harness).
    fn resolve_permission<'a>(
        &'a self,
        _invocation: &'a ToolInvocation,
    ) -> Option<PermissionFuture<'a>> {
        None
    }
}

impl<F> ToolHandler for F
where
    F: Fn(&ToolInvocation, ToolOutputSink) -> OwnedToolHandlerFuture + Send + Sync,
{
    fn invoke<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
        output: ToolOutputSink,
    ) -> ToolHandlerFuture<'a> {
        self(invocation, output)
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    /// A call the permission gate let through and that then failed.
    #[error("{source}")]
    Approved {
        approval: ToolApproval,
        source: Box<ToolError>,
    },
    /// The answer the call waited on was refused, which fails the turn.
    #[error("{0}")]
    TurnFailed(String),
    #[error("invalid tool name `{0}`")]
    InvalidName(String),
    #[error("tool `{name}` is published by two sources: {existing}, {incoming}")]
    DuplicateName {
        name: String,
        existing: String,
        incoming: String,
    },
    #[error("tool `{0}` is unavailable")]
    Unavailable(String),
    #[error("tool `{0}` panicked during execution")]
    Panicked(String),
    #[error("invalid {kind} schema: {message}")]
    InvalidSchema { kind: &'static str, message: String },
    #[error("schema validation failed at {path}: {message}")]
    SchemaViolation { path: String, message: String },
    /// One call's arguments failed the tool's schema, named by the tool that
    /// refused them and carrying every place the payload was wrong.
    ///
    /// The reference raises from the tool wrapper and Pydantic collects, so a
    /// model reads back which tool objected and to which arguments. Reporting
    /// only the first would send it round the loop once per broken argument.
    #[error("tool `{tool}` rejected the arguments: {}", render_violations(.violations))]
    InvalidArguments {
        tool: String,
        violations: Vec<SchemaViolation>,
    },
    #[error("invalid typed tool result: {0}")]
    InvalidResult(String),
    #[error("tool output is {actual} bytes, exceeding the {limit}-byte limit")]
    OutputTooLarge { actual: usize, limit: usize },
    #[error("{0}")]
    Execution(String),
}

impl ToolError {
    fn bounded(self) -> Self {
        match self {
            Self::Execution(message) => {
                Self::Execution(truncate_utf8(&message, MAX_TOOL_ERROR_BYTES).to_owned())
            }
            Self::InvalidResult(message) => {
                Self::InvalidResult(truncate_utf8(&message, MAX_TOOL_ERROR_BYTES).to_owned())
            }
            error => error,
        }
    }
}

#[cfg(test)]
mod tools_tests;

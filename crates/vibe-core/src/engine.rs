//! One conversation turn, from the operator's message to the stop reason.
//!
//! [`ConversationEngine`] is generic over its four collaborators
//! (`contracts`); everything else a turn runs under is one `TurnSettings`
//! value. The turn itself is split by concern: `turn` drives the cycle,
//! `controls` drains what a client queues into a running turn, `streaming`
//! makes and reports one model call, `compact` compacts the transcript,
//! `tool_round` runs one reply's tool calls concurrently, and `tool_call` runs
//! one call from its pre-tool hooks to its post-tool hooks.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::compaction::{CompactionFailure, CompactionFailureReason};
use crate::events::{
    EngineEvent, EventEnvelope, ModelMessage, ProjectionError, ProjectionSnapshot,
    PublicContentBlock,
};
use crate::hooks::{HookSessionContext, HooksManager};
use crate::middleware::{CompactionSettings, ConversationMiddleware};
use crate::provider::{AssistantMessage, ProviderError, ProviderStream, Usage};
use crate::tools::{ToolExecutionOutput, ToolVerdict};
use crate::tracing::{BackendFailure, TracedError};

mod compact;
mod contracts;
mod controls;
mod ledger;
mod streaming;
mod tool_call;
mod tool_round;
mod turn;

pub use compact::SummaryRequestRecorder;
pub use contracts::{
    Compactor, CompletionProvider, CompositeEventObserver, EventObserver, NoTools,
    NoopEventObserver, NoopTranscriptSink, RejectCompaction, SessionTranscriptSink, ToolExecutor,
    TranscriptSink, chunks_of,
};
pub use controls::{CancellationToken, TurnControl, TurnControlHandle};

pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AssistantMessage, ProviderError>> + Send + 'a>>;
pub type ProviderStreamFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderStream<'a>, ProviderError>> + Send + 'a>>;

pub type ToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolExecutionOutput, String>> + Send + 'a>>;
/// What a running tool reports before it settles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStreamItem {
    /// Output text, appended to what the call has shown so far.
    Output(String),
    /// The session a delegation opened for the call, which its effect names
    /// from then on. Reference `TurnController.link_subagent`.
    ChildSession(String),
}

impl From<String> for ToolStreamItem {
    fn from(chunk: String) -> Self {
        Self::Output(chunk)
    }
}

pub type ToolStreamSink = Arc<dyn Fn(ToolStreamItem) -> Result<(), String> + Send + Sync>;
pub type CompactionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompactionResult, CompactionFailure>> + Send + 'a>>;
pub type PersistenceFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// What a call the interrupt cut short answers. Reference
/// `get_user_cancellation_message(TOOL_INTERRUPTED)`: the cancellation tag is
/// reproduced, the sentence inside it is this port's own.
const INTERRUPTED_TOOL_RESULT: &str =
    "<user_cancellation>Tool execution interrupted</user_cancellation>";
/// The profile a turn runs under when its caller names none. Reference
/// `BuiltinAgentName.DEFAULT`.
pub const DEFAULT_AGENT_PROFILE: &str = "default";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStats {
    pub usage: Usage,
    pub context_tokens: u64,
    pub steps: u32,
    /// The last model call's own usage and duration, which the reference
    /// keeps as `last_turn_*` on `AgentStats` and restores with a session.
    pub last_call: Option<ModelCallStats>,
    pub tool_calls: ToolCallTally,
}

/// What one model call spent, and how long it took.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCallStats {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub duration_ms: u64,
}

/// How the session's tool calls settled: reference `AgentStats.tool_calls_*`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCallTally {
    /// Calls whose body started.
    pub agreed: u64,
    /// Calls the permission gate or the operator declined.
    pub rejected: u64,
    /// Calls a pre-tool hook denied.
    pub hook_denied: u64,
    /// Calls that never resolved, raised, or were interrupted.
    pub failed: u64,
    /// Calls whose body returned.
    pub succeeded: u64,
}

impl ToolCallTally {
    /// Counts the call `event` settles, as reference `AgentLoop` counts it
    /// where the answer is decided (`vibe/core/agent_loop/_loop.py`,
    /// `vibe/core/agent_loop_hooks.py`). Every other event is ignored.
    pub fn observe(&mut self, event: &EngineEvent) {
        let (before, after) = Self::settled_by(event);
        *self = self
            .plus(before)
            .plus(after)
            .plus(Self::abandoned_by(event));
    }

    /// The call `event` abandoned at its gate when the turn ended, which the
    /// reference fails once the turn's closing accounting is published.
    #[must_use]
    pub fn abandoned_by(event: &EngineEvent) -> Self {
        Self {
            failed: u64::from(matches!(event, EngineEvent::ToolCallAbandoned { .. })),
            ..Self::default()
        }
    }

    /// What `event` settles, split by when the reference counts it: before it
    /// yields the result event, or once its consumer has read it. A client
    /// reading the accounting while the event is published sees only the
    /// first part. A call that never resolved and a call that returned are
    /// counted after their result event; a refusal, a denial, a failure and an
    /// interruption before it.
    #[must_use]
    pub fn settled_by(event: &EngineEvent) -> (Self, Self) {
        let mut before = Self::default();
        let mut after = Self::default();
        let EngineEvent::ToolResult {
            is_error,
            skipped,
            approval,
            ..
        } = event
        else {
            return (before, after);
        };
        if *skipped {
            // A pre-tool hook denies before any gate answers; the gate's own
            // refusal carries its decision.
            if approval.is_unset() {
                before.hook_denied = 1;
            } else {
                before.rejected = 1;
            }
        } else if !*is_error {
            before.agreed = 1;
            after.succeeded = 1;
        } else if approval.is_unset() {
            // Reference `_emit_failed_tool_events`: a call that failed to
            // resolve never reached its gate.
            after.failed = 1;
        } else {
            // A call granted at its gate started its body before it failed or
            // was interrupted.
            if approval.decision == Some(ToolVerdict::Execute) {
                before.agreed = 1;
            }
            before.failed = 1;
        }
        (before, after)
    }

    /// `self` with `other` counted on top.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            agreed: self.agreed.saturating_add(other.agreed),
            rejected: self.rejected.saturating_add(other.rejected),
            hook_denied: self.hook_denied.saturating_add(other.hook_denied),
            failed: self.failed.saturating_add(other.failed),
            succeeded: self.succeeded.saturating_add(other.succeeded),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineLimits {
    pub max_steps: i64,
    pub max_total_tokens: i64,
    pub max_price_micros: i64,
    pub input_price_per_million_micros: u64,
    pub output_price_per_million_micros: u64,
    pub max_response_bytes: usize,
}

/// A limit at the maximum of its type is absent: the reference registers a
/// budget policy only for a limit it was given, so nothing caps a turn by
/// default.
impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            max_steps: i64::MAX,
            max_total_tokens: i64::MAX,
            max_price_micros: i64::MAX,
            input_price_per_million_micros: 0,
            output_price_per_million_micros: 0,
            max_response_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionResult {
    pub new_session_id: String,
    pub summary: String,
    pub messages: Vec<ModelMessage>,
    /// What the summarization spent. The reference makes every compaction call
    /// through the same accounted path a turn's own requests go through, so the
    /// token and price ceilings an operator set cover them too.
    pub usage: Usage,
    /// The classified failure a compaction degraded from, when its summary is
    /// the placeholder. A compaction that produced a real summary carries none.
    pub failure: Option<CompactionFailureReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStopReason {
    Complete,
    MaxSteps,
    TokenLimit,
    PriceLimit,
    Refusal,
    ResponseLength,
    Cancelled,
    Failed,
}

impl TurnStopReason {
    /// Whether the turn stopped on an operator-set ceiling (steps, tokens or
    /// price), which each surface reports as its own limit outcome.
    #[must_use]
    pub const fn is_limit(&self) -> bool {
        matches!(self, Self::MaxSteps | Self::TokenLimit | Self::PriceLimit)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    pub session_id: String,
    pub events: Vec<EventEnvelope>,
    pub snapshot: ProjectionSnapshot,
    pub messages: Vec<ModelMessage>,
    pub usage: Usage,
    pub context_tokens: u64,
    pub price_micros: u64,
    pub steps: u32,
    pub checkpoints: u32,
    pub stop_reason: TurnStopReason,
}

/// What a turn runs under, independently of who provides, tools, compacts or
/// persists it.
///
/// The four collaborators are type parameters, so swapping one rebuilds the
/// engine's type. Keeping everything that is *not* a collaborator in one value
/// means [`ConversationEngine::with_tools`], [`ConversationEngine::with_compactor`]
/// and [`ConversationEngine::with_sink`] carry it across in one move, and a new
/// setting is declared once instead of in three rebuild sites.
struct TurnSettings {
    limits: EngineLimits,
    baseline: SessionStats,
    observer: Arc<dyn EventObserver>,
    /// Policies registered after the budget ones, which is where the reference
    /// registers automatic compaction and everything that follows it.
    middleware: Vec<Arc<dyn ConversationMiddleware>>,
    compaction: CompactionSettings,
    /// How a slash-invoked skill resolves, when the session publishes one.
    /// Absent, a `/name` prompt is an ordinary message.
    invoked_skills: Option<Arc<dyn crate::skills::InvokedSkillResolver>>,
    /// Whether the harness wrote this turn's prompt. Reference `_loop.py`
    /// appends an injected prompt to the conversation and publishes nothing
    /// for it: no user entry, no skill or mention expansion.
    injected_prompt: bool,
    /// The identifier the client gave the turn's message, which its entry
    /// and its stored message take. Reference `client_message_id`.
    user_message_id: Option<String>,
    user_attachments: Vec<PublicContentBlock>,
    /// The words the operator typed, when the prompt folds attached resources
    /// in, and what the client asked the message to be shown as.
    user_input_text: Option<String>,
    user_display_content: Option<Value>,
    /// The agent profile this turn runs under, which every request and tool
    /// event reports. Reference `self.agent_profile.name`, whose default
    /// profile is named `default`.
    agent_profile: String,
    /// The session's directory, stamped on every event this turn emits.
    working_directory: Option<PathBuf>,
    /// The hooks this turn runs, when the session loaded any.
    hooks: Option<TurnHooks>,
    /// What every request carries as its `metadata`, built per request.
    /// Reference `_build_backend_metadata`.
    request_census: Option<crate::telemetry::TelemetryContext>,
}

/// The hooks a turn runs, and the session fields every invocation carries.
/// Reference `AgentLoopHooksMixin`, whose session context is read per
/// invocation.
#[derive(Clone)]
pub struct TurnHooks {
    pub manager: Arc<HooksManager>,
    /// The session's resolved `messages.jsonl`, or empty when it is not
    /// logged.
    pub transcript_path: String,
    pub cwd: String,
    pub parent_session_id: Option<String>,
}

impl TurnHooks {
    fn context(&self, session_id: &str) -> HookSessionContext {
        HookSessionContext {
            session_id: session_id.to_owned(),
            transcript_path: self.transcript_path.clone(),
            cwd: self.cwd.clone(),
            parent_session_id: self.parent_session_id.clone(),
        }
    }
}

impl Default for TurnSettings {
    fn default() -> Self {
        Self {
            limits: EngineLimits::default(),
            baseline: SessionStats::default(),
            observer: Arc::new(NoopEventObserver),
            middleware: Vec::new(),
            compaction: CompactionSettings::default(),
            invoked_skills: None,
            injected_prompt: false,
            user_message_id: None,
            user_attachments: Vec::new(),
            user_input_text: None,
            user_display_content: None,
            agent_profile: DEFAULT_AGENT_PROFILE.to_owned(),
            working_directory: None,
            hooks: None,
            request_census: None,
        }
    }
}

pub struct ConversationEngine<P, T = NoTools, C = RejectCompaction, S = NoopTranscriptSink> {
    provider: P,
    tools: T,
    compactor: C,
    sink: S,
    settings: TurnSettings,
}

impl<P> ConversationEngine<P> {
    #[must_use]
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            tools: NoTools,
            compactor: RejectCompaction,
            sink: NoopTranscriptSink,
            settings: TurnSettings::default(),
        }
    }
}

impl<P, T, C, S> ConversationEngine<P, T, C, S> {
    #[must_use]
    pub fn with_tools<T2>(self, tools: T2) -> ConversationEngine<P, T2, C, S> {
        ConversationEngine {
            provider: self.provider,
            tools,
            compactor: self.compactor,
            sink: self.sink,
            settings: self.settings,
        }
    }

    #[must_use]
    pub fn with_compactor<C2>(self, compactor: C2) -> ConversationEngine<P, T, C2, S> {
        ConversationEngine {
            provider: self.provider,
            tools: self.tools,
            compactor,
            sink: self.sink,
            settings: self.settings,
        }
    }

    #[must_use]
    pub fn with_sink<S2>(self, sink: S2) -> ConversationEngine<P, T, C, S2> {
        ConversationEngine {
            provider: self.provider,
            tools: self.tools,
            compactor: self.compactor,
            sink,
            settings: self.settings,
        }
    }

    #[must_use]
    pub fn with_limits(mut self, limits: EngineLimits) -> Self {
        self.settings.limits = limits;
        self
    }

    /// Registers one conversation policy after the budget policies.
    #[must_use]
    pub fn with_middleware(mut self, middleware: Arc<dyn ConversationMiddleware>) -> Self {
        self.settings.middleware.push(middleware);
        self
    }

    #[must_use]
    pub fn with_compaction_settings(mut self, compaction: CompactionSettings) -> Self {
        self.settings.compaction = compaction;
        self
    }

    #[must_use]
    pub fn with_baseline(mut self, baseline: SessionStats) -> Self {
        self.settings.baseline = baseline;
        self
    }

    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.settings.observer = observer;
        self
    }

    /// Names the turn's message the way its client did.
    #[must_use]
    pub fn with_user_message_id(mut self, message_id: impl Into<String>) -> Self {
        self.settings.user_message_id = Some(message_id.into());
        self
    }

    /// Attaches the images and resources the operator sent with the prompt.
    #[must_use]
    pub fn with_user_attachments(mut self, attachments: Vec<PublicContentBlock>) -> Self {
        self.settings.user_attachments = attachments;
        self
    }

    /// Records what the operator typed, when the prompt folds attached
    /// resources in, and how the client asked the message to be shown.
    /// Reference `_open_user_turn`'s `input_text` and `user_display_content`.
    #[must_use]
    pub fn with_user_input(
        mut self,
        input_text: Option<String>,
        user_display_content: Option<Value>,
    ) -> Self {
        self.settings.user_input_text = input_text;
        self.settings.user_display_content = user_display_content;
        self
    }

    /// Marks this turn's prompt as written by the harness.
    #[must_use]
    pub fn with_injected_prompt(mut self) -> Self {
        self.settings.injected_prompt = true;
        self
    }

    #[must_use]
    pub fn with_invoked_skills(
        mut self,
        resolver: Arc<dyn crate::skills::InvokedSkillResolver>,
    ) -> Self {
        self.settings.invoked_skills = Some(resolver);
        self
    }

    /// Stamps every request with the request census the context builds.
    /// Absent, a request carries no `metadata`.
    #[must_use]
    pub fn with_request_census(mut self, context: crate::telemetry::TelemetryContext) -> Self {
        self.settings.request_census = Some(context);
        self
    }

    /// Names the agent profile this turn runs under. Absent, the turn reports
    /// the reference's default profile.
    #[must_use]
    pub fn with_agent_profile(mut self, profile: impl Into<String>) -> Self {
        self.settings.agent_profile = profile.into();
        self
    }

    /// Runs the session's hooks around this turn's tool calls and at its end.
    #[must_use]
    pub fn with_hooks(mut self, hooks: TurnHooks) -> Self {
        self.settings.hooks = Some(hooks);
        self
    }

    /// Names the directory the session sits in, which every event this turn
    /// emits carries. Absent, a file path is displayed against the process
    /// directory.
    #[must_use]
    pub fn with_working_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.settings.working_directory = Some(directory.into());
        self
    }
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Projection(#[from] ProjectionError),
    #[error(transparent)]
    Provider(ProviderError),
    #[error("compaction failed: {0}")]
    Compaction(String),
    #[error("persistence failed: {0}")]
    Persistence(String),
    #[error("event observation failed: {0}")]
    Observation(String),
    /// A tool's answer failed the turn; the message is the turn's error.
    #[error("{0}")]
    ToolFailure(String),
}

impl TracedError for EngineError {
    fn error_type(&self) -> &'static str {
        "EngineError"
    }

    /// Reference `_backend_error_from` walks the cause chain; the only cause a
    /// turn failure carries here is the provider error it wraps.
    fn backend_failure(&self) -> Option<BackendFailure> {
        match self {
            Self::Provider(error) => error.backend_failure(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod engine_tests;

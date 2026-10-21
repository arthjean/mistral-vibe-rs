//! The turn loop: one user input, model calls, tool calls, until the model
//! answers without asking for a tool.
//!
//! The reference runs it as a native core that a Python runtime drives
//! (`harness/core/`, `harness/runtimes/python/`): every input the core takes
//! commits a step, and the runtime republishes the session's public state
//! after each. What a client and the model see of that is reproduced here:
//! which steps commit, in which order, what each publishes, and what the
//! model is sent. Streamed text is published as it arrives without being
//! journaled, the first fragment at once and the rest at most every 75 ms.
//!
//! The session a turn runs on is [`SessionCore`] (`session_core`), holding the
//! conversation the model reads (`conversation`); a new session can start
//! from another's conversation (`fork`). A turn (`turn`) streams each model
//! call (`stream`), compacts the conversation when it outgrows the model
//! (`compaction_run`), reports a failed call as the reference does
//! (`provider_errors`), checks the calls the model asked for (`calls`), and
//! runs them (`execute`), a `run_typescript` program among them (`program`).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use vibe_core::engine::CompletionProvider;
use vibe_core::provider::config::ModelConfig;

use crate::catalog::ModelTool;
use crate::gate::ToolGate;
use crate::state::{Advance, HarnessState, PublicError};
use crate::tools::files::Workspace;
use crate::tools::shell::ShellContext;

mod calls;
mod compaction_run;
mod conversation;
mod execute;
mod fork;
mod program;
mod provider_errors;
mod session_core;
mod stream;
mod turn;

pub use calls::model_content;
pub(crate) use calls::sorted;
pub use compaction_run::{CompactionEnd, compact_session};
pub use conversation::{ConversationMessage, FILE_IMAGE_LINK};
pub use fork::{fork_core, fork_core_before, fork_core_through, import_core};
pub use session_core::SessionCore;
pub use turn::run_turn;

/// What a client asked a turn to start with.
#[derive(Debug, Clone, Default)]
pub struct TurnRequest {
    pub content: Vec<Value>,
    pub client_message_id: Option<String>,
    pub user_display_content: Option<Value>,
    /// A turn the host started on the user's behalf: the model reads its
    /// message, the public history never shows it (reference
    /// `content_is_injected`).
    pub injected: bool,
}

/// One state the session reached, and whether it was journaled.
#[derive(Debug, Clone)]
pub struct Publication {
    pub state: HarnessState,
    pub advance: Advance,
    /// The callbacks open when it was reached, which the state is shown
    /// blocked on.
    pub callbacks: Vec<Value>,
    /// A callback opened or settled just before it.
    pub event: Option<CallbackEvent>,
}

/// A callback that opened or settled, published ahead of the state.
#[derive(Debug, Clone, PartialEq)]
pub enum CallbackEvent {
    Requested(Value),
    Resolved(Value),
}

/// Where a turn's publications go, in order.
pub type Sink = Arc<dyn Fn(Publication) + Send + Sync>;

/// What the turn runs with.
#[derive(Clone)]
pub struct TurnEnvironment {
    pub provider: Arc<dyn CompletionProvider>,
    pub model: Option<ModelConfig>,
    pub system_prompt: String,
    pub tools: Vec<ModelTool>,
    /// The request metadata every call carries; each call adds its type and
    /// the message it answers.
    pub metadata: BTreeMap<String, String>,
    pub workspace: Workspace,
    pub shell: ShellContext,
    /// What decides whether each tool call runs.
    pub gate: Arc<dyn ToolGate>,
    /// The skills the session reads, by name in the order it holds them,
    /// with what reading each answers.
    pub skills: Vec<(String, String)>,
    /// What a `run_typescript` program can call.
    pub callables: Arc<Vec<crate::programmatic::Callable>>,
    /// The same functions as `search_tool_functions` finds them.
    pub functions: Arc<Vec<crate::discovery::Function>>,
    /// What runs a call to a group the host provides (its MCP servers and
    /// connectors); `None` when it provides none.
    pub provided: Option<Arc<dyn crate::programmatic::ProvidedTools>>,
    /// Where the session is stored, once it is.
    pub session_dir: Option<std::path::PathBuf>,
    /// What the session's subagents run with; `None` in a session that
    /// cannot start any.
    pub subagents: Option<Arc<crate::subagents::SubagentSetup>>,
    /// The message the provider is told each call answers, when it is not
    /// the turn's own input: a subagent reports its parent's.
    pub message_id: Option<String>,
    /// The context size, in tokens, past which a turn compacts its
    /// conversation before its next model call; 0 never does.
    pub compact_threshold: u64,
    /// How many model calls a turn makes at most: reference
    /// `RustTurnSettings.max_iterations`. The call that reaches it still runs
    /// its tools, and the turn completes once their results land.
    pub max_iterations: Option<u32>,
}

/// How a turn ended, for the host.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEnd {
    Completed,
    Interrupted,
    Failed(PublicError),
}

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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio::time::Instant;
use vibe_core::engine::CompletionProvider;
use vibe_core::events::{ModelMessage, ModelToolCall};
use vibe_core::provider::config::ModelConfig;
use vibe_core::provider::{
    ProviderChunk, ProviderError, ProviderInput, RequestDialect, ToolDefinition,
};

use crate::catalog::{self, ModelTool};
use crate::clock::now_millis;
use crate::gate::{
    Approver, AskFuture, CallbackAnswer, CallbackBook, GateVerdict, ToolGate, approval_callback,
    user_input_callback,
};
use crate::state::{
    Advance, CompletionDelta, HarnessState, Observation, Projector, PublicError, ReasoningPart,
    TokenUsage, ToolAction, ToolResult, TransitionTurn,
};
use crate::tools::files::Workspace;
use crate::tools::shell::ShellContext;
use crate::tools::vibe::VibeTools;

/// How long streamed fragments gather before they are published together.
const DELTA_INTERVAL: Duration = Duration::from_millis(75);

/// One message the model is sent back on the next call.
#[derive(Debug, Clone, PartialEq)]
pub enum ConversationMessage {
    User {
        content: Vec<Value>,
        /// Written by the harness rather than typed: injected context, a
        /// compaction's prompt and handoff. A compaction preserves only the
        /// messages a person wrote.
        injected: bool,
    },
    Assistant {
        text: String,
        reasoning: Option<String>,
        tool_calls: Vec<ModelToolCall>,
    },
    Tool {
        call_id: String,
        name: String,
        content: String,
    },
}

impl ConversationMessage {
    /// The message as a model call sends it. A model that reads no image is
    /// handed the file an image was read from in its place
    /// (`image_delivery` `resource_link`); an inline image stays, which the
    /// call then refuses.
    pub(crate) fn to_model(&self, native_images: bool) -> ModelMessage {
        match self {
            Self::User { content, .. } => {
                let delivered = delivered_content(content, native_images);
                ModelMessage::User {
                    content: content_text(&delivered),
                    injected: false,
                    message_id: None,
                    attachments: image_parts(&delivered),
                    user_display_content: None,
                    input_text: None,
                    manual_shell: None,
                    compaction_boundary: false,
                }
            }
            Self::Assistant {
                text,
                reasoning,
                tool_calls,
            } => ModelMessage::Assistant {
                message_id: None,
                reasoning_message_id: None,
                content: text.clone(),
                reasoning: reasoning.clone(),
                reasoning_payloads: Vec::new(),
                tool_calls: tool_calls.clone(),
                keeps_empty_content: false,
            },
            Self::Tool {
                call_id,
                name,
                content,
            } => ModelMessage::Tool {
                call_id: call_id.clone(),
                content: content.clone(),
                is_error: false,
                name: name.clone(),
                result: None,
            },
        }
    }

    /// Whether the message still holds an image once delivered to a model
    /// that reads them only when `native_images`.
    #[must_use]
    pub fn holds_image(&self, native_images: bool) -> bool {
        match self {
            Self::User { content, .. } => delivered_content(content, native_images)
                .iter()
                .any(|block| block["type"] == "image"),
            _ => false,
        }
    }

    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::User { content, injected } => {
                let mut message = json!({"role": "user", "content": content});
                if *injected {
                    message["injected"] = json!(true);
                }
                message
            }
            Self::Assistant {
                text,
                reasoning,
                tool_calls,
            } => json!({
                "role": "assistant",
                "text": text,
                "reasoning": reasoning,
                "toolCalls": tool_calls
                    .iter()
                    .map(|call| json!({"id": call.id, "name": call.name, "arguments": call.arguments}))
                    .collect::<Vec<_>>(),
            }),
            Self::Tool {
                call_id,
                name,
                content,
            } => json!({"role": "tool", "callId": call_id, "name": name, "content": content}),
        }
    }

    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_owned();
        Some(match value["role"].as_str()? {
            "user" => Self::User {
                content: value["content"].as_array().cloned().unwrap_or_default(),
                injected: value["injected"].as_bool().unwrap_or(false),
            },
            "assistant" => Self::Assistant {
                text: text("text"),
                reasoning: value["reasoning"].as_str().map(ToOwned::to_owned),
                tool_calls: value["toolCalls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|call| ModelToolCall {
                        id: call["id"].as_str().unwrap_or_default().to_owned(),
                        name: call["name"].as_str().unwrap_or_default().to_owned(),
                        arguments: call["arguments"].as_str().unwrap_or_default().to_owned(),
                        presentation: None,
                    })
                    .collect(),
            },
            "tool" => Self::Tool {
                call_id: text("callId"),
                name: text("name"),
                content: text("content"),
            },
            _ => return None,
        })
    }
}

/// The text blocks of public content, joined as the adapter joins them.
fn content_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(block_text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Reference `_block_text`: what one block reads as, when it reads as text.
fn block_text(block: &Value) -> Option<String> {
    match block["type"].as_str() {
        Some("text") => block["text"].as_str().map(ToOwned::to_owned),
        Some("resource_link") => block["uri"]
            .as_str()
            .map(|uri| format!("[resource: {uri}]")),
        Some("resource") => resource_text(&block["resource"]),
        _ => None,
    }
}

/// Reference `FILE_IMAGE_RESOURCE_LINK_META_KEY`: where an image read from
/// a file keeps the link a blind model is handed instead.
pub const FILE_IMAGE_LINK: &str = "mistralai.vibe.harness/file-image-resource-link";

/// `content` as a model call takes it: an image read from a file becomes
/// the link to that file when the model reads no image.
fn delivered_content(content: &[Value], native_images: bool) -> Vec<Value> {
    content
        .iter()
        .map(|block| {
            let link = &block["_meta"][FILE_IMAGE_LINK];
            if native_images || block["type"] != "image" || !link["uri"].is_string() {
                return block.clone();
            }
            json!({"type": "resource_link", "uri": link["uri"], "name": link["name"]})
        })
        .collect()
}

/// The blocks of `content` as the chunks of a message that carries an
/// image, in order; none when it carries none.
fn image_parts(content: &[Value]) -> Vec<vibe_core::events::PublicContentBlock> {
    use vibe_core::events::PublicContentBlock;
    if !content.iter().any(|block| block["type"] == "image") {
        return Vec::new();
    }
    content
        .iter()
        .filter_map(|block| {
            if block["type"] == "image" {
                return Some(PublicContentBlock::Image {
                    attachment: json!({"mimeType": block["mimeType"], "data": block["data"]}),
                });
            }
            block_text(block).map(|text| PublicContentBlock::Text { text })
        })
        .collect()
}

/// Reference `_resource_text` (the harness's Mistral adapter): an embedded
/// text resource as the model reads it, fenced by more backticks than it
/// holds in a row; a link names its URI.
fn resource_text(resource: &Value) -> Option<String> {
    let uri = resource["uri"].as_str()?;
    let Some(text) = resource["text"].as_str() else {
        return (resource["kind"] == "link").then(|| format!("[resource: {uri}]"));
    };
    let mut fence = "````".to_owned();
    while text.contains(&fence) {
        fence.push('`');
    }
    let escaped = uri
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;");
    Some(format!("Resource: {escaped}\n{fence}\n{text}\n{fence}"))
}

/// Everything a session keeps between turns.
#[derive(Debug)]
pub struct SessionCore {
    pub projector: Projector,
    pub conversation: Vec<ConversationMessage>,
    pub vibe: VibeTools,
    /// The last input the core took; every turn start, model answer and tool
    /// result takes one.
    pub inputs: u64,
    /// What the session runs on, as its turns pinned it: `model` and
    /// `reasoningEffort`.
    pub pins: BTreeMap<String, String>,
    /// The callbacks the session waits on, which are published beside its
    /// state and never journaled.
    pub callbacks: CallbackBook,
    /// The background processes the session's programs started.
    pub processes: Arc<crate::processes::ProcessManager>,
    /// The subagents the session's programs started.
    pub subagents: Arc<crate::subagents::SubagentManager>,
    /// The prompts the session scheduled for itself.
    pub loops: Arc<crate::loops::ScheduledLoops>,
    /// Steering accepted for the running turn that no model call carried
    /// yet: each message's content and client message ID.
    pub pending_steer: Vec<(Vec<Value>, Option<String>)>,
    /// Reference `PluginLockV1` left empty: a session imported from a legacy
    /// transcript binds no plugins, now or on any later resume.
    pub without_plugins: bool,
}

impl SessionCore {
    #[must_use]
    pub fn new(state: HarnessState, scratchpad: std::path::PathBuf) -> Self {
        Self {
            projector: Projector::new(state, 0),
            conversation: Vec::new(),
            vibe: VibeTools::new(scratchpad),
            inputs: 0,
            pins: BTreeMap::new(),
            callbacks: CallbackBook::default(),
            processes: crate::processes::ProcessManager::new(),
            subagents: crate::subagents::SubagentManager::new(),
            loops: Arc::new(crate::loops::ScheduledLoops::new()),
            pending_steer: Vec::new(),
            without_plugins: false,
        }
    }

    /// Reference `UserMessageMode::Steer`: the message joins the history
    /// at once and the model reads it on its next call.
    pub fn steer(&mut self, turn_id: &str, request: &TurnRequest) -> Advance {
        let input = self.next_input();
        let advance = self.projector.apply(
            input,
            &[Observation::TurnSteeringReceived {
                turn_id: turn_id.to_owned(),
                content: request.content.clone(),
                client_message_id: request.client_message_id.clone(),
                user_display_content: request.user_display_content.clone(),
            }],
            Some(&TransitionTurn::Running(turn_id.to_owned())),
            now_millis(),
        );
        self.pending_steer
            .push((request.content.clone(), request.client_message_id.clone()));
        advance
    }

    /// Hands the pending steering to the model, the turn now answering the
    /// last message; nothing when none is pending.
    fn deliver_steering(&mut self, turn_id: &str) -> Option<Advance> {
        if self.pending_steer.is_empty() {
            return None;
        }
        let steering = std::mem::take(&mut self.pending_steer);
        let observations: Vec<Observation> = steering
            .iter()
            .map(|(_, client_message_id)| Observation::TurnSteered {
                turn_id: turn_id.to_owned(),
                client_message_id: client_message_id.clone(),
            })
            .collect();
        let input = self.inputs;
        let advance = self.projector.apply(
            input,
            &observations,
            Some(&TransitionTurn::Running(turn_id.to_owned())),
            now_millis(),
        );
        for (content, client_message_id) in steering {
            self.conversation.push(ConversationMessage::User {
                content: tagged(&content, client_message_id.as_deref()),
                injected: false,
            });
        }
        Some(advance)
    }

    /// Reference `finish_turn`: steering the model never read stays in
    /// its context, as the history already shows it.
    fn keep_pending_steering(&mut self) {
        for (content, client_message_id) in std::mem::take(&mut self.pending_steer) {
            self.conversation.push(ConversationMessage::User {
                content: tagged(&content, client_message_id.as_deref()),
                injected: false,
            });
        }
    }

    /// Reference `_drain_runtime_notifications`: what ended in the
    /// background since the model last read reaches it as one user message.
    fn deliver_notifications(&mut self) {
        let mut notifications: Vec<String> = self
            .processes
            .take_notifications()
            .iter()
            .map(|notification| format!("Runtime notification:\n{}", compact(notification)))
            .collect();
        notifications.extend(self.subagents.take_notifications());
        if notifications.is_empty() {
            return;
        }
        let text = notifications.join("\n\n");
        self.conversation.push(ConversationMessage::User {
            content: vec![json!({"type": "text", "text": text})],
            injected: true,
        });
    }

    /// Takes the process list into the public state.
    pub fn refresh_processes(&mut self) {
        self.projector.state.background_processes = self.processes.public();
    }

    /// The session's state as published now, with `event` before it.
    #[must_use]
    pub fn publication(&self, advance: Advance, event: Option<CallbackEvent>) -> Publication {
        Publication {
            state: self.projector.state.clone(),
            advance,
            callbacks: self.callbacks.open.clone(),
            event,
        }
    }

    /// The input entry of the turn `turn_id`, when it is the latest one.
    fn input_entry_of(&self, turn_id: &str) -> Option<String> {
        self.projector
            .state
            .latest_turn
            .as_ref()
            .filter(|turn| turn.id == turn_id)
            .and_then(|turn| turn.input_entry_id.clone())
    }

    fn next_input(&mut self) -> u64 {
        self.inputs += 1;
        self.inputs
    }

    /// Commits a new turn: its user entry, and the session running it.
    pub fn begin_turn(&mut self, request: &TurnRequest) -> (String, HarnessState) {
        self.begin_turn_as(request, format!("turn-{}", random_hex(16)))
    }

    /// Reference `_start_queued_turn`: a queued turn's context entries reach
    /// the model ahead of its user entry, and the turn names the item.
    pub fn begin_queued_turn(
        &mut self,
        request: &TurnRequest,
        queue_item_id: &str,
        context: &[Vec<Value>],
    ) -> (String, HarnessState) {
        for content in context {
            self.conversation.push(ConversationMessage::User {
                content: content.clone(),
                injected: true,
            });
        }
        let turn_id = format!("turn-{}", random_hex(16));
        let input = self.next_input();
        self.projector.apply(
            input,
            &[Observation::TurnStarted {
                turn_id: turn_id.clone(),
                content: request.content.clone(),
                injected: false,
                queue_item_id: Some(queue_item_id.to_owned()),
                client_message_id: request.client_message_id.clone(),
                user_display_content: request.user_display_content.clone(),
            }],
            Some(&TransitionTurn::Running(turn_id.clone())),
            now_millis(),
        );
        self.conversation.push(ConversationMessage::User {
            content: tagged(&request.content, request.client_message_id.as_deref()),
            injected: false,
        });
        (turn_id, self.projector.state.clone())
    }

    /// [`Self::begin_turn`] under the turn ID `turn_id`.
    pub fn begin_turn_as(
        &mut self,
        request: &TurnRequest,
        turn_id: String,
    ) -> (String, HarnessState) {
        let input = self.next_input();
        self.projector.apply(
            input,
            &[Observation::TurnStarted {
                turn_id: turn_id.clone(),
                content: request.content.clone(),
                injected: request.injected,
                queue_item_id: None,
                client_message_id: request.client_message_id.clone(),
                user_display_content: request.user_display_content.clone(),
            }],
            Some(&TransitionTurn::Running(turn_id.clone())),
            now_millis(),
        );
        self.conversation.push(ConversationMessage::User {
            content: tagged(&request.content, request.client_message_id.as_deref()),
            injected: false,
        });
        (turn_id, self.projector.state.clone())
    }
}

/// Reference `_rust_content_blocks`: a message a client named carries its
/// identifier on each text block, which is how the harness finds the message
/// a rewind or a fork names.
fn tagged(content: &[Value], client_message_id: Option<&str>) -> Vec<Value> {
    let Some(id) = client_message_id else {
        return content.to_vec();
    };
    content
        .iter()
        .map(|block| {
            let mut block = block.clone();
            if block["type"] == "text"
                && let Some(object) = block.as_object_mut()
            {
                object.insert("_meta".to_owned(), json!({CLIENT_MESSAGE_KEY: id}));
            }
            block
        })
        .collect()
}

/// Where a client's message identifier rides a text block.
const CLIENT_MESSAGE_KEY: &str = "vibe_client_message_id";

/// The conversation position of the user message a client named `entry_id`.
fn client_message_position(conversation: &[ConversationMessage], entry_id: &str) -> Option<usize> {
    conversation.iter().position(|message| match message {
        ConversationMessage::User { content, .. } => content
            .iter()
            .any(|block| block["_meta"][CLIENT_MESSAGE_KEY] == entry_id),
        _ => false,
    })
}

impl SessionCore {
    /// Reference `rewind` on the harness host: the conversation and the
    /// history before the user message `entry_id`, closed by a `rewind`
    /// checkpoint. Only a message a client named can be found.
    ///
    /// # Errors
    ///
    /// No user message carries that identifier.
    pub fn rewind(&mut self, entry_id: &str, now: u64) -> Result<Advance, String> {
        let position = client_message_position(&self.conversation, entry_id)
            .ok_or_else(|| format!("Cannot find user entry: {entry_id}"))?;
        self.conversation.truncate(position);
        let session_id = self.projector.state.session.id.clone();
        let entries = &mut self.projector.state.entries;
        if let Some(index) = entries.iter().position(|entry| entry["id"] == entry_id) {
            entries.truncate(index);
        }
        entries.push(json!({
            "type": "checkpoint",
            "id": format!("checkpoint-rewind-{entry_id}"),
            "sessionId": session_id,
            "turnId": null,
            "createdAt": now,
            "updatedAt": now,
            "generationStatus": "completed",
            "relatedEntryId": null,
            "inputEntryId": null,
            "kind": "rewind",
            "message": "Conversation rewound",
            "details": {"entryId": entry_id, "inplace": true, "restoreFiles": false},
        }));
        self.projector.state.latest_turn = None;
        self.vibe.todos.clear();
        self.next_input();
        Ok(self.projector.touch(now))
    }
}

/// Reference `fork` on the harness host: a new session whose conversation
/// is `source`'s, imported rather than recorded, so its public history
/// numbers each message by position (`_public_state`) and holds no turns.
#[must_use]
pub fn fork_core(
    source: &SessionCore,
    session_id: &str,
    now: u64,
    scratchpad: std::path::PathBuf,
) -> SessionCore {
    fork_core_until(
        source,
        session_id,
        now,
        scratchpad,
        source.conversation.len(),
    )
}

/// Reference `fork` with an anchor left out: the conversation before the
/// user message a client named `entry_id`.
///
/// # Errors
///
/// No user message carries that identifier.
pub fn fork_core_before(
    source: &SessionCore,
    session_id: &str,
    now: u64,
    scratchpad: std::path::PathBuf,
    entry_id: &str,
) -> Result<SessionCore, String> {
    let end = client_message_position(&source.conversation, entry_id)
        .ok_or_else(|| format!("Cannot find user entry: {entry_id}"))?;
    Ok(fork_core_until(source, session_id, now, scratchpad, end))
}

/// Reference `fork` with the anchor kept (`include_entry=True`): the
/// conversation through the turn the user message `entry_id` opened, up to
/// the next user message.
///
/// # Errors
///
/// No user message carries that identifier.
pub fn fork_core_through(
    source: &SessionCore,
    session_id: &str,
    now: u64,
    scratchpad: std::path::PathBuf,
    entry_id: &str,
) -> Result<SessionCore, String> {
    let anchor = client_message_position(&source.conversation, entry_id)
        .ok_or_else(|| format!("Cannot find user entry: {entry_id}"))?;
    let end = source.conversation[anchor + 1..]
        .iter()
        .position(|message| matches!(message, ConversationMessage::User { .. }))
        .map_or(source.conversation.len(), |offset| anchor + 1 + offset);
    Ok(fork_core_until(source, session_id, now, scratchpad, end))
}

fn fork_core_until(
    source: &SessionCore,
    session_id: &str,
    now: u64,
    scratchpad: std::path::PathBuf,
    end: usize,
) -> SessionCore {
    let conversation = &source.conversation[..end];
    let source_state = &source.projector.state.session;
    let mut state = HarnessState::new(
        session_id,
        Some(source_state.id.as_str()),
        &source_state.root_session_id,
        now,
    );
    state.entries = imported_entries(conversation, session_id, now);
    state.session.preview = crate::state::first_user_message_preview(&state.entries);
    let mut core = SessionCore::new(state, scratchpad);
    core.conversation = conversation.to_vec();
    core.pins = source.pins.clone();
    core.without_plugins = source.without_plugins;
    // Reference `copy_scheduled_loops`: a fork keeps its source's schedule.
    let _ = core.loops.replace(source.loops.list());
    core
}

/// Reference `import_legacy` on the harness host: a new root session whose
/// conversation is a legacy transcript, imported as a fork's is.
#[must_use]
pub fn import_core(
    session_id: &str,
    now: u64,
    scratchpad: std::path::PathBuf,
    conversation: Vec<ConversationMessage>,
    model: Option<String>,
) -> SessionCore {
    let mut state = HarnessState::new(session_id, None, session_id, now);
    state.entries = imported_entries(&conversation, session_id, now);
    state.session.preview = crate::state::first_user_message_preview(&state.entries);
    let mut core = SessionCore::new(state, scratchpad);
    core.conversation = conversation;
    if let Some(model) = model {
        core.pins.insert("model".to_owned(), model);
    }
    core.without_plugins = true;
    core
}

/// The public entries of an imported conversation: one per user or
/// assistant message with content, tool messages and calls left out.
fn imported_entries(
    conversation: &[ConversationMessage],
    session_id: &str,
    now: u64,
) -> Vec<Value> {
    conversation
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            let (role, content) = match message {
                ConversationMessage::User { content, .. } => ("user", untagged(content)),
                ConversationMessage::Assistant { text, .. } if !text.is_empty() => {
                    ("assistant", vec![json!({"type": "text", "text": text})])
                }
                _ => return None,
            };
            if content.is_empty() {
                return None;
            }
            let mut entry = json!({
                "type": "message",
                "id": format!("imported-{}", index + 1),
                "sessionId": session_id,
                "turnId": null,
                "createdAt": now,
                "updatedAt": now,
                "generationStatus": "completed",
                "relatedEntryId": null,
                "inputEntryId": null,
                "role": role,
                "content": content,
                "source": "harness",
            });
            if role == "assistant" {
                entry["outcome"] = json!({"type": "committed"});
            }
            Some(entry)
        })
        .collect()
}

/// `content` without the identifiers [`tagged`] put on it.
fn untagged(content: &[Value]) -> Vec<Value> {
    content
        .iter()
        .map(|block| {
            let mut block = block.clone();
            if let Some(object) = block.as_object_mut() {
                object.remove("_meta");
            }
            block
        })
        .collect()
}

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

/// Runs the turn `turn_id` to its end. `interrupt` turns to the reason the
/// turn is interrupted for.
pub async fn run_turn(
    core: Arc<Mutex<SessionCore>>,
    turn_id: String,
    environment: TurnEnvironment,
    sink: Sink,
    mut interrupt: watch::Receiver<Option<String>>,
) -> TurnEnd {
    let publish = |core: &SessionCore, advance: Advance| {
        sink(core.publication(advance, None));
    };
    let mut first_call = true;
    let mut iterations: u32 = 0;
    loop {
        let action_id = uuid_like();
        let prepared = {
            let Ok(mut core) = core.lock() else {
                return TurnEnd::Interrupted;
            };
            core.deliver_notifications();
            let message_id = core
                .projector
                .state
                .latest_turn
                .as_ref()
                .and_then(|turn| turn.input_entry_id.clone())
                .unwrap_or_default();
            match unsupported_image(&core, &environment) {
                Some(error) => Err(Some(error)),
                None if needs_compaction(&core, &environment) => Err(None),
                None => Ok((
                    provider_input(&core, &environment, first_call, &message_id),
                    message_id,
                )),
            }
        };
        let (input, message_id) = match prepared {
            Ok(prepared) => prepared,
            Err(Some(error)) => return fail_turn(&core, &turn_id, &sink, error),
            Err(None) => {
                let compacted = tokio::select! {
                    end = run_compaction(&core, &environment, &sink, "", Some(&turn_id)) => end,
                    reason = interrupted(&mut interrupt) => return interrupt_turn(&core, &turn_id, &sink, &reason),
                };
                match compacted {
                    CompactionEnd::Compacted { .. } => continue,
                    CompactionEnd::Failed { code, message } => {
                        return TurnEnd::Failed(PublicError {
                            code,
                            message,
                            details: None,
                        });
                    }
                }
            }
        };
        let _ = message_id;
        first_call = false;
        iterations = iterations.saturating_add(1);
        let streamed = tokio::select! {
            streamed = stream_completion(&core, &environment, &input, &turn_id, &action_id, &sink) => streamed,
            reason = interrupted(&mut interrupt) => return interrupt_turn(&core, &turn_id, &sink, &reason),
        };
        let completion = match streamed {
            Ok(completion) => completion,
            Err(error) => {
                return fail_turn(&core, &turn_id, &sink, turn_error(&error, &environment));
            }
        };
        let calls = prepare_calls(&completion.tool_calls, &environment);
        let mut pending = Vec::new();
        let mut searched: BTreeMap<String, String> = BTreeMap::new();
        {
            let Ok(mut core) = core.lock() else {
                return TurnEnd::Interrupted;
            };
            let input = core.next_input();
            // Reference `programmatic_tool_calling`: an answer that runs a
            // program commits nothing of its own until the program ends.
            let runs_program = calls
                .iter()
                .any(|call| matches!(call.own, Some(OwnCall::Program(_))));
            let mut observations = vec![Observation::AssistantMessageCommitted {
                turn_id: turn_id.clone(),
                action_id: action_id.clone(),
                reasoning: completion
                    .reasoning
                    .iter()
                    .map(|text| ReasoningPart {
                        text: vec![text.clone()],
                        summary: Vec::new(),
                        redacted: false,
                    })
                    .collect(),
                content: if completion.text.is_empty() {
                    Vec::new()
                } else {
                    vec![json!({"type": "text", "text": completion.text})]
                },
                usage: completion.usage,
            }];
            if runs_program {
                observations.clear();
            }
            for call in &calls {
                if let Some(OwnCall::Search(request)) = &call.own {
                    let answer = crate::discovery::search(&environment.functions, request.clone());
                    observations.push(Observation::ToolDiscoveryFinished {
                        turn_id: turn_id.clone(),
                        call_id: call.call_id.clone(),
                        mode: answer.mode.as_str().to_owned(),
                        tool_count: answer.tool_count as u64,
                        connector_notice_count: answer.connector_notice_count as u64,
                    });
                    searched.insert(call.call_id.clone(), answer.content);
                }
            }
            let done = calls.is_empty() && core.pending_steer.is_empty();
            if done {
                observations.push(Observation::TurnCompleted {
                    turn_id: turn_id.clone(),
                });
            }
            let transition = if done {
                TransitionTurn::Settled
            } else {
                TransitionTurn::Running(turn_id.clone())
            };
            let advance =
                core.projector
                    .apply(input, &observations, Some(&transition), now_millis());
            core.conversation.push(ConversationMessage::Assistant {
                text: completion.text.clone(),
                reasoning: completion.reasoning.clone(),
                tool_calls: calls
                    .iter()
                    .map(|call| ModelToolCall {
                        id: call.call_id.clone(),
                        name: call.model_name.clone(),
                        arguments: call.wire_arguments.clone(),
                        presentation: None,
                    })
                    .collect(),
            });
            publish(&core, advance);
            if done {
                return TurnEnd::Completed;
            }
            for call in &calls {
                if let Some(harness) = &call.harness {
                    let action = ToolAction {
                        action_id: uuid_like(),
                        turn_id: turn_id.clone(),
                        tool_name: harness.name.to_owned(),
                        arguments: harness.arguments.clone(),
                    };
                    let advance = core.projector.apply_action_started(&action, now_millis());
                    publish(&core, advance);
                    pending.push((call.clone(), Some(action)));
                } else if matches!(call.own, Some(OwnCall::Program(_))) {
                    pending.push((call.clone(), None));
                }
            }
        }
        // Every call answers the model, in the order it asked; a result is
        // committed as soon as its call finishes.
        let mut answers: BTreeMap<String, String> = calls
            .iter()
            .filter_map(|call| {
                call.invalid
                    .as_ref()
                    .map(|message| (call.call_id.clone(), invalid_call_content(message)))
            })
            .collect();
        answers.append(&mut searched);
        let mut running: FuturesUnordered<_> = pending
            .into_iter()
            .map(|(call, action)| {
                let core = Arc::clone(&core);
                let environment = environment.clone();
                let sink = Arc::clone(&sink);
                let turn_id_for_programs = turn_id.clone();
                async move {
                    let outcome = match (&action, &call.own) {
                        (Some(action), _) => {
                            gated_execute(&core, &environment, &sink, &call.call_id, action).await
                        }
                        (None, Some(OwnCall::Program(code))) => {
                            run_program(
                                &core,
                                &environment,
                                &sink,
                                &turn_id_for_programs,
                                &call.call_id,
                                code,
                            )
                            .await
                        }
                        (None, _) => {
                            CallOutcome::Done(ToolResult::failed("tool_failed", "nothing to run"))
                        }
                    };
                    (call, action, outcome)
                }
            })
            .collect();
        loop {
            let next = tokio::select! {
                next = running.next() => next,
                reason = interrupted(&mut interrupt) => return interrupt_turn(&core, &turn_id, &sink, &reason),
            };
            let Some((call, action, outcome)) = next else {
                break;
            };
            let result = match outcome {
                CallOutcome::Done(result) => result,
                CallOutcome::CancelTurn => {
                    return interrupt_turn(&core, &turn_id, &sink, "approval cancel_turn");
                }
                CallOutcome::FailTurn(message) => {
                    return fail_turn(
                        &core,
                        &turn_id,
                        &sink,
                        PublicError {
                            code: "callback_delivery_failed".to_owned(),
                            message,
                            details: None,
                        },
                    );
                }
            };
            answers.insert(call.call_id.clone(), model_content(&result));
            let Some(action) = action else {
                continue;
            };
            let Ok(mut core) = core.lock() else {
                return TurnEnd::Interrupted;
            };
            let input = core.next_input();
            let advance = core.projector.apply(
                input,
                &[Observation::ToolResultCommitted {
                    turn_id: turn_id.clone(),
                    action_id: action.action_id.clone(),
                    result,
                }],
                Some(&TransitionTurn::Running(turn_id.clone())),
                now_millis(),
            );
            publish(&core, advance);
        }
        let Ok(mut core) = core.lock() else {
            return TurnEnd::Interrupted;
        };
        for call in &calls {
            core.conversation.push(ConversationMessage::Tool {
                call_id: call.call_id.clone(),
                name: call.model_name.clone(),
                content: answers.remove(&call.call_id).unwrap_or_default(),
            });
        }
        if let Some(advance) = core.deliver_steering(&turn_id) {
            publish(&core, advance);
        }
        if environment
            .max_iterations
            .is_some_and(|max| iterations >= max)
        {
            let input = core.next_input();
            let advance = core.projector.apply(
                input,
                &[Observation::TurnCompleted {
                    turn_id: turn_id.clone(),
                }],
                Some(&TransitionTurn::Settled),
                now_millis(),
            );
            publish(&core, advance);
            return TurnEnd::Completed;
        }
    }
}

async fn interrupted(interrupt: &mut watch::Receiver<Option<String>>) -> String {
    loop {
        if let Some(reason) = interrupt.borrow().as_ref() {
            return reason.clone();
        }
        if interrupt.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Reference `_cancel_active_work` then `interrupt`: the open callbacks are
/// cancelled, then the turn is interrupted for `reason`.
fn interrupt_turn(
    core: &Arc<Mutex<SessionCore>>,
    turn_id: &str,
    sink: &Sink,
    reason: &str,
) -> TurnEnd {
    let Ok(mut core) = core.lock() else {
        return TurnEnd::Interrupted;
    };
    close_callbacks(&mut core, sink, "Turn interrupted");
    core.keep_pending_steering();
    let input = core.next_input();
    let advance = core.projector.apply(
        input,
        &[Observation::TurnInterrupted {
            turn_id: turn_id.to_owned(),
            reason: Some(reason.to_owned()),
        }],
        Some(&TransitionTurn::Settled),
        now_millis(),
    );
    sink(core.publication(advance, None));
    TurnEnd::Interrupted
}

/// Cancels every open callback, publishing each one settled.
fn close_callbacks(core: &mut SessionCore, sink: &Sink, reason: &str) {
    for entry in core.callbacks.close_all(reason, now_millis()) {
        sink(core.publication(Advance::Provisional, Some(CallbackEvent::Resolved(entry))));
    }
}

fn fail_turn(
    core: &Arc<Mutex<SessionCore>>,
    turn_id: &str,
    sink: &Sink,
    error: PublicError,
) -> TurnEnd {
    let Ok(mut core) = core.lock() else {
        return TurnEnd::Interrupted;
    };
    close_callbacks(&mut core, sink, "Turn failed");
    core.keep_pending_steering();
    let input = core.next_input();
    let advance = core.projector.apply(
        input,
        &[Observation::TurnFailed {
            turn_id: turn_id.to_owned(),
            error: error.clone(),
        }],
        Some(&TransitionTurn::Failed(error.message.clone())),
        now_millis(),
    );
    sink(core.publication(advance, None));
    TurnEnd::Failed(error)
}

/// Whether the session's model reads images (reference
/// `LocalModelRoute.supports_images`).
fn native_images(environment: &TurnEnvironment) -> bool {
    environment
        .model
        .as_ref()
        .is_none_or(|model| model.supports_images)
}

/// Reference `_validate_image_support`: the refusal a call to a model that
/// reads no image makes while the conversation still holds one.
fn unsupported_image(core: &SessionCore, environment: &TurnEnvironment) -> Option<PublicError> {
    if native_images(environment)
        || !core
            .conversation
            .iter()
            .any(|message| message.holds_image(false))
    {
        return None;
    }
    let model = environment
        .model
        .as_ref()
        .map(|model| model.name.clone())
        .unwrap_or_default();
    Some(PublicError {
        message: format!(
            "Model {model} does not support images, but the completion input still contains an image"
        ),
        code: "images_not_supported".to_owned(),
        details: Some(json!({"model": model})),
    })
}

/// The request one model call makes.
fn provider_input(
    core: &SessionCore,
    environment: &TurnEnvironment,
    first_call: bool,
    message_id: &str,
) -> ProviderInput {
    let mut messages = vec![ModelMessage::System {
        content: environment.system_prompt.clone(),
    }];
    messages.extend(
        core.conversation
            .iter()
            .map(|message| message.to_model(native_images(environment))),
    );
    let mut metadata = environment.metadata.clone();
    metadata.insert(
        "call_type".to_owned(),
        if first_call {
            "main_call"
        } else {
            "secondary_call"
        }
        .to_owned(),
    );
    metadata.insert(
        "message_id".to_owned(),
        environment
            .message_id
            .clone()
            .unwrap_or_else(|| message_id.to_owned()),
    );
    ProviderInput {
        turn_id: None,
        session_id: Some(core.projector.state.session.id.clone()),
        model_override: environment.model.as_ref().map(|model| model.alias.clone()),
        model: environment.model.clone(),
        messages,
        stream: true,
        images: Vec::new(),
        tools: environment
            .tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name.to_owned(),
                description: tool.description.clone(),
                input_schema: tool.parameters.clone(),
            })
            .collect(),
        tool_choice: None,
        thinking: false,
        reasoning_effort: None,
        headers: BTreeMap::new(),
        limits: Default::default(),
        metadata,
        dialect: RequestDialect::Unified,
    }
}

/// What one model call answered.
#[derive(Debug, Clone, Default)]
struct Completion {
    text: String,
    reasoning: Option<String>,
    tool_calls: Vec<(String, String, String)>,
    usage: Option<TokenUsage>,
}

async fn stream_completion(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    input: &ProviderInput,
    turn_id: &str,
    action_id: &str,
    sink: &Sink,
) -> Result<Completion, ProviderError> {
    let retries = TurnRetries {
        core,
        sink,
        turn_id,
        attempts: std::sync::atomic::AtomicU32::new(0),
    };
    let mut stream = environment
        .provider
        .stream_observed(input, &retries)
        .await?;
    retries.recovered();
    let mut completion = Completion::default();
    let mut reasoning = String::new();
    let mut pending = CompletionDelta {
        action_id: action_id.to_owned(),
        turn_id: turn_id.to_owned(),
        ..CompletionDelta::default()
    };
    let mut flushed_at: Option<Instant> = None;
    let flush = |pending: &mut CompletionDelta, flushed_at: &mut Option<Instant>| {
        if pending.text.is_empty() && pending.reasoning.is_empty() {
            return;
        }
        if let Ok(mut core) = core.lock() {
            let advance = core.projector.apply_completion_delta(pending, now_millis());
            sink(core.publication(advance, None));
        }
        pending.text.clear();
        pending.reasoning.clear();
        *flushed_at = Some(Instant::now());
    };
    loop {
        let deadline = flushed_at
            .filter(|_| !pending.text.is_empty() || !pending.reasoning.is_empty())
            .map(|at| at + DELTA_INTERVAL);
        let chunk = match deadline {
            Some(deadline) => tokio::select! {
                chunk = stream.chunks.next() => chunk,
                () = tokio::time::sleep_until(deadline) => {
                    flush(&mut pending, &mut flushed_at);
                    continue;
                }
            },
            None => stream.chunks.next().await,
        };
        let Some(chunk) = chunk else {
            break;
        };
        match chunk? {
            ProviderChunk::Text { text } => {
                completion.text.push_str(&text);
                pending.text.push_str(&text);
            }
            ProviderChunk::Reasoning { text } => {
                reasoning.push_str(&text);
                pending.reasoning.push_str(&text);
            }
            ProviderChunk::ToolCall {
                id,
                name,
                arguments,
            } => completion.tool_calls.push((id, name, arguments)),
            ProviderChunk::Usage {
                input_tokens,
                output_tokens,
                cached_tokens,
            } => {
                completion.usage = Some(TokenUsage {
                    input_tokens,
                    output_tokens,
                    total_tokens: input_tokens + output_tokens,
                    cached_input_tokens: cached_tokens,
                });
            }
            ProviderChunk::ReasoningPayload { .. }
            | ProviderChunk::Refusal { .. }
            | ProviderChunk::Stop { .. } => {}
        }
        let due = flushed_at.is_none_or(|at| at.elapsed() >= DELTA_INTERVAL);
        if due {
            flush(&mut pending, &mut flushed_at);
        }
    }
    // What arrived since the last publication is published before the answer
    // commits.
    flush(&mut pending, &mut flushed_at);
    if !reasoning.is_empty() {
        completion.reasoning = Some(reasoning);
    }
    // Reference `_read_stream`: an answer with nothing in it is one space.
    if completion.text.is_empty()
        && completion.reasoning.is_none()
        && completion.tool_calls.is_empty()
    {
        completion.text = " ".to_owned();
    }
    Ok(completion)
}

/// What a compaction between turns ended with.
#[derive(Debug, Clone, PartialEq)]
pub enum CompactionEnd {
    /// The conversation was replaced; the summary that replaced it.
    Compacted { summary: String },
    /// The compaction failed and the conversation is unchanged.
    Failed { code: String, message: String },
}

/// Reference `start_compaction` then `finish_compaction` for a compaction a
/// client asked for between turns.
pub async fn compact_session(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    sink: &Sink,
    extra_instructions: &str,
) -> CompactionEnd {
    run_compaction(core, environment, sink, extra_instructions, None).await
}

/// One compaction: a manual one between turns, or an automatic one inside
/// `turn_id` once the context outgrew the model's threshold. A refused
/// summary is asked for again until the attempts run out.
async fn run_compaction(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    sink: &Sink,
    extra_instructions: &str,
    turn_id: Option<&str>,
) -> CompactionEnd {
    let trigger = if turn_id.is_some() {
        "automatic"
    } else {
        "manual"
    };
    let compaction_id = uuid_like();
    let mut attempt = 1;
    let mut retried = false;
    loop {
        let input = {
            let Ok(mut core) = core.lock() else {
                return CompactionEnd::Failed {
                    code: "internal_error".to_owned(),
                    message: "the session lock is poisoned".to_owned(),
                };
            };
            let now = now_millis();
            let input_entry_id = turn_id.and_then(|_| {
                core.projector
                    .state
                    .latest_turn
                    .as_ref()
                    .and_then(|turn| turn.input_entry_id.clone())
            });
            let entry = crate::state::running_compaction_entry(
                &core.projector.state.session.id.clone(),
                &core.projector.state.entries,
                &compaction_id,
                turn_id,
                trigger,
                attempt,
                now,
                input_entry_id.as_deref(),
            );
            if !retried {
                let advance = core.projector.upsert_provisional(entry, now);
                sink(core.publication(advance, None));
            }
            let message_id = core
                .projector
                .state
                .latest_turn
                .as_ref()
                .and_then(|turn| turn.input_entry_id.clone())
                .unwrap_or_default();
            let mut input = provider_input(&core, environment, false, &message_id);
            input.stream = false;
            input.messages.push(
                ConversationMessage::User {
                    content: vec![json!({"type": "text", "text": crate::compaction::prompt(extra_instructions)})],
                    injected: true,
                }
                .to_model(true),
            );
            input
        };
        let answered = environment.provider.complete(&input).await;
        let Ok(mut core) = core.lock() else {
            return CompactionEnd::Failed {
                code: "internal_error".to_owned(),
                message: "the session lock is poisoned".to_owned(),
            };
        };
        let failed = |error: PublicError, reason: Option<String>| {
            (
                Observation::ContextCompactionFailed {
                    turn_id: turn_id.map(ToOwned::to_owned),
                    compaction_id: compaction_id.clone(),
                    trigger: trigger.to_owned(),
                    attempt,
                    code: error.code.clone(),
                    reason,
                },
                CompactionEnd::Failed {
                    code: error.code.clone(),
                    message: error.message.clone(),
                },
                Some(error),
            )
        };
        let (observation, end, error) = match answered {
            // Reference `complete` (the harness's adapters): an answer with
            // no text is a failed call, not a summary to refuse.
            Ok(message) if message.text.is_empty() => failed(
                compaction_error(
                    environment,
                    "Completion response has no text content",
                    "ValueError",
                ),
                None,
            ),
            Ok(message) => {
                match crate::compaction::extract_summary(
                    &message.text,
                    !message.tool_calls.is_empty(),
                ) {
                    Ok(summary) => {
                        let replaced = crate::compaction::replacement(&core.conversation, &summary);
                        core.conversation = replaced;
                        (
                            Observation::ContextCompacted {
                                turn_id: turn_id.map(ToOwned::to_owned),
                                compaction_id: compaction_id.clone(),
                                trigger: trigger.to_owned(),
                                attempt,
                                summary_len: summary.chars().count(),
                                usage: Some(TokenUsage {
                                    input_tokens: message.usage.input_tokens,
                                    output_tokens: message.usage.output_tokens,
                                    total_tokens: message.usage.input_tokens
                                        + message.usage.output_tokens,
                                    cached_input_tokens: message.usage.cached_tokens,
                                }),
                            },
                            CompactionEnd::Compacted { summary },
                            None,
                        )
                    }
                    Err(_) if attempt < crate::compaction::MAX_ATTEMPTS => {
                        // The retry shows only once the compaction settles:
                        // until then the checkpoint keeps its first attempt.
                        attempt += 1;
                        let advance = core.projector.touch(now_millis());
                        sink(core.publication(advance, None));
                        retried = true;
                        continue;
                    }
                    // Reference `_compaction_failure_reason`: the public
                    // reason a refused summary is recovered as.
                    Err(rejection) => failed(
                        PublicError {
                            code: "invalid_compaction_summary".to_owned(),
                            message: rejection.message().to_owned(),
                            details: None,
                        },
                        Some(
                            if rejection == crate::compaction::Rejection::ToolCalls {
                                "tool_call"
                            } else {
                                "empty_summary"
                            }
                            .to_owned(),
                        ),
                    ),
                }
            }
            Err(error) => {
                let mut error = turn_error(&error, environment);
                if let Some(details) = error.details.as_mut().and_then(Value::as_object_mut) {
                    details.insert("purpose".to_owned(), json!("compaction"));
                }
                failed(error, None)
            }
        };
        let input_id = core.next_input();
        let mut observations = vec![observation];
        let transition = match (turn_id, error) {
            (Some(turn_id), Some(error)) => {
                close_callbacks(&mut core, sink, "Turn failed");
                core.keep_pending_steering();
                let message = error.message.clone();
                observations.push(Observation::TurnFailed {
                    turn_id: turn_id.to_owned(),
                    error,
                });
                Some(TransitionTurn::Failed(message))
            }
            (Some(turn_id), None) => Some(TransitionTurn::Running(turn_id.to_owned())),
            (None, _) => None,
        };
        let advance =
            core.projector
                .apply(input_id, &observations, transition.as_ref(), now_millis());
        sink(core.publication(advance, None));
        return end;
    }
}

/// A compaction call's failure as the turn it ran in reports it.
fn compaction_error(environment: &TurnEnvironment, message: &str, exception: &str) -> PublicError {
    let model = environment.model.as_ref();
    PublicError {
        code: "model_stream_failed".to_owned(),
        message: message.to_owned(),
        details: Some(json!({
            "provider": model.map_or("mistral", |model| model.provider.as_str()),
            "model": model.map(|model| model.name.clone()).unwrap_or_default(),
            "purpose": "compaction",
            "exceptionType": exception,
        })),
    }
}

/// Reference automatic compaction: whether the context the last call
/// reported has reached the model's threshold.
fn needs_compaction(core: &SessionCore, environment: &TurnEnvironment) -> bool {
    environment.compact_threshold > 0
        && core
            .projector
            .state
            .session
            .context_usage
            .is_some_and(|usage| usage.total_tokens >= environment.compact_threshold)
}

/// Publishes the provider retries a model call waits on (reference
/// `_set_provider_retry`): each one shows in the state until an attempt is
/// answered or the turn ends.
struct TurnRetries<'a> {
    core: &'a Arc<Mutex<SessionCore>>,
    sink: &'a Sink,
    turn_id: &'a str,
    attempts: std::sync::atomic::AtomicU32,
}

impl TurnRetries<'_> {
    fn publish(&self, retrying: Option<Value>) {
        let Ok(mut core) = self.core.lock() else {
            return;
        };
        if core.projector.state.retrying == retrying {
            return;
        }
        core.projector.state.retrying = retrying;
        (self.sink)(core.publication(Advance::Provisional, None));
    }

    /// An attempt was answered, so nothing is retrying any more.
    fn recovered(&self) {
        if self.attempts.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            self.publish(None);
        }
    }
}

impl vibe_core::llm::retry::RetryObserver for TurnRetries<'_> {
    fn retrying(&self, reason: &vibe_core::llm::retry::RetryReason) {
        self.retrying_after(reason, 0.0);
    }

    fn retrying_after(&self, reason: &vibe_core::llm::retry::RetryReason, delay_seconds: f64) {
        let attempt = self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let delay_millis = (delay_seconds * 1000.0).round().max(0.0) as u64;
        self.publish(Some(json!({
            "turnId": self.turn_id,
            "category": reason.category.as_str(),
            "detail": reason.detail,
            "retryAt": now_millis() + delay_millis,
            "retryAttempt": attempt,
        })));
    }
}

/// Reference `execute_completion`'s failure events: what a failed model call
/// reports, which `harness_turn_error_code` later maps for clients.
fn turn_error(error: &ProviderError, environment: &TurnEnvironment) -> PublicError {
    let cause = backend_cause(error);
    let local = local_cause(error);
    let status = status_of(error);
    let mut details = Map::new();
    details.insert(
        "provider".to_owned(),
        json!(cause.map_or_else(
            || {
                environment
                    .model
                    .as_ref()
                    .map_or("mistral", |model| model.provider.as_str())
                    .to_owned()
            },
            |cause| cause.provider.clone(),
        )),
    );
    details.insert(
        "model".to_owned(),
        json!(cause.map_or_else(
            || {
                environment
                    .model
                    .as_ref()
                    .map(|model| model.name.clone())
                    .unwrap_or_default()
            },
            |cause| cause.model.clone(),
        )),
    );
    details.insert("purpose".to_owned(), json!("agent"));
    details.insert(
        "exceptionType".to_owned(),
        json!(exception_type(cause, local)),
    );
    if let Some(status) = status {
        details.insert("httpStatus".to_owned(), json!(status));
    }
    if let Some(correlation) = cause.and_then(|cause| header(cause, "mistral-correlation-id")) {
        details.insert("correlationId".to_owned(), json!(correlation));
    }
    let rejection = match status {
        Some(401) => Some("http_unauthorized"),
        Some(403) => Some("http_forbidden"),
        _ => None,
    };
    if let Some(reason) = rejection {
        // Only a 401 says the credential itself was refused, so only then is
        // where it came from worth naming.
        let origin = cause
            .filter(|_| reason == "http_unauthorized")
            .and_then(|cause| cause.api_key_origin.as_ref());
        details.insert("reason".to_owned(), json!(reason));
        return PublicError {
            code: "model_unauthorized".to_owned(),
            message: invalid_api_key_message(origin),
            details: Some(Value::Object(details)),
        };
    }
    PublicError {
        code: "model_stream_failed".to_owned(),
        message: failure_message(error, cause),
        details: Some(Value::Object(details)),
    }
}

/// Reference `invalid_api_key_message`.
fn invalid_api_key_message(origin: Option<&vibe_core::llm::error::KeyOrigin>) -> String {
    use vibe_core::llm::error::KeySource;
    match origin {
        Some(origin) => {
            let source = match origin.source {
                KeySource::Environment => format!("env var {}", origin.variable),
                KeySource::Keyring => "the keyring".to_owned(),
            };
            format!("Invalid API key (from {source}). Please check your API key and try again.")
        }
        None => "Invalid API key. Please check your API key and try again.".to_owned(),
    }
}

/// The class name the reference's client raises the failure as.
fn exception_type(
    cause: Option<&vibe_core::llm::error::BackendError>,
    local: Option<&vibe_core::llm::error::LocalFailure>,
) -> String {
    use vibe_core::llm::error::BackendErrorSource;
    if let Some(cause) = cause {
        return match &cause.source {
            BackendErrorSource::Client if cause.status == Some(422) => {
                "HTTPValidationError".to_owned()
            }
            BackendErrorSource::Client => "SDKError".to_owned(),
            BackendErrorSource::Status => "HTTPStatusError".to_owned(),
            BackendErrorSource::Stream => "OpenAIResponsesStreamError".to_owned(),
            BackendErrorSource::Request(kind) => kind.name().to_owned(),
        };
    }
    local
        .map_or("Exception", |local| local.kind.reference_class())
        .to_owned()
}

/// Reference `_failure_message`: the client's own rendering, with the
/// provider's explanation appended when it does not already carry it.
fn failure_message(
    error: &ProviderError,
    cause: Option<&vibe_core::llm::error::BackendError>,
) -> String {
    use vibe_core::llm::error::BackendErrorSource;
    let Some(cause) = cause else {
        return local_cause(error).map_or_else(|| error.to_string(), |local| local.message.clone());
    };
    let rendered = match (&cause.source, cause.status) {
        (BackendErrorSource::Client, Some(status)) => sdk_error_message(cause, status),
        (BackendErrorSource::Status, Some(status)) => {
            let kind = match status {
                100..=199 => "Informational response",
                300..=399 => "Redirect response",
                400..=499 => "Client error",
                _ => "Server error",
            };
            format!(
                "{kind} '{status} {}' for url '{}'\nFor more information check: \
                 https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/{status}",
                cause.reason.as_deref().unwrap_or_default(),
                cause.endpoint,
            )
        }
        _ => cause.reason.clone().unwrap_or_else(|| error.to_string()),
    };
    match provider_message(&cause.body_text) {
        Some(detail) if !rendered.contains(&detail) => format!("{rendered}: {detail}"),
        _ => rendered,
    }
}

/// How the Mistral client's `SDKError` renders a refused answer.
fn sdk_error_message(cause: &vibe_core::llm::error::BackendError, status: u16) -> String {
    const MAX_MESSAGE_LEN: usize = 10_000;
    let mut message = format!("API error occurred: Status {status}");
    let content_type = header(cause, "content-type").unwrap_or("\"\"");
    if content_type != "application/json" {
        if content_type.contains(' ') {
            message.push_str(&format!(" Content-Type \"{content_type}\""));
        } else {
            message.push_str(&format!(" Content-Type {content_type}"));
        }
    }
    let body = if cause.body_text.is_empty() {
        "\"\""
    } else {
        cause.body_text.as_str()
    };
    let length = body.chars().count();
    if length > MAX_MESSAGE_LEN {
        let truncated: String = body.chars().take(MAX_MESSAGE_LEN).collect();
        message.push_str(&format!(
            ". Body: {truncated}...and {} more chars",
            length - MAX_MESSAGE_LEN
        ));
    } else {
        message.push_str(&format!(". Body: {body}"));
    }
    message.trim().to_owned()
}

/// Reference `_provider_message`: the explanation a provider put in an error
/// body, the OpenAI-compatible `error.message` first.
fn provider_message(body: &str) -> Option<String> {
    let payload: Value = serde_json::from_str(body).ok()?;
    let payload = payload.as_object()?;
    let error = payload.get("error");
    [
        error.and_then(|error| error.get("message")),
        error.filter(|error| error.is_string()),
        payload.get("message"),
        payload.get("detail"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .map(str::trim)
    .find(|candidate| !candidate.is_empty())
    .map(ToOwned::to_owned)
}

fn header<'a>(cause: &'a vibe_core::llm::error::BackendError, name: &str) -> Option<&'a str> {
    cause
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The backend failure a failed call carries, when it reached the provider.
fn backend_cause(error: &ProviderError) -> Option<&vibe_core::llm::error::BackendError> {
    use vibe_core::llm::error::{CallFailure, WrappedCause};
    let ProviderError::Call(call) = error else {
        return None;
    };
    match &call.failure {
        CallFailure::RateLimit { cause, .. }
        | CallFailure::ContextTooLong { cause, .. }
        | CallFailure::ResponseTooLong { cause, .. } => Some(cause),
        CallFailure::InvalidModel(cause) => Some(cause),
        CallFailure::Wrapped {
            cause: WrappedCause::Backend(cause),
            ..
        } => Some(cause),
        _ => None,
    }
}

fn local_cause(error: &ProviderError) -> Option<&vibe_core::llm::error::LocalFailure> {
    use vibe_core::llm::error::{CallFailure, WrappedCause};
    match error {
        ProviderError::Call(call) => match &call.failure {
            CallFailure::Wrapped {
                cause: WrappedCause::Local(local),
                ..
            } => Some(local),
            _ => None,
        },
        _ => None,
    }
}

/// The HTTP status a failed call carried, when it got that far.
fn status_of(error: &ProviderError) -> Option<u16> {
    match error {
        ProviderError::HttpStatus { status } => Some(*status),
        ProviderError::Call(call)
            if matches!(
                call.failure,
                vibe_core::llm::error::CallFailure::RateLimit { .. }
            ) =>
        {
            backend_cause(error)
                .and_then(|cause| cause.status)
                .or(Some(429))
        }
        _ => backend_cause(error).and_then(|cause| cause.status),
    }
}

/// A call the model asked for, checked against the catalog.
#[derive(Debug, Clone)]
struct PreparedCall {
    call_id: String,
    model_name: String,
    /// The arguments as the next request replays them: what the model sent,
    /// or `{}` when that was not a JSON object.
    wire_arguments: String,
    harness: Option<HarnessCall>,
    own: Option<OwnCall>,
    invalid: Option<String>,
}

#[derive(Debug, Clone)]
struct HarnessCall {
    name: &'static str,
    arguments: Value,
}

/// A call the engine answers itself instead of running a harness tool.
#[derive(Debug, Clone)]
enum OwnCall {
    /// `run_typescript`, with the program.
    Program(String),
    /// `search_tool_functions`.
    Search(crate::discovery::SearchRequest),
}

fn prepare_calls(
    calls: &[(String, String, String)],
    environment: &TurnEnvironment,
) -> Vec<PreparedCall> {
    let tools = &environment.tools;
    let skills: Vec<&str> = environment
        .skills
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, raw))| {
            let call_id = if id.is_empty() {
                format!("tool_call_{index}")
            } else {
                id.clone()
            };
            let raw = if raw.is_empty() { "{}" } else { raw.as_str() };
            let parsed: Result<Value, String> =
                serde_json::from_str(raw).map_err(|error| error.to_string());
            let wire_arguments = match &parsed {
                Ok(Value::Object(_)) => raw.to_owned(),
                _ => "{}".to_owned(),
            };
            let mut prepared = PreparedCall {
                call_id,
                model_name: name.clone(),
                wire_arguments,
                harness: None,
                own: None,
                invalid: None,
            };
            let declared = tools.iter().find(|tool| tool.name == name);
            let check = || -> Result<Result<HarnessCall, OwnCall>, String> {
                let tool =
                    declared.ok_or_else(|| format!("unsupported top-level tool {name:?}"))?;
                let arguments = parsed.clone()?;
                if !arguments.is_object() {
                    return Err("tool arguments must be a JSON object".to_owned());
                }
                // Reference `tool_discovery::dispatch` reads the search
                // arguments itself, before any schema check.
                if name == "search_tool_functions" {
                    return crate::discovery::SearchRequest::parse(&arguments)
                        .map(|request| Err(OwnCall::Search(request)));
                }
                validate(&tool.parameters, &arguments)?;
                if name == "run_typescript" {
                    let code = arguments["code"].as_str().unwrap_or_default().to_owned();
                    return Ok(Err(OwnCall::Program(code)));
                }
                if name == "skill" {
                    crate::skills::check_request(&skills, &arguments)?;
                }
                let harness = catalog::harness_name(name)
                    .ok_or_else(|| format!("unsupported top-level tool {name:?}"))?;
                Ok(Ok(HarnessCall {
                    name: harness,
                    arguments: catalog::harness_arguments(name, arguments),
                }))
            };
            match check() {
                Ok(Ok(harness)) => prepared.harness = Some(harness),
                Ok(Err(own)) => prepared.own = Some(own),
                Err(message) => prepared.invalid = Some(message),
            }
            prepared
        })
        .collect()
}

fn validate(schema: &Value, arguments: &Value) -> Result<(), String> {
    let validator = jsonschema::draft7::options()
        .should_validate_formats(true)
        .build(schema)
        .map_err(|error| format!("invalid Core-owned JSON Schema: {error}"))?;
    validator.validate(arguments).map_err(|error| {
        format!("tool arguments do not satisfy the declared input schema: {error}")
    })
}

fn invalid_call_content(message: &str) -> String {
    compact(&json!({
        "result": {
            "type": "error",
            "error": {"name": "InvalidToolCall", "message": message},
        },
    }))
}

/// What the model reads of a tool result: its text, else its structured
/// content with sorted keys, else the failure.
#[must_use]
pub fn model_content(result: &ToolResult) -> String {
    match result {
        ToolResult::Success {
            content,
            structured_content,
            ..
        } => {
            let texts: Vec<&str> = content
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect();
            if !texts.is_empty() {
                return texts.join("\n\n");
            }
            structured_content
                .as_ref()
                .map_or_else(String::new, |value| compact(&sorted(value)))
        }
        ToolResult::Failure { content, .. }
            if content.iter().any(|block| block["text"].is_string()) =>
        {
            content
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        ToolResult::Failure {
            code,
            message,
            details,
            ..
        } => compact(&json!({
            "result": {
                "type": "error",
                "error": {"name": code, "message": message, "details": details},
            },
        })),
    }
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// A JSON value with every object's keys in sorted order.
#[must_use]
pub fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), sorted(&map[key])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// How one call ended for the turn.
enum CallOutcome {
    Done(ToolResult),
    CancelTurn,
    FailTurn(String),
}

/// Reference `_gate_tool_call` then the tool: the gate decides, the call
/// runs or is refused, and its result carries the approval record.
async fn gated_execute(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    sink: &Sink,
    call_id: &str,
    action: &ToolAction,
) -> CallOutcome {
    // Reference `_process_actions`: process arguments are checked before any
    // approval is asked, and pacing the agent itself asks none.
    let precheck = crate::processes::precheck(
        &action.tool_name,
        &action.arguments,
        &environment.workspace.cwd,
    );
    if let Some(refused) = precheck {
        return CallOutcome::Done(refused);
    }
    if action.tool_name == "self.sleep" || action.tool_name.starts_with("subagent.") {
        return match execute(core, environment, sink, call_id, action).await {
            Ok(result) => CallOutcome::Done(result),
            Err(message) => CallOutcome::FailTurn(message),
        };
    }
    let approver = EngineApprover {
        core,
        sink,
        action,
        call_id,
    };
    let (mut result, approval) = match environment.gate.check(action, &approver).await {
        GateVerdict::Run { approval } => {
            match execute(core, environment, sink, call_id, action).await {
                Ok(result) => (result, approval),
                Err(message) => return CallOutcome::FailTurn(message),
            }
        }
        GateVerdict::Refuse { approval, message } => {
            (ToolResult::failed("tool_denied", message), approval)
        }
        GateVerdict::CancelTurn => return CallOutcome::CancelTurn,
        GateVerdict::FailTurn { message } => return CallOutcome::FailTurn(message),
    };
    let mut approval = approval;
    // Reference `_with_approval_note`: a note on why the call ran rides the
    // result's metadata, apart from the approval record.
    let note = approval
        .as_object_mut()
        .and_then(|record| record.remove("note"));
    let meta = result.meta_mut().get_or_insert_with(Map::new);
    meta.insert("approval".to_owned(), approval);
    if let Some(note) = note.filter(Value::is_string) {
        meta.insert("approval_note".to_owned(), note);
    }
    CallOutcome::Done(result)
}

/// Reference `programmatic_tool_calling`: runs a `run_typescript` program to
/// its end. Each evaluation replays the program against the calls answered so
/// far; the calls it then waits on run as tools of their own, each with its
/// effect entry and its gate, and the next evaluation sees their results.
async fn run_program(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    sink: &Sink,
    turn_id: &str,
    call_id: &str,
    code: &str,
) -> CallOutcome {
    use crate::code_mode::{self, Facts, Recorded, Settled, Stopped};
    let javascript = match code_mode::transpile(code) {
        Ok(javascript) => javascript,
        Err(error) => return CallOutcome::Done(program_failure(&error, "", "")),
    };
    let message_count = core
        .lock()
        .map(|core| core.conversation.len() + 1)
        .unwrap_or_default();
    let mut seed_bytes = [0u8; 4];
    let _ = getrandom::fill(&mut seed_bytes);
    let tools = crate::programmatic::namespaces(&environment.callables);
    let now = now_millis();
    let mut record: Vec<Recorded> = Vec::new();
    let mut barriers = StartBarriers {
        manager: core.lock().ok().map(|core| Arc::clone(&core.processes)),
        session_id: core
            .lock()
            .map(|core| core.projector.state.session.id.clone())
            .unwrap_or_default(),
        started: Vec::new(),
        applied: Vec::new(),
    };
    for round in 0usize.. {
        let facts = Facts {
            tools: tools.clone(),
            record: record.clone(),
            now,
            seed: u32::from_le_bytes(seed_bytes),
        };
        let program = javascript.clone();
        let evaluated =
            tokio::task::spawn_blocking(move || code_mode::evaluate(&program, &facts)).await;
        let evaluation = match evaluated {
            Ok(Ok(evaluation)) => evaluation,
            Ok(Err(Stopped::Memory { operations })) => {
                let error = json!({
                    "name": "MemoryLimitError",
                    "message": code_mode::memory_message(operations),
                    "details": {"memoryLimitMiB": 16, "operationCount": operations},
                });
                return CallOutcome::Done(program_failure(&error, "", ""));
            }
            Ok(Err(Stopped::Deadline)) => {
                return CallOutcome::FailTurn(code_mode::DEADLINE_MESSAGE.to_owned());
            }
            Err(error) => return CallOutcome::FailTurn(error.to_string()),
        };
        let calls = match evaluation.settled {
            Settled::Returned(value) => {
                return CallOutcome::Done(program_success(
                    &value,
                    &evaluation.stdout,
                    &evaluation.stderr,
                ));
            }
            Settled::Threw(error) => {
                return CallOutcome::Done(program_failure(
                    &error,
                    &evaluation.stdout,
                    &evaluation.stderr,
                ));
            }
            Settled::Pending(calls) => calls,
        };
        // Calls whose arguments do not fit are refused without running;
        // the others start in order and run together.
        let mut slots: Vec<Option<Result<Value, Value>>> = Vec::with_capacity(calls.len());
        let mut started = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            let Some(callable) = environment
                .callables
                .iter()
                .find(|callable| callable.qualified() == call.name)
            else {
                slots.push(Some(Err(json!({"name": "Error", "message": format!("Unknown tool function: {}", call.name)}))));
                continue;
            };
            if let Some(error) = crate::programmatic::argument_error(callable, &call.arguments) {
                slots.push(Some(Err(error)));
                continue;
            }
            let action = ToolAction {
                action_id: uuid_like(),
                turn_id: turn_id.to_owned(),
                tool_name: callable.runtime.to_string(),
                arguments: callable.runtime_arguments(call.arguments.clone()),
            };
            {
                let Ok(mut core) = core.lock() else {
                    return CallOutcome::FailTurn("the session is gone".to_owned());
                };
                let advance = core.projector.apply_action_started(&action, now_millis());
                sink(core.publication(advance, None));
            }
            let operation = format!(
                "typescript:{message_count}:{call_id}:{round}:tool:{}",
                record.len() + index
            );
            barriers.started(&action, &operation);
            started.push((slots.len(), operation, action));
            slots.push(None);
        }
        // The calls this round started are published, which is the step the
        // reference's engine takes together with applying the last results.
        barriers.open_applied();
        let mut running: FuturesUnordered<_> = started
            .into_iter()
            .map(|(slot, operation, action)| async move {
                let outcome = gated_execute(core, environment, sink, &operation, &action).await;
                (slot, action, outcome)
            })
            .collect();
        while let Some((slot, action, outcome)) = running.next().await {
            let result = match outcome {
                CallOutcome::Done(result) => result,
                other => return other,
            };
            slots[slot] = Some(crate::programmatic::settled_value(&result));
            let Ok(mut core) = core.lock() else {
                return CallOutcome::FailTurn("the session is gone".to_owned());
            };
            let input = core.next_input();
            let advance = core.projector.apply(
                input,
                &[Observation::ToolResultCommitted {
                    turn_id: turn_id.to_owned(),
                    action_id: action.action_id.clone(),
                    result,
                }],
                Some(&TransitionTurn::Running(turn_id.to_owned())),
                now_millis(),
            );
            sink(core.publication(advance, None));
            barriers.applied(&action);
        }
        drop(running);
        for (call, slot) in calls.into_iter().zip(slots) {
            record.push(Recorded {
                name: call.name,
                arguments: call.arguments,
                outcome: slot.unwrap_or_else(|| {
                    Err(json!({"name": "Error", "message": "the call did not finish"}))
                }),
            });
        }
    }
    CallOutcome::FailTurn("run_typescript never settled".to_owned())
}

/// The background processes a program started, held until it moves past them.
///
/// The reference's engine applies a call's result and starts the program's
/// next calls in one step, and only then opens the barrier that lets the
/// started process's end be published; a process that ends at once therefore
/// shows after both. Here a result is committed and the next calls start in
/// two publications, so a start is acknowledged after the second. A program
/// that ends, fails or is dropped acknowledges every start it made.
struct StartBarriers {
    manager: Option<Arc<crate::processes::ProcessManager>>,
    session_id: String,
    /// Each `process.start` action started, with the process it names.
    started: Vec<(String, String)>,
    applied: Vec<String>,
}

impl StartBarriers {
    /// `operation` is the call identifier the action runs under, which
    /// [`crate::processes::process_id`] reads.
    fn started(&mut self, action: &ToolAction, operation: &str) {
        if action.tool_name == "process.start" {
            let process_id =
                crate::processes::process_id(&self.session_id, &action.action_id, operation);
            self.started.push((action.action_id.clone(), process_id));
        }
    }

    fn applied(&mut self, action: &ToolAction) {
        if let Some((_, process_id)) = self
            .started
            .iter()
            .find(|(action_id, _)| *action_id == action.action_id)
        {
            self.applied.push(process_id.clone());
        }
    }

    fn open_applied(&mut self) {
        if let Some(manager) = &self.manager {
            for process_id in self.applied.drain(..) {
                manager.acknowledge_start(&process_id);
            }
        }
    }
}

impl Drop for StartBarriers {
    fn drop(&mut self) {
        self.open_applied();
        if let Some(manager) = &self.manager {
            for (_, process_id) in self.started.drain(..) {
                manager.acknowledge_start(&process_id);
            }
        }
    }
}

/// The logs a program wrote, as the blocks after its result.
fn program_logs(stdout: &str, stderr: &str) -> Vec<Value> {
    [("stdout", stdout), ("stderr", stderr)]
        .into_iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(stream, text)| json!({"type": "text", "text": format!("{stream}:\n{text}")}))
        .collect()
}

/// What a program that returned `value` (compact JSON) answers: the value,
/// or the content blocks it returned, then its logs.
fn program_success(value: &str, stdout: &str, stderr: &str) -> ToolResult {
    let parsed: Value = serde_json::from_str(value).unwrap_or(Value::Null);
    let blocks = parsed
        .as_array()
        .filter(|items| !items.is_empty() && items.iter().all(is_content_block))
        .cloned();
    let mut content = blocks.unwrap_or_else(|| vec![json!({"type": "text", "text": value})]);
    content.extend(program_logs(stdout, stderr));
    ToolResult::Success {
        content,
        structured_content: Some(parsed),
        meta: None,
    }
}

fn is_content_block(value: &Value) -> bool {
    match value["type"].as_str() {
        Some("text") => value["text"].is_string(),
        Some("image" | "audio") => value["data"].is_string() && value["mimeType"].is_string(),
        Some("resource_link") => value["uri"].is_string() && value["name"].is_string(),
        Some("resource") => value["resource"].is_object(),
        _ => false,
    }
}

/// What a program that threw `error` answers.
fn program_failure(error: &Value, stdout: &str, stderr: &str) -> ToolResult {
    let mut content =
        vec![json!({"type": "text", "text": format!("run_typescript failed: {}", compact(error))})];
    content.extend(program_logs(stdout, stderr));
    ToolResult::Failure {
        content,
        code: error["name"]
            .as_str()
            .unwrap_or("ExecutionError")
            .to_owned(),
        message: error["message"]
            .as_str()
            .map_or_else(|| compact(error), str::to_owned),
        details: error
            .get("details")
            .filter(|details| !details.is_null())
            .cloned(),
        retryable: false,
        meta: None,
    }
}

/// Opens the callbacks one call asks.
struct EngineApprover<'a> {
    core: &'a Arc<Mutex<SessionCore>>,
    sink: &'a Sink,
    action: &'a ToolAction,
    call_id: &'a str,
}

impl Approver for EngineApprover<'_> {
    fn ask(&self, required_permissions: Vec<Value>, reason: Option<String>) -> AskFuture<'_> {
        Box::pin(async move {
            let receiver = {
                let Ok(mut core) = self.core.lock() else {
                    return CallbackAnswer::Closed("the session is gone".to_owned());
                };
                let callback_id = core
                    .callbacks
                    .fresh_id(&format!("approval-{}", self.call_id));
                let entry = approval_callback(
                    &core.projector.state.session.id,
                    &callback_id,
                    self.action,
                    now_millis(),
                    core.input_entry_of(&self.action.turn_id).as_deref(),
                    reason.as_deref(),
                    required_permissions,
                );
                open_callback(&mut core, self.sink, entry)
            };
            receiver
                .await
                .unwrap_or_else(|_| CallbackAnswer::Closed("Turn interrupted".to_owned()))
        })
    }
}

/// Opens `entry`, publishes it, and answers where its answer arrives.
fn open_callback(
    core: &mut SessionCore,
    sink: &Sink,
    entry: Value,
) -> tokio::sync::oneshot::Receiver<CallbackAnswer> {
    let receiver = core.callbacks.open(entry.clone());
    sink(core.publication(Advance::Provisional, Some(CallbackEvent::Requested(entry))));
    receiver
}

/// Reference `_request_user_input`: the questions go to the person, and
/// their answers are the result. A callback closed without answers fails the
/// turn.
async fn ask_user(
    core: &Arc<Mutex<SessionCore>>,
    sink: &Sink,
    call_id: &str,
    action: &ToolAction,
) -> Result<ToolResult, String> {
    let receiver = {
        let Ok(mut core) = core.lock() else {
            return Err("the session is gone".to_owned());
        };
        let callback_id = core.callbacks.fresh_id(&format!("user-input-{call_id}"));
        let entry = user_input_callback(
            &core.projector.state.session.id,
            &callback_id,
            action,
            call_id,
            now_millis(),
            core.input_entry_of(&action.turn_id).as_deref(),
        );
        open_callback(&mut core, sink, entry)
    };
    match receiver.await {
        Ok(CallbackAnswer::Output(output)) => {
            let answers = crate::tools::vibe::question_result(&output["result"]);
            Ok(ToolResult::Success {
                content: vec![
                    json!({"type": "text", "text": crate::tools::vibe::python_dumps(&answers)}),
                ],
                structured_content: Some(answers),
                meta: None,
            })
        }
        Ok(CallbackAnswer::Closed(message)) => Err(message),
        Err(_) => Err("Callback delivery failed".to_owned()),
    }
}

async fn execute(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    sink: &Sink,
    call_id: &str,
    action: &ToolAction,
) -> Result<ToolResult, String> {
    let arguments = action.arguments.clone();
    Ok(match action.tool_name.as_str() {
        "ui.ask_user_question" => return ask_user(core, sink, call_id, action).await,
        name @ ("file_system.read_file"
        | "file_system.write_file"
        | "file_system.search_replace") => {
            let name = name.to_owned();
            let workspace = environment.workspace.clone();
            tokio::task::spawn_blocking(move || {
                crate::tools::files::execute(&name, &arguments, &workspace)
            })
            .await
            .unwrap_or_else(|error| ToolResult::failed("tool_failed", error.to_string()))
        }
        "file_system.bash" => crate::tools::shell::execute(&arguments, &environment.shell).await,
        name if name.starts_with("process.") => {
            let (manager, session_id) = match core.lock() {
                Ok(core) => (
                    Arc::clone(&core.processes),
                    core.projector.state.session.id.clone(),
                ),
                Err(_) => return Ok(ToolResult::failed("tool_failed", "the session is gone")),
            };
            let operation = name.trim_start_matches("process.").to_owned();
            let process_id = crate::processes::process_id(&session_id, &action.action_id, call_id);
            let workspace = environment.workspace.cwd.clone();
            let session_dir = environment.session_dir.clone();
            tokio::task::spawn_blocking(move || match operation.as_str() {
                "start" => {
                    manager.start(process_id, &arguments, &workspace, session_dir.as_deref())
                }
                "output" => manager.output(&arguments),
                "write" => manager.write(&arguments),
                "list" => manager.list(),
                "stop" => manager.stop(&arguments),
                other => {
                    ToolResult::failed("tool_failed", format!("Unsupported tool: process.{other}"))
                }
            })
            .await
            .unwrap_or_else(|error| ToolResult::failed("tool_failed", error.to_string()))
        }
        name if name.starts_with("subagent.") => {
            let (manager, session_id, message_id, scratchpad) = match core.lock() {
                Ok(core) => (
                    Arc::clone(&core.subagents),
                    core.projector.state.session.id.clone(),
                    core.projector
                        .state
                        .latest_turn
                        .as_ref()
                        .and_then(|turn| turn.input_entry_id.clone()),
                    core.vibe.scratchpad.clone(),
                ),
                Err(_) => return Ok(ToolResult::failed("tool_failed", "the session is gone")),
            };
            let operation = name.trim_start_matches("subagent.");
            manager
                .execute(
                    operation,
                    &arguments,
                    &action.action_id,
                    &session_id,
                    environment,
                    message_id,
                    &scratchpad,
                )
                .await
        }
        "self.sleep" => {
            let seconds = arguments["seconds"].as_f64().unwrap_or_default();
            tokio::time::sleep(Duration::from_secs_f64(seconds.clamp(0.0, 60.0))).await;
            ToolResult::Success {
                content: Vec::new(),
                structured_content: Some(json!({"seconds": arguments["seconds"]})),
                meta: None,
            }
        }
        "skill.read" => {
            let name = arguments["name"].as_str().unwrap_or_default();
            match environment.skills.iter().find(|(known, _)| known == name) {
                Some((_, body)) => ToolResult::Success {
                    content: vec![json!({"type": "text", "text": body})],
                    structured_content: Some(json!(body)),
                    meta: None,
                },
                None => {
                    let mut names: Vec<&str> = environment
                        .skills
                        .iter()
                        .map(|(known, _)| known.as_str())
                        .collect();
                    names.sort_unstable();
                    let listing = if names.is_empty() {
                        "none".to_owned()
                    } else {
                        names.join(", ")
                    };
                    ToolResult::failed(
                        "tool_failed",
                        format!(
                            "Skill \"{name}\" is not available to this runtime. Available skills: {listing}"
                        ),
                    )
                }
            }
        }
        "vibe.cron" => match core.lock() {
            Ok(core) => core.loops.run_tool(&arguments),
            Err(_) => ToolResult::failed("tool_failed", "the session is gone"),
        },
        name if name.starts_with("vibe.") => {
            let tool = name.trim_start_matches("vibe.");
            match core.lock() {
                Ok(mut core) => core.vibe.execute(tool, &arguments),
                Err(_) => ToolResult::failed("tool_failed", "the session is gone"),
            }
        }
        other => match &environment.provided {
            Some(provided) if provided.routes(other) => provided.call(other, arguments).await,
            _ => ToolResult::failed("tool_failed", format!("Unsupported tool: {other}")),
        },
    })
}

/// A random identifier shaped like a UUID.
#[must_use]
pub fn uuid_like() -> String {
    let hex = random_hex(16);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `bytes` random bytes as lowercase hex.
#[must_use]
pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    if getrandom::fill(&mut buffer).is_err() {
        let seed = now_millis().to_le_bytes();
        for (index, byte) in buffer.iter_mut().enumerate() {
            *byte = seed[index % seed.len()].wrapping_add(u8::try_from(index % 256).unwrap_or(0));
        }
    }
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

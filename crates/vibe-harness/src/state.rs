//! The public session state the Unified Harness republishes after every step,
//! and the projector that derives each one from the step's observations.
//!
//! Reference `SessionProjector` and the entry builders beside it
//! (`harness/runtimes/python/python/mistralai_vibe_local_harness/vibe/_projection.py`),
//! over the `harness.public-session-state/v1` shapes of
//! `mistralai_vibe_local_harness/session_protocol.py`. History entries stay
//! JSON objects: the adapter re-projects them into the wire's typed entries
//! (`crate::wire`), and the store writes them as they are.

use serde_json::{Map, Value, json};

/// The format tag of a harness snapshot.
pub const HARNESS_STATE_FORMAT: &str = "harness.public-session-state/v1";

/// How many characters a session preview keeps before its ellipsis.
const PREVIEW_MAX_CHARS: usize = 200;

/// The turn queue's capacity, which every snapshot publishes.
pub const TURN_QUEUE_MAX_ITEMS: u64 = 32;

/// Token accounting, cumulative on the session and per call on the context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
}

impl TokenUsage {
    #[must_use]
    pub fn added(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            total_tokens: self.total_tokens + other.total_tokens,
            cached_input_tokens: self.cached_input_tokens + other.cached_input_tokens,
        }
    }

    /// The harness shape, which names the cached tokens.
    #[must_use]
    pub fn to_json(self) -> Value {
        json!({
            "cachedInputTokens": self.cached_input_tokens,
            "inputTokens": self.input_tokens,
            "outputTokens": self.output_tokens,
            "totalTokens": self.total_tokens,
        })
    }

    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        let field = |name: &str| value.get(name).and_then(Value::as_u64).unwrap_or(0);
        value.is_object().then(|| Self {
            input_tokens: field("inputTokens"),
            output_tokens: field("outputTokens"),
            total_tokens: field("totalTokens"),
            cached_input_tokens: field("cachedInputTokens"),
        })
    }
}

/// What the session is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Idle,
    Running {
        active_turn_id: String,
    },
    Blocked {
        active_turn_id: String,
        callback_id: String,
        callback_kind: String,
    },
    Failed {
        message: String,
    },
}

impl SessionStatus {
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::Idle => json!({"type": "idle"}),
            Self::Running { active_turn_id } => {
                json!({"type": "running", "activeTurnId": active_turn_id})
            }
            Self::Blocked {
                active_turn_id,
                callback_id,
                callback_kind,
            } => json!({
                "type": "blocked",
                "activeTurnId": active_turn_id,
                "callbackId": callback_id,
                "callbackKind": callback_kind,
            }),
            Self::Failed { message } => json!({"type": "failed", "message": message}),
        }
    }

    #[must_use]
    pub fn from_json(value: &Value) -> Self {
        let text = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        match value.get("type").and_then(Value::as_str) {
            Some("running") => Self::Running {
                active_turn_id: text("activeTurnId"),
            },
            Some("blocked") => Self::Blocked {
                active_turn_id: text("activeTurnId"),
                callback_id: text("callbackId"),
                callback_kind: text("callbackKind"),
            },
            Some("failed") => Self::Failed {
                message: text("message"),
            },
            _ => Self::Idle,
        }
    }

    #[must_use]
    pub fn active_turn_id(&self) -> Option<&str> {
        match self {
            Self::Running { active_turn_id } | Self::Blocked { active_turn_id, .. } => {
                Some(active_turn_id)
            }
            Self::Idle | Self::Failed { .. } => None,
        }
    }
}

/// The session a snapshot describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessSession {
    pub id: String,
    pub root_session_id: String,
    pub parent_session_id: Option<String>,
    pub title: Option<String>,
    pub title_source: String,
    pub preview: String,
    pub status: SessionStatus,
    pub created_at: u64,
    pub updated_at: u64,
    pub token_usage: Option<TokenUsage>,
    pub context_usage: Option<TokenUsage>,
}

impl HarnessSession {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "contextUsage": self.context_usage.map(TokenUsage::to_json),
            "createdAt": self.created_at,
            "id": self.id,
            "parentSessionId": self.parent_session_id,
            "preview": self.preview,
            "rootSessionId": self.root_session_id,
            "status": self.status.to_json(),
            "title": self.title,
            "titleSource": self.title_source,
            "tokenUsage": self.token_usage.map(TokenUsage::to_json),
            "updatedAt": self.updated_at,
        })
    }

    #[must_use]
    pub fn from_json(value: &Value) -> Self {
        let text = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
        let id = text("id").unwrap_or_default();
        Self {
            root_session_id: text("rootSessionId").unwrap_or_else(|| id.clone()),
            id,
            parent_session_id: text("parentSessionId"),
            title: text("title"),
            title_source: text("titleSource").unwrap_or_else(|| "auto".to_owned()),
            preview: text("preview").unwrap_or_default(),
            status: value
                .get("status")
                .map_or(SessionStatus::Idle, SessionStatus::from_json),
            created_at: value.get("createdAt").and_then(Value::as_u64).unwrap_or(0),
            updated_at: value.get("updatedAt").and_then(Value::as_u64).unwrap_or(0),
            token_usage: value.get("tokenUsage").and_then(TokenUsage::from_json),
            context_usage: value.get("contextUsage").and_then(TokenUsage::from_json),
        }
    }
}

/// The state a turn ended in, or that it is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Failed,
}

impl TurnStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "completed" => Self::Completed,
            "interrupted" => Self::Interrupted,
            "failed" => Self::Failed,
            _ => Self::InProgress,
        }
    }
}

/// An error a failed turn carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicError {
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
}

impl PublicError {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({"code": self.code, "message": self.message, "details": self.details})
    }
}

/// The session's latest turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessTurn {
    pub id: String,
    pub session_id: String,
    pub status: TurnStatus,
    pub queue_item_id: Option<String>,
    pub input_entry_id: Option<String>,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub error: Option<PublicError>,
    /// Why an interrupted turn stopped.
    pub reason: Option<String>,
}

impl HarnessTurn {
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut turn = json!({
            "id": self.id,
            "inputEntryId": self.input_entry_id,
            "queueItemId": self.queue_item_id,
            "sessionId": self.session_id,
            "startedAt": self.started_at,
            "status": self.status.as_str(),
        });
        if self.status != TurnStatus::InProgress {
            turn["completedAt"] = json!(self.completed_at);
        }
        // Only a completed turn records a stop reason: an interrupted one
        // carries its reason and a failed one its error.
        if self.status == TurnStatus::Completed {
            turn["stopReason"] = Value::Null;
        }
        if self.status == TurnStatus::Failed {
            turn["error"] = self
                .error
                .as_ref()
                .map_or(Value::Null, PublicError::to_json);
        }
        if self.status == TurnStatus::Interrupted {
            turn["reason"] = json!(self.reason);
        }
        turn
    }

    #[must_use]
    pub fn from_json(value: &Value) -> Self {
        let text = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
        Self {
            id: text("id").unwrap_or_default(),
            session_id: text("sessionId").unwrap_or_default(),
            status: TurnStatus::parse(text("status").as_deref().unwrap_or_default()),
            queue_item_id: text("queueItemId"),
            input_entry_id: text("inputEntryId"),
            started_at: value.get("startedAt").and_then(Value::as_u64).unwrap_or(0),
            completed_at: value.get("completedAt").and_then(Value::as_u64),
            error: value
                .get("error")
                .filter(|error| error.is_object())
                .map(|error| PublicError {
                    code: error["code"].as_str().unwrap_or_default().to_owned(),
                    message: error["message"].as_str().unwrap_or_default().to_owned(),
                    details: error.get("details").filter(|d| !d.is_null()).cloned(),
                }),
            reason: text("reason"),
        }
    }

    fn in_progress_for(&self, turn_id: &str) -> bool {
        self.status == TurnStatus::InProgress && self.id == turn_id
    }
}

/// A whole snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessState {
    pub session: HarnessSession,
    pub entries: Vec<Value>,
    pub latest_turn: Option<HarnessTurn>,
    pub queue_items: Vec<Value>,
    pub queue_paused: bool,
    pub active_callbacks: Vec<Value>,
    pub background_processes: Vec<Value>,
    pub retrying: Option<Value>,
}

impl HarnessState {
    /// A new session's first snapshot.
    #[must_use]
    pub fn new(session_id: &str, parent_session_id: Option<&str>, root: &str, now: u64) -> Self {
        Self {
            session: HarnessSession {
                id: session_id.to_owned(),
                root_session_id: root.to_owned(),
                parent_session_id: parent_session_id.map(str::to_owned),
                title: None,
                title_source: "auto".to_owned(),
                preview: String::new(),
                status: SessionStatus::Idle,
                created_at: now,
                updated_at: now,
                token_usage: None,
                context_usage: None,
            },
            entries: Vec::new(),
            latest_turn: None,
            queue_items: Vec::new(),
            queue_paused: false,
            active_callbacks: Vec::new(),
            background_processes: Vec::new(),
            retrying: None,
        }
    }

    /// The snapshot as the store persists it: the history paged out to the
    /// store's own chunks, so the snapshot itself carries an empty page.
    #[must_use]
    pub fn persisted_json(&self) -> Value {
        json!({
            "activeCallbacks": self.active_callbacks,
            "format": HARNESS_STATE_FORMAT,
            "history": {"cursor": {"after": null, "before": null}, "entries": [], "range": "latest"},
            "latestTurn": self.latest_turn.as_ref().map(HarnessTurn::to_json),
            "session": self.session.to_json(),
            "turnQueue": self.turn_queue_json(),
        })
    }

    #[must_use]
    pub fn turn_queue_json(&self) -> Value {
        json!({
            "items": self.queue_items,
            "maxItems": TURN_QUEUE_MAX_ITEMS,
            "paused": self.queue_paused,
        })
    }

    /// Rebuilds a snapshot the store persisted, with the history it paged out.
    #[must_use]
    pub fn from_persisted(snapshot: &Value, entries: Vec<Value>) -> Self {
        let queue = &snapshot["turnQueue"];
        Self {
            session: HarnessSession::from_json(&snapshot["session"]),
            entries,
            latest_turn: snapshot
                .get("latestTurn")
                .filter(|turn| turn.is_object())
                .map(HarnessTurn::from_json),
            queue_items: queue["items"].as_array().cloned().unwrap_or_default(),
            queue_paused: queue["paused"].as_bool().unwrap_or(false),
            active_callbacks: snapshot["activeCallbacks"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            background_processes: Vec::new(),
            retrying: None,
        }
    }
}

// --------------------------------------------------------------------------
// Observations
// --------------------------------------------------------------------------

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

// --------------------------------------------------------------------------
// The projector
// --------------------------------------------------------------------------

/// Derives each snapshot from the one before.
///
/// The live snapshot carries provisional completion content the journal never
/// holds; `durable` is the journaled view, and the watermark counts journaled
/// advances only.
#[derive(Debug, Clone)]
pub struct Projector {
    pub state: HarnessState,
    pub watermark: u64,
}

/// Whether an advance is journaled or only published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    Journaled,
    Provisional,
}

impl Projector {
    #[must_use]
    pub fn new(mut state: HarnessState, watermark: u64) -> Self {
        if state.session.preview.is_empty() {
            state.session.preview = first_user_message_preview(&state.entries);
        }
        Self { state, watermark }
    }

    /// The journaled view: everything but streamed content.
    #[must_use]
    pub fn durable(&self) -> HarnessState {
        let mut durable = self.state.clone();
        durable.entries.retain(|entry| !is_provisional(entry));
        durable
    }

    fn advance(&mut self, kind: Advance) -> Advance {
        if kind == Advance::Journaled {
            self.watermark += 1;
        }
        kind
    }

    /// Reference `apply_action_started`: a tool call's running entry.
    pub fn apply_action_started(&mut self, action: &ToolAction, observed_at: u64) -> Advance {
        let input_entry_id =
            active_input_entry_id(self.state.latest_turn.as_ref(), &action.turn_id);
        let entry =
            running_effect_entry(&self.state.session.id, action, observed_at, input_entry_id);
        upsert(&mut self.state.entries, entry);
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }

    /// Reference `SessionProjector.apply`.
    pub fn apply(
        &mut self,
        input_id: u64,
        observations: &[Observation],
        turn: Option<&TransitionTurn>,
        observed_at: u64,
    ) -> Advance {
        let session_id = self.state.session.id.clone();
        let mut entries = std::mem::take(&mut self.state.entries);
        let mut latest_turn = self.state.latest_turn.take();
        let mut token_usage = self.state.session.token_usage;
        let mut context_usage = self.state.session.context_usage;
        let mut preview = if self.state.session.preview.is_empty() {
            first_user_message_preview(&entries)
        } else {
            self.state.session.preview.clone()
        };
        for observation in observations {
            match observation {
                Observation::TurnStarted {
                    turn_id,
                    content,
                    injected,
                    queue_item_id,
                    client_message_id,
                    user_display_content,
                } => {
                    let mut turn = HarnessTurn {
                        id: turn_id.clone(),
                        session_id: session_id.clone(),
                        status: TurnStatus::InProgress,
                        queue_item_id: queue_item_id.clone(),
                        input_entry_id: None,
                        started_at: observed_at,
                        completed_at: None,
                        error: None,
                        reason: None,
                    };
                    if *injected {
                        latest_turn = Some(turn);
                        continue;
                    }
                    if preview.is_empty() {
                        preview = content_preview(content);
                    }
                    let existing = entries.iter().find(|candidate| {
                        candidate["type"] == "message"
                            && candidate["role"] == "user"
                            && candidate["turnId"] == json!(turn_id)
                            && candidate["source"] == "turn_start"
                    });
                    let user_entry_id = if let Some(existing) = existing {
                        existing["id"].as_str().unwrap_or_default().to_owned()
                    } else {
                        let entry = message_entry(&MessageEntry {
                            session_id: &session_id,
                            turn_id: Some(turn_id),
                            entry_id: &format!("user-{turn_id}-{input_id}"),
                            role: "user",
                            content,
                            observed_at,
                            source: "turn_start",
                            input_entry_id: None,
                            client_message_id: client_message_id.as_deref(),
                            user_display_content: user_display_content.as_ref(),
                        });
                        let id = entry["id"].as_str().unwrap_or_default().to_owned();
                        entries.push(entry);
                        id
                    };
                    turn.input_entry_id = Some(user_entry_id);
                    latest_turn = Some(turn);
                }
                Observation::TurnSteeringReceived {
                    turn_id,
                    content,
                    client_message_id,
                    user_display_content,
                } => {
                    entries.push(message_entry(&MessageEntry {
                        session_id: &session_id,
                        turn_id: Some(turn_id),
                        entry_id: &format!("user-{turn_id}-{input_id}"),
                        role: "user",
                        content,
                        observed_at,
                        source: "turn_steer",
                        input_entry_id: None,
                        client_message_id: client_message_id.as_deref(),
                        user_display_content: user_display_content.as_ref(),
                    }));
                }
                Observation::TurnSteered {
                    turn_id,
                    client_message_id,
                } => {
                    let active = active_input_entry_id(latest_turn.as_ref(), turn_id);
                    if let Some(input_entry_id) = steered_input_entry_id(
                        &entries,
                        turn_id,
                        active.as_deref(),
                        client_message_id.as_deref(),
                    ) && let Some(turn) = latest_turn.as_mut()
                    {
                        turn.input_entry_id = Some(input_entry_id);
                    }
                }
                Observation::AssistantMessageCommitted {
                    turn_id,
                    action_id,
                    reasoning,
                    content,
                    usage,
                } => {
                    let input_entry_id = active_input_entry_id(latest_turn.as_ref(), turn_id);
                    for (index, part) in reasoning.iter().enumerate() {
                        if let Some(entry) = reasoning_entry(
                            &session_id,
                            turn_id,
                            &format!("reasoning-{action_id}-{index}"),
                            part,
                            observed_at,
                            input_entry_id.as_deref(),
                        ) {
                            upsert(&mut entries, entry);
                        }
                    }
                    if !content.is_empty() {
                        upsert(
                            &mut entries,
                            message_entry(&MessageEntry {
                                session_id: &session_id,
                                turn_id: Some(turn_id),
                                entry_id: &format!("assistant-{action_id}"),
                                role: "assistant",
                                content,
                                observed_at,
                                source: "harness",
                                input_entry_id: input_entry_id.as_deref(),
                                client_message_id: None,
                                user_display_content: None,
                            }),
                        );
                    }
                    for entry in &mut entries {
                        if is_provisional(entry) && belongs_to_action(entry, action_id) {
                            settle_provisional(entry, observed_at, "failure");
                        }
                    }
                    if let Some(usage) = usage {
                        token_usage = Some(token_usage.unwrap_or_default().added(*usage));
                        context_usage = Some(*usage);
                    }
                }
                Observation::ToolResultCommitted {
                    turn_id,
                    action_id,
                    result,
                } => {
                    let input_entry_id = active_input_entry_id(latest_turn.as_ref(), turn_id);
                    let entry = completed_effect_entry(
                        &session_id,
                        &entries,
                        turn_id,
                        action_id,
                        result,
                        observed_at,
                        input_entry_id.as_deref(),
                    );
                    upsert(&mut entries, entry);
                }
                Observation::ToolDiscoveryFinished {
                    turn_id,
                    call_id,
                    mode,
                    tool_count,
                    connector_notice_count,
                } => {
                    let input_entry_id = active_input_entry_id(latest_turn.as_ref(), turn_id);
                    let message = "for relevant tools";
                    upsert(
                        &mut entries,
                        json!({
                            "type": "effect",
                            "id": format!("effect-tool-discovery-{call_id}"),
                            "sessionId": session_id,
                            "turnId": turn_id,
                            "createdAt": observed_at,
                            "updatedAt": observed_at,
                            "generationStatus": "completed",
                            "relatedEntryId": null,
                            "inputEntryId": input_entry_id,
                            "title": "Searching for relevant tools",
                            "detail": {
                                "kind": "tool",
                                "toolName": "search_tool_functions",
                                "input": {"mode": mode},
                                "display": {
                                    "summary": "Searching for relevant tools",
                                    "verb": "Searching",
                                    "message": "Searching for relevant tools",
                                    "settledVerb": "Searched",
                                    "settledMessage": message,
                                    "statusText": "Searching for relevant tools",
                                },
                            },
                            "state": {
                                "status": "completed",
                                "output": {
                                    "mode": mode,
                                    "toolCount": tool_count,
                                    "connectorNoticeCount": connector_notice_count,
                                },
                                "outputText": "",
                                "display": {"success": true, "verb": "Searched", "message": message},
                            },
                        }),
                    );
                }
                Observation::ContextCompacted {
                    turn_id,
                    compaction_id,
                    trigger,
                    attempt,
                    summary_len,
                    usage,
                } => {
                    let input_entry_id = turn_id
                        .as_deref()
                        .and_then(|turn| active_input_entry_id(latest_turn.as_ref(), turn));
                    let entry = terminal_compaction_entry(
                        &session_id,
                        &entries,
                        compaction_id,
                        turn_id.as_deref(),
                        observed_at,
                        input_entry_id.as_deref(),
                        "Context compacted",
                        json!({"trigger": trigger, "attempt": attempt, "summaryLength": summary_len}),
                    );
                    upsert(&mut entries, entry);
                    if let Some(usage) = usage {
                        token_usage = Some(token_usage.unwrap_or_default().added(*usage));
                    }
                    context_usage = Some(TokenUsage::default());
                }
                Observation::ContextCompactionFailed {
                    turn_id,
                    compaction_id,
                    trigger,
                    attempt,
                    code,
                    reason,
                } => {
                    let input_entry_id = turn_id
                        .as_deref()
                        .and_then(|turn| active_input_entry_id(latest_turn.as_ref(), turn));
                    let entry = terminal_compaction_entry(
                        &session_id,
                        &entries,
                        compaction_id,
                        turn_id.as_deref(),
                        observed_at,
                        input_entry_id.as_deref(),
                        "Context compaction failed",
                        json!({
                            "trigger": trigger,
                            "attempt": attempt,
                            "error": {"code": code},
                            "reason": reason,
                        }),
                    );
                    upsert(&mut entries, entry);
                }
                Observation::TurnCompleted { turn_id } => {
                    let started_at = started_at(latest_turn.as_ref(), turn_id, observed_at);
                    entries.retain(|entry| !is_open_effect(entry, turn_id));
                    for entry in &mut entries {
                        if is_provisional(entry) && entry["turnId"] == json!(turn_id) {
                            settle_provisional(entry, observed_at, "failure");
                        }
                    }
                    latest_turn = Some(settled_turn(
                        latest_turn.as_ref(),
                        &session_id,
                        turn_id,
                        TurnStatus::Completed,
                        started_at,
                        observed_at,
                    ));
                }
                Observation::TurnInterrupted { turn_id, reason } => {
                    let started_at = started_at(latest_turn.as_ref(), turn_id, observed_at);
                    settle_open_effects(
                        &mut entries,
                        turn_id,
                        observed_at,
                        true,
                        reason.as_deref().unwrap_or("Turn interrupted"),
                    );
                    let mut turn = settled_turn(
                        latest_turn.as_ref(),
                        &session_id,
                        turn_id,
                        TurnStatus::Interrupted,
                        started_at,
                        observed_at,
                    );
                    turn.reason.clone_from(reason);
                    latest_turn = Some(turn);
                }
                Observation::TurnFailed { turn_id, error } => {
                    let started_at = started_at(latest_turn.as_ref(), turn_id, observed_at);
                    settle_open_effects(&mut entries, turn_id, observed_at, false, &error.message);
                    let mut turn = settled_turn(
                        latest_turn.as_ref(),
                        &session_id,
                        turn_id,
                        TurnStatus::Failed,
                        started_at,
                        observed_at,
                    );
                    turn.error = Some(error.clone());
                    latest_turn = Some(turn);
                }
            }
        }
        self.state.session.status = match turn {
            Some(TransitionTurn::Running(turn_id)) => SessionStatus::Running {
                active_turn_id: turn_id.clone(),
            },
            Some(TransitionTurn::Failed(message)) => SessionStatus::Failed {
                message: message.clone(),
            },
            Some(TransitionTurn::Settled) | None => SessionStatus::Idle,
        };
        if matches!(
            self.state.session.status,
            SessionStatus::Idle | SessionStatus::Failed { .. }
        ) {
            // A provider retry only lives while a completion is in flight.
            self.state.retrying = None;
        }
        self.state.session.updated_at = observed_at;
        self.state.session.preview = preview;
        self.state.session.token_usage = token_usage;
        self.state.session.context_usage = context_usage;
        self.state.entries = entries;
        self.state.latest_turn = latest_turn;
        self.advance(Advance::Journaled)
    }

    /// Reference `append_provisional_completion_content`.
    pub fn apply_completion_delta(&mut self, delta: &CompletionDelta, observed_at: u64) -> Advance {
        let session_id = self.state.session.id.clone();
        let input_entry_id = active_input_entry_id(self.state.latest_turn.as_ref(), &delta.turn_id);
        let entries = &mut self.state.entries;
        if delta.restart {
            for entry in entries.iter_mut() {
                if is_provisional(entry) && belongs_to_action(entry, &delta.action_id) {
                    if entry["type"] == "reasoning" {
                        entry["text"] = json!("");
                    } else {
                        entry["content"] = json!([{"type": "text", "text": ""}]);
                    }
                    entry["updatedAt"] = json!(observed_at);
                }
            }
        }
        if !delta.reasoning.is_empty() {
            let id = format!("reasoning-{}-0", delta.action_id);
            merge_provisional(entries, &id, observed_at, |entry| {
                let text = entry["text"].as_str().unwrap_or_default().to_owned();
                entry["text"] = json!(format!("{text}{}", delta.reasoning));
            })
            .unwrap_or_else(|| {
                entries.push(json!({
                    "type": "reasoning",
                    "id": id,
                    "sessionId": session_id,
                    "turnId": delta.turn_id,
                    "createdAt": observed_at,
                    "updatedAt": observed_at,
                    "generationStatus": "in_progress",
                    "outcome": null,
                    "relatedEntryId": null,
                    "inputEntryId": input_entry_id,
                    "text": delta.reasoning,
                    "summary": [],
                }));
            });
        }
        if !delta.text.is_empty() {
            let id = format!("assistant-{}", delta.action_id);
            merge_provisional(entries, &id, observed_at, |entry| {
                let mut blocks = entry["content"].as_array().cloned().unwrap_or_default();
                if blocks.is_empty() || !blocks[0].is_object() {
                    blocks.insert(0, json!({"type": "text", "text": ""}));
                }
                let text = blocks[0]["text"].as_str().unwrap_or_default().to_owned();
                blocks[0]["text"] = json!(format!("{text}{}", delta.text));
                entry["content"] = Value::Array(blocks);
            })
            .unwrap_or_else(|| {
                entries.push(json!({
                    "type": "message",
                    "id": id,
                    "sessionId": session_id,
                    "turnId": delta.turn_id,
                    "createdAt": observed_at,
                    "updatedAt": observed_at,
                    "generationStatus": "in_progress",
                    "outcome": null,
                    "relatedEntryId": null,
                    "inputEntryId": input_entry_id,
                    "role": "assistant",
                    "content": [{"type": "text", "text": delta.text}],
                    "source": "harness",
                }));
            });
        }
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Provisional)
    }

    /// Reference `apply_model_change`.
    pub fn apply_model_change(&mut self, model: &str, observed_at: u64) -> Advance {
        let sequence = count_checkpoints(&self.state.entries, "model_change") + 1;
        self.state.entries.push(json!({
            "type": "checkpoint",
            "id": format!("checkpoint-model-change-{sequence}"),
            "sessionId": self.state.session.id,
            "turnId": null,
            "createdAt": observed_at,
            "updatedAt": observed_at,
            "generationStatus": "completed",
            "relatedEntryId": null,
            "inputEntryId": null,
            "kind": "model_change",
            "message": format!("Model changed to {model}"),
            "details": {"model": model},
        }));
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }

    /// Reference `apply_agent_change`.
    pub fn apply_agent_change(
        &mut self,
        agent: &str,
        turn_id: Option<&str>,
        observed_at: u64,
    ) -> Advance {
        let sequence = count_checkpoints(&self.state.entries, "agent_change") + 1;
        self.state.entries.push(json!({
            "type": "checkpoint",
            "id": format!("checkpoint-agent-change-{sequence}"),
            "sessionId": self.state.session.id,
            "turnId": turn_id,
            "createdAt": observed_at,
            "updatedAt": observed_at,
            "generationStatus": "completed",
            "relatedEntryId": null,
            "kind": "agent_change",
            "message": format!("Agent changed to {agent}"),
            "details": {"agent": agent},
        }));
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }

    /// Reference `append_public_history_entries`.
    pub fn append_entries(&mut self, entries: Vec<Value>, observed_at: u64) -> Advance {
        self.state.entries.extend(entries);
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }

    /// Adds `entry`, or replaces the entry it shares an identifier with,
    /// without journaling it: a step still in flight.
    pub fn upsert_provisional(&mut self, entry: Value, observed_at: u64) -> Advance {
        upsert(&mut self.state.entries, entry);
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Provisional)
    }

    /// Reference `rename_session`.
    pub fn rename(&mut self, title: &str, source: &str, observed_at: u64) -> Advance {
        self.state.session.title = Some(title.to_owned());
        source.clone_into(&mut self.state.session.title_source);
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }

    /// A snapshot published without a step behind it, which only stamps the
    /// session: reference `_advance` over an unchanged state with a new
    /// observation time.
    pub fn touch(&mut self, observed_at: u64) -> Advance {
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }
}

/// Reference `settle_stalled_projection`: a running turn nothing owns any more
/// reads as interrupted.
pub fn settle_stalled(state: &mut HarnessState, observed_at: u64) {
    let Some(turn_id) = state.session.status.active_turn_id().map(str::to_owned) else {
        return;
    };
    let reason = "Interrupted by process restart";
    settle_open_effects(&mut state.entries, &turn_id, observed_at, true, reason);
    let started = started_at(state.latest_turn.as_ref(), &turn_id, observed_at);
    let mut turn = settled_turn(
        state.latest_turn.as_ref(),
        &state.session.id,
        &turn_id,
        TurnStatus::Interrupted,
        started,
        observed_at,
    );
    turn.reason = Some(reason.to_owned());
    state.latest_turn = Some(turn);
    state.session.status = SessionStatus::Idle;
}

// --------------------------------------------------------------------------
// Entry builders
// --------------------------------------------------------------------------

struct MessageEntry<'a> {
    session_id: &'a str,
    turn_id: Option<&'a str>,
    entry_id: &'a str,
    role: &'a str,
    content: &'a [Value],
    observed_at: u64,
    source: &'a str,
    input_entry_id: Option<&'a str>,
    client_message_id: Option<&'a str>,
    user_display_content: Option<&'a Value>,
}

fn message_entry(entry: &MessageEntry<'_>) -> Value {
    let mut out = json!({
        "type": "message",
        "id": entry.client_message_id.unwrap_or(entry.entry_id),
        "sessionId": entry.session_id,
        "turnId": entry.turn_id,
        "createdAt": entry.observed_at,
        "updatedAt": entry.observed_at,
        "generationStatus": "completed",
        "relatedEntryId": null,
        "inputEntryId": entry.input_entry_id,
        "role": entry.role,
        "content": entry.content,
        "source": entry.source,
    });
    if entry.role == "user"
        && let Some(display) = entry.user_display_content
    {
        out["userDisplayContent"] = display.clone();
    }
    if entry.role == "assistant" {
        out["outcome"] = json!({"type": "committed"});
    }
    out
}

/// A user message entry the host appends on its own.
#[must_use]
pub fn public_message_entry(
    session_id: &str,
    turn_id: Option<&str>,
    entry_id: &str,
    role: &str,
    content: &[Value],
    observed_at: u64,
    source: &str,
) -> Value {
    message_entry(&MessageEntry {
        session_id,
        turn_id,
        entry_id,
        role,
        content,
        observed_at,
        source,
        input_entry_id: None,
        client_message_id: None,
        user_display_content: None,
    })
}

fn reasoning_entry(
    session_id: &str,
    turn_id: &str,
    entry_id: &str,
    part: &ReasoningPart,
    observed_at: u64,
    input_entry_id: Option<&str>,
) -> Option<Value> {
    let mut text = part.text.concat();
    if text.is_empty() && part.summary.is_empty() && !part.redacted {
        return None;
    }
    if text.is_empty() && part.redacted {
        "[redacted]".clone_into(&mut text);
    }
    Some(json!({
        "type": "reasoning",
        "id": entry_id,
        "sessionId": session_id,
        "turnId": turn_id,
        "createdAt": observed_at,
        "updatedAt": observed_at,
        "generationStatus": "completed",
        "outcome": {"type": "committed"},
        "relatedEntryId": null,
        "inputEntryId": input_entry_id,
        "text": text,
        "summary": part.summary,
    }))
}

fn running_effect_entry(
    session_id: &str,
    action: &ToolAction,
    observed_at: u64,
    input_entry_id: Option<String>,
) -> Value {
    let (tool_name, detail) = effect_detail(action);
    json!({
        "type": "effect",
        "id": format!("effect-{}", action.action_id),
        "sessionId": session_id,
        "turnId": action.turn_id,
        "createdAt": observed_at,
        "updatedAt": observed_at,
        "generationStatus": "in_progress",
        "relatedEntryId": null,
        "inputEntryId": input_entry_id,
        "title": tool_name,
        "detail": detail,
        "state": {"status": "running", "outputText": ""},
    })
}

fn effect_detail(action: &ToolAction) -> (String, Value) {
    let name = action.tool_name.clone();
    if name == "subagent.spawn" {
        let argument = |key: &str| action.arguments.get(key).and_then(Value::as_str);
        let agent_name = argument("agentName").unwrap_or("subagent");
        let agent_type = argument("agentType").unwrap_or("generic");
        let message = argument("message").unwrap_or_default();
        return (
            name.clone(),
            json!({
                "kind": "subagent",
                "toolName": name,
                "input": {"task": message, "agent": agent_type},
                "childSessionId": null,
                "agentName": agent_name,
                "display": {
                    "summary": format!("Starting {agent_name}"),
                    "verb": "Starting",
                    "message": agent_name,
                    "settledVerb": "Started",
                    "settledMessage": format!("Started {agent_name}"),
                    "statusText": format!("Starting {agent_name}"),
                },
            }),
        );
    }
    (
        name.clone(),
        json!({
            "kind": "tool",
            "toolName": name,
            "input": action.arguments,
            "display": {
                "summary": name,
                "verb": "Running",
                "message": name,
                "settledVerb": "Ran",
                "settledMessage": name,
                "statusText": format!("Running {name}"),
            },
        }),
    )
}

fn completed_effect_entry(
    session_id: &str,
    entries: &[Value],
    turn_id: &str,
    action_id: &str,
    result: &ToolResult,
    observed_at: u64,
    input_entry_id: Option<&str>,
) -> Value {
    let entry_id = format!("effect-{action_id}");
    let existing = entries.iter().find(|entry| entry["id"] == json!(entry_id));
    let output_text = result
        .content()
        .iter()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let serialized = result.to_json();
    let mut state = match result {
        ToolResult::Failure { code, message, .. } if code == "tool_skipped" => {
            let reason = if output_text.is_empty() {
                message.clone()
            } else {
                output_text.clone()
            };
            json!({
                "status": "skipped",
                "reason": reason,
                "display": {"success": false, "message": reason},
            })
        }
        ToolResult::Failure {
            code,
            message,
            details,
            ..
        } => json!({
            "status": "failed",
            "error": {"code": code, "message": message, "details": details},
            "output": serialized,
            "outputText": output_text,
            "display": {"success": false, "message": message},
        }),
        ToolResult::Success { meta, .. } => {
            let title = existing
                .and_then(|entry| entry["title"].as_str())
                .unwrap_or("tool");
            let message = existing
                .and_then(|entry| entry["detail"]["display"]["settledMessage"].as_str())
                .map_or_else(|| format!("{title} completed"), str::to_owned);
            let mut display = json!({"success": true, "message": message});
            if let Some(note) = meta
                .as_ref()
                .and_then(|meta| meta.get("approval_note"))
                .and_then(Value::as_str)
                .filter(|note| !note.is_empty())
            {
                display["approvalNote"] = json!(note);
            }
            json!({
                "status": "completed",
                "output": serialized,
                "outputText": output_text,
                "display": display,
            })
        }
    };
    if let Some(approval) = result
        .meta()
        .and_then(|meta| meta.get("approval"))
        .filter(|approval| {
            ["decision", "approvalType", "approvalSource"]
                .iter()
                .all(|key| approval[*key].is_string())
        })
    {
        for key in ["decision", "approvalType", "approvalSource"] {
            state[key] = approval[key].clone();
        }
    }
    if let Some(existing) = existing {
        let mut updated = existing.clone();
        updated["updatedAt"] = json!(observed_at);
        updated["generationStatus"] = json!("completed");
        updated["detail"] = completed_effect_detail(existing, result.meta());
        updated["state"] = state;
        return updated;
    }
    let message = state["display"]["message"]
        .as_str()
        .unwrap_or("tool completed")
        .to_owned();
    json!({
        "type": "effect",
        "id": entry_id,
        "sessionId": session_id,
        "turnId": turn_id,
        "createdAt": observed_at,
        "updatedAt": observed_at,
        "generationStatus": "completed",
        "relatedEntryId": null,
        "inputEntryId": input_entry_id,
        "title": "tool",
        "detail": {
            "kind": "tool",
            "toolName": "tool",
            "input": null,
            "display": {"summary": "tool", "statusText": "Running tool", "settledMessage": message},
        },
        "state": state,
    })
}

fn completed_effect_detail(existing: &Value, meta: Option<&Map<String, Value>>) -> Value {
    let detail = existing.get("detail").cloned().unwrap_or_else(|| json!({}));
    if detail["kind"] != "subagent" {
        return detail;
    }
    let Some(child) = meta
        .and_then(|meta| meta.get("mistral.vibe.subagent"))
        .and_then(|annotation| annotation["childSessionId"].as_str())
        .filter(|child| !child.is_empty())
    else {
        return detail;
    };
    let mut completed = detail;
    completed["childSessionId"] = json!(child);
    completed
}

#[allow(clippy::too_many_arguments)]
fn terminal_compaction_entry(
    session_id: &str,
    entries: &[Value],
    compaction_id: &str,
    turn_id: Option<&str>,
    observed_at: u64,
    input_entry_id: Option<&str>,
    message: &str,
    details: Value,
) -> Value {
    let entry_id = format!("checkpoint-compaction-{compaction_id}");
    let existing = entries.iter().find(|entry| entry["id"] == json!(entry_id));
    let created_at = existing
        .and_then(|entry| entry["createdAt"].as_u64())
        .unwrap_or(observed_at);
    let input = existing
        .and_then(|entry| entry["inputEntryId"].as_str())
        .or(input_entry_id);
    json!({
        "type": "checkpoint",
        "id": entry_id,
        "sessionId": session_id,
        "turnId": turn_id,
        "createdAt": created_at,
        "updatedAt": observed_at,
        "generationStatus": "completed",
        "relatedEntryId": null,
        "inputEntryId": input,
        "kind": "compaction",
        "message": message,
        "details": details,
    })
}

/// Reference `_running_compaction_entry`.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn running_compaction_entry(
    session_id: &str,
    entries: &[Value],
    compaction_id: &str,
    turn_id: Option<&str>,
    trigger: &str,
    attempt: u64,
    observed_at: u64,
    input_entry_id: Option<&str>,
) -> Value {
    let entry_id = format!("checkpoint-compaction-{compaction_id}");
    let created_at = entries
        .iter()
        .find(|entry| entry["id"] == json!(entry_id))
        .and_then(|entry| entry["createdAt"].as_u64())
        .unwrap_or(observed_at);
    json!({
        "type": "checkpoint",
        "id": entry_id,
        "sessionId": session_id,
        "turnId": turn_id,
        "createdAt": created_at,
        "updatedAt": observed_at,
        "generationStatus": "in_progress",
        "relatedEntryId": null,
        "inputEntryId": input_entry_id,
        "kind": "compaction",
        "message": "Compacting context",
        "details": {"trigger": trigger, "attempt": attempt},
    })
}

/// Reference `public_notice_entry`.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn public_notice_entry(
    session_id: &str,
    entry_id: &str,
    kind: &str,
    scope: &str,
    observed_at: u64,
    fields: &[(&str, Option<&str>)],
    status: Option<&str>,
    content: Option<&str>,
) -> Value {
    let mut detail = json!({"kind": kind, "scope": scope});
    let mut hook_name = None;
    for (key, value) in fields {
        if let Some(value) = value {
            detail[*key] = json!(value);
            if *key == "hookName" {
                hook_name = Some(*value);
            }
        }
    }
    if let Some(status) = status {
        detail["status"] = json!(status);
    }
    if let Some(content) = content {
        detail["content"] = json!(content);
    }
    let level = match status {
        Some("warning") => "warning",
        Some("error") => "error",
        _ => "info",
    };
    let message = content.map_or_else(
        || match (kind, hook_name) {
            ("hook_run_started", _) => "Running hooks".to_owned(),
            ("hook_run_completed", _) => "Hooks completed".to_owned(),
            ("hook_started", Some(name)) => format!("Running hook {name}"),
            ("hook_started", None) => "Running hook".to_owned(),
            (_, Some(name)) => format!("Hook {name} completed"),
            (_, None) => "Hook completed".to_owned(),
        },
        str::to_owned,
    );
    json!({
        "type": "notice",
        "id": entry_id,
        "sessionId": session_id,
        "turnId": null,
        "createdAt": observed_at,
        "updatedAt": observed_at,
        "generationStatus": "completed",
        "level": level,
        "relatedEntryId": null,
        "inputEntryId": null,
        "message": message,
        "detail": detail,
    })
}

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

/// Whether an entry is streamed completion content awaiting its commit.
#[must_use]
pub fn is_provisional(entry: &Value) -> bool {
    entry["generationStatus"] == "in_progress"
        && (entry["type"] == "reasoning"
            || (entry["type"] == "message" && entry["role"] == "assistant"))
}

fn belongs_to_action(entry: &Value, action_id: &str) -> bool {
    entry["id"].as_str().is_some_and(|id| {
        id == format!("assistant-{action_id}") || id.starts_with(&format!("reasoning-{action_id}-"))
    })
}

fn settle_provisional(entry: &mut Value, observed_at: u64, cause: &str) {
    entry["updatedAt"] = json!(observed_at);
    entry["generationStatus"] = json!("completed");
    entry["outcome"] = json!({"type": "discarded", "cause": cause});
}

fn is_open_effect(entry: &Value, turn_id: &str) -> bool {
    entry["type"] == "effect"
        && entry["turnId"] == json!(turn_id)
        && entry["generationStatus"] != "completed"
}

fn settle_open_effects(
    entries: &mut [Value],
    turn_id: &str,
    observed_at: u64,
    cancelled: bool,
    reason: &str,
) {
    for entry in entries.iter_mut() {
        if is_open_effect(entry, turn_id) {
            let output_text = entry["state"]["outputText"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            entry["state"] = if cancelled {
                json!({
                    "status": "cancelled",
                    "reason": reason,
                    "outputText": output_text,
                    "display": {"success": false, "message": reason},
                })
            } else {
                json!({
                    "status": "failed",
                    "error": {"message": reason},
                    "outputText": output_text,
                    "display": {"success": false, "message": reason},
                })
            };
            entry["updatedAt"] = json!(observed_at);
            entry["generationStatus"] = json!("completed");
        } else if is_provisional(entry) && entry["turnId"] == json!(turn_id) {
            settle_provisional(
                entry,
                observed_at,
                if cancelled { "interrupt" } else { "failure" },
            );
        }
    }
}

fn merge_provisional(
    entries: &mut [Value],
    entry_id: &str,
    observed_at: u64,
    extend: impl FnOnce(&mut Value),
) -> Option<()> {
    let entry = entries
        .iter_mut()
        .find(|entry| entry["id"] == json!(entry_id))?;
    if is_provisional(entry) {
        extend(entry);
        entry["updatedAt"] = json!(observed_at);
    }
    Some(())
}

fn upsert(entries: &mut Vec<Value>, entry: Value) {
    match entries
        .iter_mut()
        .find(|existing| existing["id"] == entry["id"])
    {
        Some(existing) => *existing = entry,
        None => entries.push(entry),
    }
}

fn count_checkpoints(entries: &[Value], kind: &str) -> usize {
    entries
        .iter()
        .filter(|entry| entry["type"] == "checkpoint" && entry["kind"] == json!(kind))
        .count()
}

fn active_input_entry_id(latest_turn: Option<&HarnessTurn>, turn_id: &str) -> Option<String> {
    latest_turn
        .filter(|turn| turn.in_progress_for(turn_id))
        .and_then(|turn| turn.input_entry_id.clone())
}

fn steered_input_entry_id(
    entries: &[Value],
    turn_id: &str,
    active: Option<&str>,
    client_message_id: Option<&str>,
) -> Option<String> {
    let is_steering = |entry: &Value| {
        entry["type"] == "message"
            && entry["role"] == "user"
            && entry["source"] == "turn_steer"
            && entry["turnId"] == json!(turn_id)
    };
    if let Some(id) = client_message_id
        && entries
            .iter()
            .any(|entry| entry["id"] == json!(id) && is_steering(entry))
    {
        return Some(id.to_owned());
    }
    let mut after = active.is_none();
    for entry in entries {
        if active.is_some() && entry["id"].as_str() == active {
            after = true;
            continue;
        }
        if after && is_steering(entry) {
            return entry["id"].as_str().map(str::to_owned);
        }
    }
    None
}

fn started_at(latest_turn: Option<&HarnessTurn>, turn_id: &str, fallback: u64) -> u64 {
    latest_turn
        .filter(|turn| turn.in_progress_for(turn_id))
        .map_or(fallback, |turn| turn.started_at)
}

fn settled_turn(
    latest_turn: Option<&HarnessTurn>,
    session_id: &str,
    turn_id: &str,
    status: TurnStatus,
    started_at: u64,
    completed_at: u64,
) -> HarnessTurn {
    let active = latest_turn.filter(|turn| turn.in_progress_for(turn_id));
    HarnessTurn {
        id: turn_id.to_owned(),
        session_id: session_id.to_owned(),
        status,
        queue_item_id: active.and_then(|turn| turn.queue_item_id.clone()),
        input_entry_id: active.and_then(|turn| turn.input_entry_id.clone()),
        started_at,
        completed_at: Some(completed_at),
        error: None,
        reason: None,
    }
}

/// Reference `first_user_message_preview`.
#[must_use]
pub fn first_user_message_preview(entries: &[Value]) -> String {
    entries
        .iter()
        .filter(|entry| entry["type"] == "message" && entry["role"] == "user")
        .map(|entry| {
            let parts: Vec<&str> = entry["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|block| block["text"].as_str())
                        .collect()
                })
                .unwrap_or_default();
            text_preview(&parts)
        })
        .find(|preview| !preview.is_empty())
        .unwrap_or_default()
}

fn content_preview(content: &[Value]) -> String {
    let parts: Vec<&str> = content
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    text_preview(&parts)
}

/// Reference `_text_preview`: joined, trimmed, on one line and cut at 200
/// characters with an ellipsis.
#[must_use]
pub fn text_preview(parts: &[&str]) -> String {
    let text = parts.join("\n").trim().replace('\n', " ");
    if text.chars().count() > PREVIEW_MAX_CHARS {
        let cut: String = text.chars().take(PREVIEW_MAX_CHARS).collect();
        return format!("{}…", cut.trim_end());
    }
    text
}

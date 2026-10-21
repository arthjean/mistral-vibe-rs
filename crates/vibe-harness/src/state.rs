//! The public session state the Unified Harness republishes after every step,
//! and the projector that derives each one from the step's observations.
//!
//! Reference `SessionProjector` and the entry builders beside it
//! (`harness/runtimes/python/python/mistralai_vibe_local_harness/vibe/_projection.py`),
//! over the `harness.public-session-state/v1` shapes of
//! `mistralai_vibe_local_harness/session_protocol.py`. History entries stay
//! JSON objects: the adapter re-projects them into the wire's typed entries
//! (`crate::wire`), and the store writes them as they are.
//!
//! This module holds the snapshot itself; what a step commits is in
//! `observation`, the projector folding it in is in `projector`, and the
//! entries it writes are built in `entries`.

use serde_json::{Value, json};

mod entries;
mod observation;
mod projector;

pub use entries::{
    is_provisional, public_message_entry, public_notice_entry, running_compaction_entry,
};
pub use observation::{
    CompletionDelta, Observation, ReasoningPart, ToolAction, ToolResult, TransitionTurn,
};
pub use projector::{Advance, Projector, settle_stalled};

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
    vibe_core::pycompat::clip(&text, PREVIEW_MAX_CHARS)
}

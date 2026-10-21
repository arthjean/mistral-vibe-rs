//! The public history a client reads: entries, turns, effects and callbacks,
//! in the vocabulary reference `vibe/app_server` publishes them in.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{CallbackDetail, CallbackOutput, EffectDetail, EffectResultDisplay, NoticeDetail};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicEntryMetadata {
    pub id: String,
    pub session_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    /// The user entry the turn answers. Reference `_PublicHistoryEntryBase`
    /// publishes it since v2.26.0, and only its Unified Harness fills it.
    #[serde(default)]
    pub input_entry_id: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub generation_status: PublicEntryGenerationStatus,
    #[serde(default)]
    pub related_entry_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicEntryGenerationStatus {
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicMessageRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicMessageSource {
    TurnStart,
    TurnSteer,
    Harness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PublicContentBlock {
    Text { text: String },
    Image { attachment: Value },
    Resource { resource: Value },
}

/// Reference `UserResource` as pydantic dumps it back: every field its kind
/// declares, in declaration order, an absent one as `null`. A value naming no
/// kind the reference reads is kept as sent.
#[must_use]
pub fn canonical_user_resource(resource: &Value) -> Value {
    let Some(object) = resource.as_object() else {
        return resource.clone();
    };
    let declared: &[&str] = match object.get("kind").and_then(Value::as_str) {
        Some("text") => &["text"],
        Some("blob") => &["blob"],
        Some("link") => &["name", "title", "description", "size"],
        _ => return resource.clone(),
    };
    let mut canonical = serde_json::Map::new();
    for key in ["uri", "mediaType", "kind"].iter().chain(declared) {
        let value = object
            .get(*key)
            .or_else(|| {
                (*key == "mediaType")
                    .then(|| object.get("media_type"))
                    .flatten()
            })
            .cloned()
            .unwrap_or(Value::Null);
        canonical.insert((*key).to_owned(), value);
    }
    Value::Object(canonical)
}

/// The text a message carrying `resources` sends the model: what the operator
/// typed, then each resource. Reference `decode_content_blocks` and
/// `render_user_resources` (`vibe/app_server/_utils.py`, `vibe/user_content.py`).
#[must_use]
pub fn prompt_with_resources(input_text: &str, resources: &[&Value]) -> String {
    let rendered = resources
        .iter()
        .map(|resource| render_user_resource(resource))
        .collect::<Vec<_>>()
        .join("\n\n");
    [input_text, rendered.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_user_resource(resource: &Value) -> String {
    let field = |key: &str| resource.get(key).filter(|value| !value.is_null());
    let text = |value: &Value| match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let uri = field("uri").map(text).unwrap_or_default();
    match resource.get("kind").and_then(Value::as_str) {
        Some("text") => format!(
            "path: {uri}\ncontent: {}",
            field("text").map(text).unwrap_or_default()
        ),
        Some("blob") => format!(
            "path: {uri}\ncontent (base64): {}",
            field("blob").map(text).unwrap_or_default()
        ),
        _ => [
            ("uri", "uri"),
            ("name", "name"),
            ("title", "title"),
            ("description", "description"),
            ("mime_type", "mediaType"),
            ("size", "size"),
        ]
        .into_iter()
        .filter_map(|(label, key)| field(key).map(|value| format!("{label}: {}", text(value))))
        .collect::<Vec<_>>()
        .join("\n"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PublicEffectState {
    Pending,
    Running {
        #[serde(default)]
        output_text: String,
    },
    Blocked {
        callback_id: String,
        #[serde(default)]
        output_text: String,
    },
    Completed {
        #[serde(default)]
        output: Value,
        #[serde(default)]
        output_text: String,
        #[serde(default, deserialize_with = "whole_millis")]
        duration_ms: u64,
        display: EffectResultDisplay,
        #[serde(flatten)]
        approval: EffectApproval,
    },
    Failed {
        error: PublicError,
        /// Reference `FailedEffectState.output`, which no failure fills.
        #[serde(default)]
        output: Value,
        #[serde(default)]
        output_text: String,
        #[serde(default, deserialize_with = "whole_millis")]
        duration_ms: u64,
        display: EffectResultDisplay,
        #[serde(flatten)]
        approval: EffectApproval,
    },
    Cancelled {
        reason: String,
        #[serde(default)]
        output_text: String,
        #[serde(default, deserialize_with = "whole_millis")]
        duration_ms: u64,
        /// A cancellation that produced no result carries no display, which is
        /// the one settled state the reference lets publish a null one.
        #[serde(default)]
        display: Option<EffectResultDisplay>,
        #[serde(flatten)]
        approval: EffectApproval,
    },
    Skipped {
        reason: String,
        display: EffectResultDisplay,
        #[serde(flatten)]
        approval: EffectApproval,
    },
}

/// How the permission gate settled a call, as every settled state publishes
/// it: the three fields of reference `CompletedEffectState` and its siblings,
/// each null when no gate answered (a replayed call, or one the turn closed
/// under).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectApproval {
    #[serde(default)]
    pub decision: Option<crate::tools::ToolVerdict>,
    #[serde(default)]
    pub approval_type: Option<crate::tools::ToolApprovalType>,
    #[serde(default)]
    pub approval_source: Option<crate::tools::ToolApprovalSource>,
}

impl EffectApproval {
    /// Whether no gate answered the call.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// The gate's whole answer, when it reached one.
    #[must_use]
    pub fn complete(&self) -> Option<crate::tools::ToolApproval> {
        Some(crate::tools::ToolApproval {
            decision: self.decision?,
            approval_type: self.approval_type?,
            approval_source: self.approval_source?,
        })
    }
}

impl From<Option<crate::tools::ToolApproval>> for EffectApproval {
    fn from(approval: Option<crate::tools::ToolApproval>) -> Self {
        approval.map_or_else(Self::default, |approval| Self {
            decision: Some(approval.decision),
            approval_type: Some(approval.approval_type),
            approval_source: Some(approval.approval_source),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicError {
    pub message: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub details: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicTurnStatus {
    InProgress,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicTurnStopReason {
    Limit,
}

/// Why a turn failed, in the vocabulary a client branches on.
///
/// A failing turn always carries one of these: the message explains, the code
/// is what a UI can act on, so it is classified from the failure's type rather
/// than from the text it rendered to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnErrorCode {
    RateLimit,
    ContextTooLong,
    ResponseTooLong,
    Refusal,
    InvalidImageAttachment,
    ImagesNotSupported,
    CompactionFailed,
    IncompleteStream,
    BackendError,
    InvalidModel,
    InvalidApiKey,
    InternalError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicTurn {
    pub id: String,
    pub session_id: String,
    pub status: PublicTurnStatus,
    /// The user entry the turn answers. Reference `PublicTurn` publishes it
    /// since v2.26.0, and only its Unified Harness fills it.
    #[serde(default)]
    pub input_entry_id: Option<String>,
    pub started_at: u64,
    #[serde(default)]
    pub completed_at: Option<u64>,
    #[serde(default)]
    pub error: Option<PublicError>,
    #[serde(default)]
    pub stop_reason: Option<PublicTurnStopReason>,
    /// The queued input the turn was started from. Reference `PublicTurn`
    /// always publishes it, and this port starts no turn from a queue.
    #[serde(default)]
    pub queue_item_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PublicCallbackState {
    Open,
    Answered { output: CallbackOutput },
    Cancelled { reason: String },
    Expired { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicNoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PublicHistoryEntry {
    Message {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        role: PublicMessageRole,
        content: Vec<PublicContentBlock>,
        #[serde(default)]
        source: Option<PublicMessageSource>,
        #[serde(default)]
        user_display_content: Option<Value>,
    },
    Reasoning {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        text: String,
        #[serde(default)]
        summary: Vec<String>,
    },
    Effect {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        title: String,
        /// Boxed because the call and result displays it carries are what make
        /// an effect entry three times the size of every other variant.
        detail: Box<EffectDetail>,
        state: PublicEffectState,
        /// Which model tool call this effect projects.
        ///
        /// The reference does not publish it and a conforming client rejects a
        /// surplus field, so it stays off the wire and lives here, where the
        /// reducer correlates a stream chunk or a result with the call it
        /// belongs to.
        #[serde(skip)]
        tool_call_id: String,
    },
    Callback {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        callback_id: String,
        title: String,
        detail: CallbackDetail,
        state: PublicCallbackState,
    },
    Checkpoint {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        kind: String,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        details: Value,
    },
    Notice {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        level: PublicNoticeLevel,
        message: String,
        detail: NoticeDetail,
    },
}

impl PublicHistoryEntry {
    #[must_use]
    pub fn metadata(&self) -> &PublicEntryMetadata {
        match self {
            Self::Message { metadata, .. }
            | Self::Reasoning { metadata, .. }
            | Self::Effect { metadata, .. }
            | Self::Callback { metadata, .. }
            | Self::Checkpoint { metadata, .. }
            | Self::Notice { metadata, .. } => metadata,
        }
    }

    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.metadata().generation_status == PublicEntryGenerationStatus::Completed
    }

    pub fn rebind_session(&mut self, session_id: impl Into<String>) {
        self.metadata_mut().session_id = session_id.into();
    }

    /// The fields every entry carries, to change in place.
    pub fn metadata_mut(&mut self) -> &mut PublicEntryMetadata {
        match self {
            Self::Message { metadata, .. }
            | Self::Reasoning { metadata, .. }
            | Self::Effect { metadata, .. }
            | Self::Callback { metadata, .. }
            | Self::Checkpoint { metadata, .. }
            | Self::Notice { metadata, .. } => metadata,
        }
    }
}

/// A duration in milliseconds, which the Unified Harness publishes as a
/// float; this port keeps whole milliseconds.
fn whole_millis<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let millis = f64::deserialize(deserializer)?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(millis.max(0.0).round() as u64)
}

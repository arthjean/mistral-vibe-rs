//! The parameters the unified handlers read.
//!
//! A request reaches one of these handlers once the reference schema has
//! validated it (`wire_validation::validate_method`, in `connection::route`),
//! so each shape reads the validated value as it stands: a field the schema
//! requires is required here too, a default is the reference model's own,
//! and a field no handler reads is left out. The models are those of
//! `vibe/app_server/protocol.py`.

use serde::Deserialize;
use serde_json::{Map, Value};

/// Reference `history_limit` of `SessionOpenParams` and `SessionForkParams`.
const fn default_history_limit() -> usize {
    200
}

/// Reference `SessionListParams.limit`.
const fn default_list_limit() -> usize {
    50
}

const fn default_true() -> bool {
    true
}

/// Reference `SessionListParams`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListParams {
    #[serde(default = "default_list_limit")]
    pub(super) limit: usize,
    pub(super) cwd: Option<String>,
    #[serde(default)]
    pub(super) include_archived: bool,
    pub(super) pinned: Option<bool>,
}

/// Reference `SessionResumeParams`, and `SessionContinueParams` once the
/// session it continues is named.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ResumeParams {
    pub(super) session_id: String,
    #[serde(default)]
    pub(super) agent_config: Map<String, Value>,
    #[serde(default = "default_history_limit")]
    pub(super) history_limit: usize,
}

/// Reference `SessionForkParams`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ForkParams {
    pub(super) source_session_id: String,
    pub(super) entry_id: Option<String>,
    #[serde(default = "default_history_limit")]
    pub(super) history_limit: usize,
    #[serde(default = "default_true")]
    pub(super) attach: bool,
}

/// Reference `SessionReadParams`, as far as the unified read pages it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReadParams {
    pub(super) session_id: String,
    pub(super) history: Option<HistoryPage>,
}

/// Reference `PageRequest`, for its limit.
#[derive(Deserialize)]
pub(super) struct HistoryPage {
    pub(super) limit: Option<usize>,
}

/// Reference `SessionPinParams`, `SessionArchiveParams` and
/// `SessionMarkAsSeenParams`, whose flag the method decides between.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MarkParams {
    pub(super) session_id: String,
    #[serde(default)]
    pub(super) pinned: bool,
    #[serde(default)]
    pub(super) archived: bool,
}

/// Reference `SessionTitleUpdateParams`, which `session/rename` reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RenameParams {
    pub(super) session_id: String,
    pub(super) title: String,
}

/// Reference `ContextInjectParams`, its content kept as sent.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct InjectParams {
    pub(super) input: Vec<Value>,
    #[serde(default)]
    pub(super) as_message: bool,
}

/// Reference `CallbackResultParams`: the result as sent, which a repeated
/// answer is compared against, and what it says.
#[derive(Deserialize)]
#[serde(try_from = "SentCallbackResult")]
pub(in crate::server) struct CallbackResultParams {
    pub(super) sent: Value,
    pub(in crate::server) callback_id: String,
    pub(in crate::server) output: Option<Value>,
    pub(in crate::server) error: Option<CallbackResultError>,
}

/// Reference `CallbackResultError`, for its message.
#[derive(Deserialize)]
pub(in crate::server) struct CallbackResultError {
    pub(in crate::server) message: String,
}

#[derive(Deserialize)]
struct SentCallbackResult {
    result: Value,
}

/// Reference `CallbackResult`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallbackResult {
    callback_id: String,
    output: Option<Value>,
    error: Option<CallbackResultError>,
}

impl TryFrom<SentCallbackResult> for CallbackResultParams {
    type Error = serde_json::Error;

    fn try_from(sent: SentCallbackResult) -> Result<Self, Self::Error> {
        let result = CallbackResult::deserialize(&sent.result)?;
        Ok(Self {
            sent: sent.result,
            callback_id: result.callback_id,
            output: result.output,
            error: result.error,
        })
    }
}

/// Reference `LoopsCreateParams`.
#[derive(Deserialize)]
pub(super) struct LoopCreateParams {
    pub(super) interval: String,
    pub(super) prompt: String,
}

/// Reference `LoopsDeleteParams`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LoopDeleteParams {
    pub(super) loop_id: String,
}

/// Reference `BackgroundProcessOutputParams`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProcessOutputParams {
    pub(super) session_id: String,
    pub(super) process_id: String,
    pub(super) from_end: bool,
    pub(super) cursor: u64,
    pub(super) wait_ms: u64,
    pub(super) max_bytes: u64,
}

/// Reference `BackgroundProcessStopParams`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProcessStopParams {
    pub(super) session_id: String,
    pub(super) process_id: String,
}

/// Reference `TurnEnqueueParams` and `TurnQueueReplaceParams`, the item
/// replaced named by the second only.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct QueueInputParams {
    pub(super) entries: Vec<Value>,
    pub(super) idempotency_key: Option<String>,
    pub(super) queue_item_id: Option<String>,
}

/// Reference `TurnQueueRemoveParams`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct QueueItemParams {
    pub(super) queue_item_id: String,
}

/// Reference `TurnQueueSteerParams`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct QueueSteerParams {
    pub(super) queue_item_id: String,
    pub(super) expected_turn_id: String,
}

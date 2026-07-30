use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    pub session_id: String,
    #[serde(skip)]
    pub turn_id: Option<String>,
    #[serde(skip)]
    pub emitted_at: u64,
    pub event_id: u64,
    #[serde(flatten)]
    pub event: EngineEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    UserMessage {
        content: String,
    },
    UserSteer {
        content: String,
    },
    ContextInjected {
        content: String,
        as_message: bool,
    },
    ModelText {
        text: String,
    },
    ModelReasoning {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    ToolStream {
        call_id: String,
        chunk: String,
    },
    ToolResult {
        call_id: String,
        content: String,
        #[serde(default)]
        typed_result: Value,
        #[serde(default)]
        display: Value,
        #[serde(default)]
        duration_ms: u64,
        is_error: bool,
        #[serde(default)]
        cancelled: bool,
    },
    CallbackRequested {
        callback_id: String,
        kind: CallbackKind,
        prompt: String,
    },
    CallbackResolved {
        callback_id: String,
        accepted: bool,
        #[serde(default)]
        value: Option<String>,
    },
    Hook {
        name: String,
        message: String,
    },
    Compaction {
        summary: String,
    },
    Title {
        title: String,
    },
    SessionHandoff {
        from_session_id: String,
        to_session_id: String,
    },
    Lifecycle {
        state: LifecycleState,
        #[serde(default)]
        message: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallbackKind {
    Approval,
    UserInput,
    ConnectorAuth,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    #[default]
    Idle,
    Running,
    WaitingCallback,
    Completed,
    Failed,
    Cancelled,
}

impl LifecycleState {
    fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::WaitingCallback)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicEntryMetadata {
    pub id: String,
    pub session_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
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
        #[serde(default)]
        duration_ms: u64,
        #[serde(default)]
        display: Value,
    },
    Failed {
        error: PublicError,
        #[serde(default)]
        output_text: String,
        #[serde(default)]
        duration_ms: u64,
        #[serde(default)]
        display: Value,
    },
    Cancelled {
        reason: String,
        #[serde(default)]
        output_text: String,
        #[serde(default)]
        duration_ms: u64,
        #[serde(default)]
        display: Option<Value>,
    },
    Skipped {
        reason: String,
        #[serde(default)]
        display: Value,
    },
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicTurn {
    pub id: String,
    pub session_id: String,
    pub status: PublicTurnStatus,
    pub started_at: u64,
    #[serde(default)]
    pub completed_at: Option<u64>,
    #[serde(default)]
    pub error: Option<PublicError>,
    #[serde(default)]
    pub stop_reason: Option<PublicTurnStopReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PublicCallbackState {
    Open,
    Answered { output: Value },
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
        detail: Value,
        state: PublicEffectState,
    },
    Callback {
        #[serde(flatten)]
        metadata: PublicEntryMetadata,
        callback_id: String,
        title: String,
        detail: Value,
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
        detail: Value,
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

    fn metadata_mut(&mut self) -> &mut PublicEntryMetadata {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ModelMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(default)]
        reasoning: Option<String>,
        #[serde(default)]
        reasoning_signature: Option<String>,
        #[serde(default)]
        reasoning_state: Vec<String>,
        #[serde(default)]
        tool_calls: Vec<ModelToolCall>,
    },
    Tool {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionSnapshot {
    pub session_id: String,
    #[serde(skip)]
    pub turn_id: Option<String>,
    pub watermark: u64,
    pub lifecycle: LifecycleState,
    pub title: Option<String>,
    pub history: Vec<PublicHistoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionReducer {
    state: ProjectionSnapshot,
}

impl ProjectionReducer {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            state: ProjectionSnapshot {
                session_id: session_id.into(),
                turn_id: None,
                watermark: 0,
                lifecycle: LifecycleState::Idle,
                title: None,
                history: Vec::new(),
            },
        }
    }

    #[must_use]
    pub fn for_turn(session_id: impl Into<String>, turn_id: impl Into<String>) -> Self {
        let mut reducer = Self::new(session_id);
        reducer.state.turn_id = Some(turn_id.into());
        reducer
    }

    #[must_use]
    pub fn state(&self) -> &ProjectionSnapshot {
        &self.state
    }

    pub fn apply(&mut self, envelope: &EventEnvelope) -> Result<ApplyOutcome, ProjectionError> {
        if envelope.event_id == 0 {
            return Err(ProjectionError::InvalidEventId);
        }
        if envelope.session_id != self.state.session_id {
            return Err(ProjectionError::ForeignSession {
                expected: self.state.session_id.clone(),
                actual: envelope.session_id.clone(),
            });
        }
        if envelope.event_id <= self.state.watermark {
            return Ok(ApplyOutcome::Duplicate);
        }
        let expected = self.state.watermark.saturating_add(1);
        if envelope.event_id != expected {
            return Err(ProjectionError::Gap {
                expected,
                actual: envelope.event_id,
            });
        }

        let mut next = self.state.clone();
        reduce_event(
            &mut next,
            envelope.event_id,
            envelope.emitted_at,
            &envelope.event,
        )?;
        next.watermark = envelope.event_id;
        self.state = next;
        Ok(ApplyOutcome::Applied)
    }

    pub fn restore(
        &mut self,
        snapshot: ProjectionSnapshot,
        expected_watermark: u64,
    ) -> Result<(), ProjectionError> {
        if snapshot.session_id != self.state.session_id {
            return Err(ProjectionError::ForeignSession {
                expected: self.state.session_id.clone(),
                actual: snapshot.session_id,
            });
        }
        if snapshot.watermark != expected_watermark {
            return Err(ProjectionError::WatermarkMismatch {
                expected: expected_watermark,
                actual: snapshot.watermark,
            });
        }
        self.state = snapshot;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    Duplicate,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProjectionError {
    #[error("event IDs must be positive")]
    InvalidEventId,
    #[error("event gap: expected {expected}, received {actual}")]
    Gap { expected: u64, actual: u64 },
    #[error("foreign session: expected `{expected}`, received `{actual}`")]
    ForeignSession { expected: String, actual: String },
    #[error("snapshot watermark mismatch: expected {expected}, received {actual}")]
    WatermarkMismatch { expected: u64, actual: u64 },
    #[error("illegal lifecycle transition from {from:?} through `{event}`")]
    IllegalTransition {
        from: LifecycleState,
        event: &'static str,
    },
    #[error("callback `{0}` is not pending")]
    CallbackNotPending(String),
    #[error("handoff source `{actual}` does not match current session `{expected}`")]
    InvalidHandoff { expected: String, actual: String },
}

fn reduce_event(
    state: &mut ProjectionSnapshot,
    event_id: u64,
    emitted_at: u64,
    event: &EngineEvent,
) -> Result<(), ProjectionError> {
    match event {
        EngineEvent::UserMessage { content } => {
            require_lifecycle(
                state,
                &[LifecycleState::Idle, LifecycleState::Completed],
                "user_message",
            )?;
            state.lifecycle = LifecycleState::Running;
            state.history.push(PublicHistoryEntry::Message {
                metadata: entry_metadata(
                    state,
                    event_id,
                    emitted_at,
                    PublicEntryGenerationStatus::Completed,
                ),
                role: PublicMessageRole::User,
                content: vec![PublicContentBlock::Text {
                    text: content.clone(),
                }],
                source: Some(PublicMessageSource::TurnStart),
                user_display_content: None,
            });
        }
        EngineEvent::UserSteer { content } => {
            require_lifecycle(state, &[LifecycleState::Running], "user_steer")?;
            complete_streaming_entries(state, emitted_at);
            state.history.push(PublicHistoryEntry::Message {
                metadata: entry_metadata(
                    state,
                    event_id,
                    emitted_at,
                    PublicEntryGenerationStatus::Completed,
                ),
                role: PublicMessageRole::User,
                content: vec![PublicContentBlock::Text {
                    text: content.clone(),
                }],
                source: Some(PublicMessageSource::TurnSteer),
                user_display_content: None,
            });
        }
        EngineEvent::ContextInjected {
            content,
            as_message,
        } => {
            require_active(state, "context_injected")?;
            complete_streaming_entries(state, emitted_at);
            let entry = if *as_message {
                PublicHistoryEntry::Message {
                    metadata: entry_metadata(
                        state,
                        event_id,
                        emitted_at,
                        PublicEntryGenerationStatus::Completed,
                    ),
                    role: PublicMessageRole::User,
                    content: vec![PublicContentBlock::Text {
                        text: content.clone(),
                    }],
                    source: Some(PublicMessageSource::Harness),
                    user_display_content: None,
                }
            } else {
                PublicHistoryEntry::Checkpoint {
                    metadata: entry_metadata(
                        state,
                        event_id,
                        emitted_at,
                        PublicEntryGenerationStatus::Completed,
                    ),
                    kind: "context_injected".to_owned(),
                    message: None,
                    details: json!({"content": content}),
                }
            };
            state.history.push(entry);
        }
        EngineEvent::ModelText { text } => {
            require_active(state, "model_text")?;
            let appended = match state.history.last_mut() {
                Some(PublicHistoryEntry::Message {
                    metadata,
                    role: PublicMessageRole::Assistant,
                    content,
                    ..
                }) if metadata.generation_status == PublicEntryGenerationStatus::InProgress => {
                    if let Some(PublicContentBlock::Text { text: current }) = content.first_mut() {
                        current.push_str(text);
                    }
                    metadata.updated_at = emitted_at;
                    true
                }
                _ => false,
            };
            if !appended {
                state.history.push(PublicHistoryEntry::Message {
                    metadata: entry_metadata(
                        state,
                        event_id,
                        emitted_at,
                        PublicEntryGenerationStatus::InProgress,
                    ),
                    role: PublicMessageRole::Assistant,
                    content: vec![PublicContentBlock::Text { text: text.clone() }],
                    source: None,
                    user_display_content: None,
                });
            }
        }
        EngineEvent::ModelReasoning { text, .. } => {
            require_active(state, "model_reasoning")?;
            let appended = match state.history.last_mut() {
                Some(PublicHistoryEntry::Reasoning {
                    metadata,
                    text: current,
                    ..
                }) if metadata.generation_status == PublicEntryGenerationStatus::InProgress => {
                    current.push_str(text);
                    metadata.updated_at = emitted_at;
                    true
                }
                _ => false,
            };
            if !appended {
                state.history.push(PublicHistoryEntry::Reasoning {
                    metadata: entry_metadata(
                        state,
                        event_id,
                        emitted_at,
                        PublicEntryGenerationStatus::InProgress,
                    ),
                    text: text.clone(),
                    summary: Vec::new(),
                });
            }
        }
        EngineEvent::ToolCall {
            call_id,
            name,
            arguments,
        } => {
            require_active(state, "tool_call")?;
            complete_streaming_entries(state, emitted_at);
            state.history.push(PublicHistoryEntry::Effect {
                metadata: entry_metadata(
                    state,
                    event_id,
                    emitted_at,
                    PublicEntryGenerationStatus::InProgress,
                ),
                title: name.clone(),
                detail: json!({
                    "kind": "tool",
                    "toolCallId": call_id,
                    "toolName": name,
                    "arguments": arguments,
                }),
                state: PublicEffectState::Running {
                    output_text: String::new(),
                },
            });
        }
        EngineEvent::ToolStream { call_id, chunk } => {
            require_active(state, "tool_stream")?;
            let entry = state
                .history
                .iter_mut()
                .rev()
                .find(|entry| {
                    matches!(entry, PublicHistoryEntry::Effect { detail, .. }
                        if detail.get("toolCallId").and_then(Value::as_str) == Some(call_id))
                })
                .ok_or(ProjectionError::IllegalTransition {
                    from: state.lifecycle,
                    event: "tool_stream_without_call",
                })?;
            if let PublicHistoryEntry::Effect {
                metadata,
                state: PublicEffectState::Running { output_text },
                ..
            } = entry
            {
                output_text.push_str(chunk);
                metadata.updated_at = emitted_at;
            }
        }
        EngineEvent::ToolResult {
            call_id,
            content,
            typed_result,
            display,
            duration_ms,
            is_error,
            cancelled,
        } => {
            require_active(state, "tool_result")?;
            let entry = state
                .history
                .iter_mut()
                .rev()
                .find(|entry| {
                    matches!(entry, PublicHistoryEntry::Effect { detail, .. }
                        if detail.get("toolCallId").and_then(Value::as_str) == Some(call_id))
                })
                .ok_or(ProjectionError::IllegalTransition {
                    from: state.lifecycle,
                    event: "tool_result_without_call",
                })?;
            if let PublicHistoryEntry::Effect {
                metadata,
                state: current_state,
                ..
            } = entry
            {
                *current_state = if *cancelled {
                    PublicEffectState::Cancelled {
                        reason: content.clone(),
                        output_text: content.clone(),
                        duration_ms: *duration_ms,
                        display: (!display.is_null()).then(|| display.clone()),
                    }
                } else if *is_error {
                    PublicEffectState::Failed {
                        error: PublicError {
                            message: content.clone(),
                            code: Some("tool_failed".to_owned()),
                            details: typed_result.clone(),
                        },
                        output_text: content.clone(),
                        duration_ms: *duration_ms,
                        display: display.clone(),
                    }
                } else {
                    PublicEffectState::Completed {
                        output: if typed_result.is_null() {
                            json!(content)
                        } else {
                            typed_result.clone()
                        },
                        output_text: content.clone(),
                        duration_ms: *duration_ms,
                        display: display.clone(),
                    }
                };
                metadata.updated_at = emitted_at;
                metadata.generation_status = PublicEntryGenerationStatus::Completed;
            }
        }
        EngineEvent::CallbackRequested {
            callback_id,
            kind,
            prompt,
        } => {
            require_lifecycle(state, &[LifecycleState::Running], "callback_requested")?;
            complete_streaming_entries(state, emitted_at);
            state.lifecycle = LifecycleState::WaitingCallback;
            state.history.push(PublicHistoryEntry::Callback {
                metadata: entry_metadata(
                    state,
                    event_id,
                    emitted_at,
                    PublicEntryGenerationStatus::InProgress,
                ),
                callback_id: callback_id.clone(),
                title: prompt.clone(),
                detail: json!({"kind": kind, "prompt": prompt}),
                state: PublicCallbackState::Open,
            });
        }
        EngineEvent::CallbackResolved {
            callback_id,
            accepted,
            value,
        } => {
            require_lifecycle(
                state,
                &[LifecycleState::WaitingCallback],
                "callback_resolved",
            )?;
            let callback = state
                .history
                .iter_mut()
                .rev()
                .find(|entry| {
                    matches!(entry, PublicHistoryEntry::Callback {
                        callback_id: id,
                        state: PublicCallbackState::Open,
                        ..
                    } if id == callback_id)
                })
                .ok_or_else(|| ProjectionError::CallbackNotPending(callback_id.clone()))?;
            if let PublicHistoryEntry::Callback {
                metadata,
                state: callback_state,
                ..
            } = callback
            {
                *callback_state = if *accepted {
                    PublicCallbackState::Answered {
                        output: json!({
                            "accepted": true,
                            "value": value,
                        }),
                    }
                } else {
                    PublicCallbackState::Cancelled {
                        reason: value
                            .clone()
                            .unwrap_or_else(|| "Callback rejected".to_owned()),
                    }
                };
                metadata.updated_at = emitted_at;
                metadata.generation_status = PublicEntryGenerationStatus::Completed;
            }
            state.lifecycle = if *accepted {
                LifecycleState::Running
            } else {
                LifecycleState::Cancelled
            };
        }
        EngineEvent::Hook { name, message } => {
            require_active(state, "hook")?;
            state.history.push(PublicHistoryEntry::Notice {
                metadata: entry_metadata(
                    state,
                    event_id,
                    emitted_at,
                    PublicEntryGenerationStatus::Completed,
                ),
                level: PublicNoticeLevel::Info,
                message: message.clone(),
                detail: json!({
                    "kind": "hook_completed",
                    "hookName": name,
                    "content": message,
                }),
            });
        }
        EngineEvent::Compaction { summary } => {
            require_active(state, "compaction")?;
            complete_streaming_entries(state, emitted_at);
            state.history.push(PublicHistoryEntry::Checkpoint {
                metadata: entry_metadata(
                    state,
                    event_id,
                    emitted_at,
                    PublicEntryGenerationStatus::Completed,
                ),
                kind: "compaction".to_owned(),
                message: Some(summary.clone()),
                details: Value::Null,
            });
        }
        EngineEvent::Title { title } => {
            require_active(state, "title")?;
            state.title = Some(title.clone());
            state.history.push(PublicHistoryEntry::Notice {
                metadata: entry_metadata(
                    state,
                    event_id,
                    emitted_at,
                    PublicEntryGenerationStatus::Completed,
                ),
                level: PublicNoticeLevel::Info,
                message: title.clone(),
                detail: json!({
                    "kind": "session_title_updated",
                    "title": title,
                }),
            });
        }
        EngineEvent::SessionHandoff {
            from_session_id,
            to_session_id,
        } => {
            require_active(state, "session_handoff")?;
            if from_session_id != &state.session_id {
                return Err(ProjectionError::InvalidHandoff {
                    expected: state.session_id.clone(),
                    actual: from_session_id.clone(),
                });
            }
            state.session_id.clone_from(to_session_id);
            for entry in &mut state.history {
                entry.metadata_mut().session_id.clone_from(to_session_id);
            }
        }
        EngineEvent::Lifecycle {
            state: next,
            message,
        } => {
            validate_lifecycle_transition(state.lifecycle, *next)?;
            state.lifecycle = *next;
            complete_streaming_entries(state, emitted_at);
            if *next == LifecycleState::Failed {
                state.history.push(PublicHistoryEntry::Notice {
                    metadata: entry_metadata(
                        state,
                        event_id,
                        emitted_at,
                        PublicEntryGenerationStatus::Completed,
                    ),
                    level: PublicNoticeLevel::Error,
                    message: message.clone().unwrap_or_else(|| "Turn failed".to_owned()),
                    detail: json!({"kind": "turn_failed"}),
                });
            }
        }
    }
    Ok(())
}

fn entry_metadata(
    state: &ProjectionSnapshot,
    event_id: u64,
    emitted_at: u64,
    generation_status: PublicEntryGenerationStatus,
) -> PublicEntryMetadata {
    let id = state.turn_id.as_ref().map_or_else(
        || format!("entry-{event_id}"),
        |turn_id| format!("entry-{turn_id}-{event_id}"),
    );
    PublicEntryMetadata {
        id,
        session_id: state.session_id.clone(),
        turn_id: state.turn_id.clone(),
        created_at: emitted_at,
        updated_at: emitted_at,
        generation_status,
        related_entry_id: None,
    }
}

fn complete_streaming_entries(state: &mut ProjectionSnapshot, emitted_at: u64) {
    for entry in &mut state.history {
        let metadata = entry.metadata_mut();
        if metadata.generation_status == PublicEntryGenerationStatus::InProgress {
            metadata.generation_status = PublicEntryGenerationStatus::Completed;
            metadata.updated_at = emitted_at;
        }
    }
}

fn require_active(state: &ProjectionSnapshot, event: &'static str) -> Result<(), ProjectionError> {
    if state.lifecycle.is_active() {
        Ok(())
    } else {
        Err(ProjectionError::IllegalTransition {
            from: state.lifecycle,
            event,
        })
    }
}

fn require_lifecycle(
    state: &ProjectionSnapshot,
    allowed: &[LifecycleState],
    event: &'static str,
) -> Result<(), ProjectionError> {
    if allowed.contains(&state.lifecycle) {
        Ok(())
    } else {
        Err(ProjectionError::IllegalTransition {
            from: state.lifecycle,
            event,
        })
    }
}

fn validate_lifecycle_transition(
    from: LifecycleState,
    to: LifecycleState,
) -> Result<(), ProjectionError> {
    let valid = matches!(
        (from, to),
        (LifecycleState::Idle, LifecycleState::Running)
            | (LifecycleState::Running, LifecycleState::WaitingCallback)
            | (LifecycleState::WaitingCallback, LifecycleState::Running)
            | (
                LifecycleState::Running | LifecycleState::WaitingCallback,
                LifecycleState::Completed | LifecycleState::Failed | LifecycleState::Cancelled
            )
            | (LifecycleState::Completed, LifecycleState::Running)
    );
    if valid {
        Ok(())
    } else {
        Err(ProjectionError::IllegalTransition {
            from,
            event: "lifecycle",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: u64, event: EngineEvent) -> EventEnvelope {
        EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: None,
            emitted_at: id,
            event_id: id,
            event,
        }
    }

    #[test]
    fn reducer_is_monotonic_and_transactional() {
        let mut reducer = ProjectionReducer::new("session-1");
        assert_eq!(
            reducer
                .apply(&event(
                    1,
                    EngineEvent::UserMessage {
                        content: "hello".to_owned(),
                    },
                ))
                .expect("first event applies"),
            ApplyOutcome::Applied
        );
        let before = reducer.state().clone();
        assert_eq!(
            reducer
                .apply(&event(
                    1,
                    EngineEvent::ModelText {
                        text: "duplicate".to_owned(),
                    },
                ))
                .expect("duplicates are suppressed"),
            ApplyOutcome::Duplicate
        );
        assert_eq!(reducer.state(), &before);

        let foreign_duplicate = EventEnvelope {
            session_id: "session-2".to_owned(),
            turn_id: None,
            emitted_at: 1,
            event_id: 1,
            event: EngineEvent::ModelText {
                text: "foreign duplicate".to_owned(),
            },
        };
        assert!(matches!(
            reducer.apply(&foreign_duplicate),
            Err(ProjectionError::ForeignSession { .. })
        ));
        assert_eq!(reducer.state(), &before);

        assert_eq!(
            reducer.apply(&event(
                3,
                EngineEvent::ModelText {
                    text: "gap".to_owned(),
                }
            )),
            Err(ProjectionError::Gap {
                expected: 2,
                actual: 3
            })
        );
        assert_eq!(reducer.state(), &before);
    }

    #[test]
    fn invalid_events_leave_public_state_unchanged() {
        let mut reducer = ProjectionReducer::new("session-1");
        let before = reducer.state().clone();
        let foreign = EventEnvelope {
            session_id: "session-2".to_owned(),
            turn_id: None,
            emitted_at: 1,
            event_id: 1,
            event: EngineEvent::ModelText {
                text: "wrong".to_owned(),
            },
        };
        assert!(matches!(
            reducer.apply(&foreign),
            Err(ProjectionError::ForeignSession { .. })
        ));
        assert_eq!(reducer.state(), &before);

        assert!(matches!(
            reducer.apply(&event(
                1,
                EngineEvent::ModelText {
                    text: "illegal".to_owned(),
                }
            )),
            Err(ProjectionError::IllegalTransition { .. })
        ));
        assert_eq!(reducer.state(), &before);
    }

    #[test]
    fn callback_and_handoff_preserve_public_contract() {
        let mut reducer = ProjectionReducer::new("session-1");
        for envelope in [
            event(
                1,
                EngineEvent::UserMessage {
                    content: "ship it".to_owned(),
                },
            ),
            event(
                2,
                EngineEvent::CallbackRequested {
                    callback_id: "callback-1".to_owned(),
                    kind: CallbackKind::Approval,
                    prompt: "approve?".to_owned(),
                },
            ),
            event(
                3,
                EngineEvent::CallbackResolved {
                    callback_id: "callback-1".to_owned(),
                    accepted: true,
                    value: Some("yes".to_owned()),
                },
            ),
            event(
                4,
                EngineEvent::SessionHandoff {
                    from_session_id: "session-1".to_owned(),
                    to_session_id: "session-2".to_owned(),
                },
            ),
        ] {
            reducer.apply(&envelope).expect("valid lifecycle event");
        }
        assert_eq!(reducer.state().session_id, "session-2");
        assert_eq!(reducer.state().watermark, 4);
        assert!(
            reducer
                .state()
                .history
                .iter()
                .all(|entry| entry.metadata().session_id == "session-2")
        );
        assert!(matches!(
            reducer.state().history.get(1),
            Some(PublicHistoryEntry::Callback {
                state: PublicCallbackState::Answered { output },
                ..
            }) if output.get("value").and_then(Value::as_str) == Some("yes")
        ));
    }

    #[test]
    fn public_projection_omits_private_reasoning_signature() {
        let private = ModelMessage::Assistant {
            content: "answer".to_owned(),
            reasoning: Some("private chain".to_owned()),
            reasoning_signature: Some("provider-signature".to_owned()),
            reasoning_state: vec!["provider-state".to_owned()],
            tool_calls: Vec::new(),
        };
        let encoded_private = serde_json::to_string(&private).expect("private message serializes");
        assert!(encoded_private.contains("provider-signature"));

        let mut reducer = ProjectionReducer::new("session-1");
        reducer
            .apply(&event(
                1,
                EngineEvent::UserMessage {
                    content: "question".to_owned(),
                },
            ))
            .expect("turn starts");
        reducer
            .apply(&event(
                2,
                EngineEvent::ModelReasoning {
                    text: "summary".to_owned(),
                },
            ))
            .expect("reasoning projects");
        let public = serde_json::to_string(reducer.state()).expect("public state serializes");
        assert!(!public.contains("provider-signature"));
    }
}

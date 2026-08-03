use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, watch};

use super::attention::AttentionNotifier;
use super::controls::CallbackPresentation;
use super::debug_console::DebugConsole;
use super::diagnostics::{Activity, ErrorLog};
use super::interaction::{Overlay, PromptQueue, QuitConfirmation};
use super::narrator::NarratorManager;
use super::rewind::RewindState;
use super::session_picker::SessionDeleteState;
use super::transcript_view::TranscriptView;

const MAX_DIAGNOSTICS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    Pending,
    Streaming,
    Blocked,
    Completed,
    Failed,
    Cancelled,
    Skipped,
}

impl EntryStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Skipped
        )
    }

    /// Reference label for the settled or in-flight state of an entry.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Streaming => "streaming",
            Self::Blocked => "blocked",
            Self::Completed => "complete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptKind {
    UserMessage,
    AssistantMessage,
    Reasoning,
    Effect,
    Callback,
    Checkpoint,
    Notice,
    Plan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptEntry {
    pub id: String,
    pub revision: u64,
    pub kind: TranscriptKind,
    pub text: String,
    pub status: EntryStatus,
    #[serde(default)]
    pub details: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuiSnapshot {
    pub session_id: String,
    pub event_id: u64,
    pub entries: Vec<TranscriptEntry>,
    pub cursor_before: Option<String>,
    pub cursor_after: Option<String>,
    pub waiting: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanReviewState {
    pub path: PathBuf,
    pub content: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalEntryPosition {
    entry_id: String,
    after_entry_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    Snapshot(TuiSnapshot),
    Watermark {
        event_id: u64,
    },
    EntryAdded {
        event_id: u64,
        entry: TranscriptEntry,
    },
    EntryUpdated {
        event_id: u64,
        entry: TranscriptEntry,
    },
    Waiting {
        event_id: u64,
        waiting: bool,
    },
    Diagnostic {
        event_id: u64,
        message: String,
    },
    TransportLost(String),
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyResult {
    Applied,
    Duplicate,
    ResyncRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiState {
    pub session_id: String,
    pub watermark: u64,
    pub entries: Vec<TranscriptEntry>,
    pub cursor_before: Option<String>,
    pub cursor_after: Option<String>,
    pub waiting: bool,
    pub ready: bool,
    pub connected: bool,
    pub resync_required: bool,
    pub scroll_offset: usize,
    pub viewport: (u16, u16),
    pub overlay: Option<Overlay>,
    pub rewind: Option<RewindState>,
    pub session_delete: Option<SessionDeleteState>,
    pub prompt_queue: PromptQueue,
    pub quit_confirmation: QuitConfirmation,
    pub rewind_confirmation: QuitConfirmation,
    pub tools_collapsed: bool,
    pub show_reasoning: bool,
    /// Reference `autocopy_to_clipboard`: copy on pointer release.
    pub autocopy_to_clipboard: bool,
    /// Reference `ask_confirmation_on_exit`: `Ctrl+D` needs a second press.
    pub ask_confirmation_on_exit: bool,
    pub callback: Option<CallbackPresentation>,
    pub callback_scroll_offset: isize,
    pub plan_review: Option<PlanReviewState>,
    /// Live turn progress, refreshed on every loop tick while work is active.
    pub activity: Option<Activity>,
    /// Failures already surfaced, so a retried turn cannot repeat itself.
    pub errors: ErrorLog,
    /// Live log paging, present only while the debug console is open.
    pub debug_console: Option<DebugConsole>,
    /// What the last frame painted, and what the operator selected in it.
    pub transcript_view: TranscriptView,
    /// Focus-aware attention effects, owned locally like the reference adapter.
    pub notifier: AttentionNotifier,
    /// Narration lifecycle, owned locally so a resync cannot revive a summary.
    pub narrator: NarratorManager,
    /// Whether this session already reported that narration cannot be spoken.
    pub speech_notice_shown: bool,
    turn_started_ms: Option<u64>,
    scroll_line_limit: usize,
    diagnostics: VecDeque<String>,
    entry_indexes: BTreeMap<String, usize>,
    local_sequence: u64,
    local_entries: Vec<LocalEntryPosition>,
}

impl TuiState {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            watermark: 0,
            entries: Vec::new(),
            cursor_before: None,
            cursor_after: None,
            waiting: false,
            ready: false,
            connected: true,
            resync_required: false,
            scroll_offset: 0,
            viewport: (80, 24),
            overlay: None,
            rewind: None,
            session_delete: None,
            prompt_queue: PromptQueue::default(),
            quit_confirmation: QuitConfirmation::default(),
            rewind_confirmation: QuitConfirmation::default(),
            // The reference folds collapsible tool results into their header
            // until the operator expands them.
            tools_collapsed: true,
            show_reasoning: true,
            autocopy_to_clipboard: false,
            ask_confirmation_on_exit: true,
            callback: None,
            callback_scroll_offset: 0,
            plan_review: None,
            activity: None,
            errors: ErrorLog::default(),
            debug_console: None,
            transcript_view: TranscriptView::default(),
            notifier: AttentionNotifier::default(),
            narrator: NarratorManager::default(),
            speech_notice_shown: false,
            turn_started_ms: None,
            scroll_line_limit: 0,
            diagnostics: VecDeque::new(),
            entry_indexes: BTreeMap::new(),
            local_sequence: 0,
            local_entries: Vec::new(),
        }
    }

    pub fn apply(&mut self, event: ServerEvent) -> Result<ApplyResult, StateError> {
        match event {
            ServerEvent::Snapshot(snapshot) => {
                if snapshot.session_id != self.session_id {
                    return Err(StateError::ForeignSession(snapshot.session_id));
                }
                validate_snapshot(&snapshot)?;
                self.watermark = snapshot.event_id;
                self.entries = snapshot.entries;
                self.cursor_before = snapshot.cursor_before;
                self.cursor_after = snapshot.cursor_after;
                self.waiting = snapshot.waiting;
                self.connected = true;
                self.resync_required = false;
                self.reindex();
                Ok(ApplyResult::Applied)
            }
            ServerEvent::TransportLost(message) => {
                self.connected = false;
                self.resync_required = true;
                self.push_diagnostic(message);
                Ok(ApplyResult::ResyncRequired)
            }
            ServerEvent::Ready => {
                self.ready = true;
                Ok(ApplyResult::Applied)
            }
            ServerEvent::Watermark { event_id } => match self.check_sequence(event_id) {
                ApplyResult::Applied => {
                    self.watermark = event_id;
                    Ok(ApplyResult::Applied)
                }
                other => Ok(other),
            },
            ServerEvent::EntryAdded { event_id, entry } => {
                match self.check_sequence(event_id) {
                    ApplyResult::Applied => {}
                    other => return Ok(other),
                }
                if self.entry_indexes.contains_key(&entry.id) {
                    return Err(StateError::DuplicateEntry(entry.id));
                }
                self.watermark = event_id;
                self.entry_indexes
                    .insert(entry.id.clone(), self.entries.len());
                self.entries.push(entry);
                Ok(ApplyResult::Applied)
            }
            ServerEvent::EntryUpdated { event_id, entry } => {
                match self.check_sequence(event_id) {
                    ApplyResult::Applied => {}
                    other => return Ok(other),
                }
                let Some(index) = self.entry_indexes.get(&entry.id).copied() else {
                    return Err(StateError::UnknownEntry(entry.id));
                };
                let current = &self.entries[index];
                if current.status.is_terminal() {
                    return Err(StateError::CompletedEntryMutation(entry.id));
                }
                if entry.revision <= current.revision {
                    return Err(StateError::StaleRevision(entry.id));
                }
                // Streaming content only ever grows, but a settling effect
                // replaces its accumulated stream with the authoritative
                // terminal projection, so that rewrite must not be rejected.
                if !entry.status.is_terminal() && !entry.text.starts_with(&current.text) {
                    return Err(StateError::NonMonotonicStream(entry.id));
                }
                self.watermark = event_id;
                self.entries[index] = entry;
                Ok(ApplyResult::Applied)
            }
            ServerEvent::Waiting { event_id, waiting } => match self.check_sequence(event_id) {
                ApplyResult::Applied => {
                    self.watermark = event_id;
                    self.waiting = waiting;
                    Ok(ApplyResult::Applied)
                }
                other => Ok(other),
            },
            ServerEvent::Diagnostic { event_id, message } => match self.check_sequence(event_id) {
                ApplyResult::Applied => {
                    self.watermark = event_id;
                    self.push_diagnostic(message);
                    Ok(ApplyResult::Applied)
                }
                other => Ok(other),
            },
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.viewport = (width.max(1), height.max(1));
    }

    /// Refreshes the live turn progress. Idle work clears it immediately, so a
    /// stale indicator can never outlive the turn it described.
    pub fn sync_activity(&mut self, now_ms: u64) {
        self.quit_confirmation.expire(now_ms);
        if !self.waiting {
            self.turn_started_ms = None;
            self.activity = None;
            return;
        }
        if self.turn_started_ms.is_none() {
            // Failure muting is scoped to one turn: the same failure repeated
            // by a later turn is new information and must be shown again.
            self.errors.clear();
        }
        let started = *self.turn_started_ms.get_or_insert(now_ms);
        self.activity = Some(Activity::new(
            super::transcript::activity_status(&self.entries),
            now_ms.saturating_sub(started) / 1_000,
            self.prompt_queue.len(),
        ));
    }

    pub fn scroll_up(&mut self, rows: usize) -> bool {
        let previous = self.scroll_offset;
        self.scroll_offset = self
            .scroll_offset
            .saturating_add(rows)
            .min(self.scroll_line_limit);
        self.scroll_offset != previous
    }

    pub fn scroll_down(&mut self, rows: usize) -> bool {
        let previous = self.scroll_offset;
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
        self.scroll_offset != previous
    }

    #[must_use]
    pub fn needs_older_history(&self) -> bool {
        self.cursor_before.is_some() && self.scroll_offset >= self.scroll_line_limit
    }

    pub(crate) fn set_scroll_line_limit(&mut self, limit: usize) {
        self.scroll_line_limit = limit;
        self.scroll_offset = self.scroll_offset.min(limit);
    }

    pub fn scroll_to_oldest(&mut self) {
        self.scroll_offset = usize::MAX;
    }

    pub fn prepend_history(
        &mut self,
        entries: Vec<TranscriptEntry>,
        cursor_before: Option<String>,
    ) -> Result<(), StateError> {
        let mut combined = Vec::with_capacity(entries.len().saturating_add(self.entries.len()));
        let mut ids = BTreeMap::new();
        for entry in entries.into_iter().chain(self.entries.iter().cloned()) {
            if ids.insert(entry.id.clone(), ()).is_some() {
                return Err(StateError::DuplicateEntry(entry.id));
            }
            combined.push(entry);
        }
        self.entries = combined;
        self.cursor_before = cursor_before;
        self.reindex();
        Ok(())
    }

    pub fn replace_projection_preserving_diagnostics(
        &mut self,
        mut replacement: Self,
    ) -> Result<(), StateError> {
        if replacement.session_id != self.session_id {
            return Err(StateError::ForeignSession(replacement.session_id));
        }
        replacement.viewport = self.viewport;
        replacement.overlay = self.overlay.take();
        replacement.rewind = self.rewind.take();
        replacement.session_delete = self.session_delete.take();
        replacement.prompt_queue = std::mem::take(&mut self.prompt_queue);
        replacement.quit_confirmation = std::mem::take(&mut self.quit_confirmation);
        replacement.rewind_confirmation = std::mem::take(&mut self.rewind_confirmation);
        replacement.tools_collapsed = self.tools_collapsed;
        replacement.show_reasoning = self.show_reasoning;
        replacement.autocopy_to_clipboard = self.autocopy_to_clipboard;
        replacement.ask_confirmation_on_exit = self.ask_confirmation_on_exit;
        replacement.callback = self.callback.take();
        replacement.callback_scroll_offset = self.callback_scroll_offset;
        replacement.plan_review = self.plan_review.take();
        // Locally owned observability: a canonical resync replaces history, not
        // what the operator is watching, selected, or was already told.
        replacement.activity = self.activity.take();
        replacement.turn_started_ms = self.turn_started_ms;
        replacement.errors = std::mem::take(&mut self.errors);
        replacement.debug_console = self.debug_console.take();
        replacement.transcript_view = std::mem::take(&mut self.transcript_view);
        replacement.notifier = std::mem::take(&mut self.notifier);
        replacement.narrator = std::mem::take(&mut self.narrator);
        replacement.speech_notice_shown = self.speech_notice_shown;
        replacement.diagnostics = self.diagnostics.clone();
        replacement.local_sequence = self.local_sequence;
        replacement.local_entries = self.local_entries.clone();
        for position in &self.local_entries {
            let Some(entry) = self
                .entry_indexes
                .get(&position.entry_id)
                .and_then(|index| self.entries.get(*index))
                .cloned()
            else {
                continue;
            };
            if replacement
                .entries
                .iter()
                .any(|candidate| candidate.id == entry.id)
            {
                return Err(StateError::DuplicateEntry(entry.id));
            }
            let insertion_index = position
                .after_entry_id
                .as_ref()
                .and_then(|anchor| {
                    replacement
                        .entries
                        .iter()
                        .position(|candidate| candidate.id == *anchor)
                })
                .map_or(replacement.entries.len(), |index| index.saturating_add(1));
            replacement.entries.insert(insertion_index, entry);
        }
        replacement.reindex();
        *self = replacement;
        Ok(())
    }

    pub fn diagnostics(&self) -> impl Iterator<Item = &str> {
        self.diagnostics.iter().map(String::as_str)
    }

    pub fn push_diagnostic(&mut self, message: impl Into<String>) {
        if self.diagnostics.len() == MAX_DIAGNOSTICS {
            self.diagnostics.pop_front();
        }
        self.diagnostics.push_back(message.into());
    }

    pub fn set_callback_presentation(&mut self, presentation: Option<CallbackPresentation>) {
        let focus_changed = match (&self.callback, &presentation) {
            (Some(previous), Some(next)) => {
                previous.callback_id != next.callback_id || previous.focus_line != next.focus_line
            }
            (None, None) => false,
            _ => true,
        };
        if focus_changed {
            self.callback_scroll_offset = 0;
        }
        self.callback = presentation;
    }

    pub fn scroll_callback(&mut self, delta: isize) {
        if self.callback.is_none() {
            return;
        }
        self.callback_scroll_offset = self.callback_scroll_offset.saturating_add(delta);
    }

    pub fn append_local(&mut self, mut entry: TranscriptEntry) -> String {
        let after_entry_id = self.entries.last().map(|entry| entry.id.clone());
        self.local_sequence = self.local_sequence.saturating_add(1);
        entry.id = format!("local-{}", self.local_sequence);
        let id = entry.id.clone();
        self.entry_indexes
            .insert(entry.id.clone(), self.entries.len());
        self.entries.push(entry);
        self.local_entries.push(LocalEntryPosition {
            entry_id: id.clone(),
            after_entry_id,
        });
        id
    }

    pub fn settle_local(&mut self, entry_id: &str, status: EntryStatus) -> Result<(), StateError> {
        let Some(index) = self.entry_indexes.get(entry_id).copied() else {
            return Err(StateError::UnknownEntry(entry_id.to_owned()));
        };
        let entry = &mut self.entries[index];
        if entry.status.is_terminal() {
            return Ok(());
        }
        entry.revision = entry.revision.saturating_add(1);
        entry.status = status;
        Ok(())
    }

    pub fn update_local(
        &mut self,
        entry_id: &str,
        text: String,
        status: EntryStatus,
    ) -> Result<(), StateError> {
        let Some(index) = self.entry_indexes.get(entry_id).copied() else {
            return Err(StateError::UnknownEntry(entry_id.to_owned()));
        };
        let entry = &mut self.entries[index];
        if entry.status.is_terminal() {
            return Ok(());
        }
        entry.revision = entry.revision.saturating_add(1);
        entry.text = text;
        entry.status = status;
        Ok(())
    }

    fn check_sequence(&mut self, event_id: u64) -> ApplyResult {
        if event_id <= self.watermark {
            return ApplyResult::Duplicate;
        }
        if event_id != self.watermark.saturating_add(1) {
            self.resync_required = true;
            return ApplyResult::ResyncRequired;
        }
        ApplyResult::Applied
    }

    fn reindex(&mut self) {
        self.entry_indexes = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id.clone(), index))
            .collect();
    }
}

fn validate_snapshot(snapshot: &TuiSnapshot) -> Result<(), StateError> {
    let mut seen = BTreeMap::new();
    for entry in &snapshot.entries {
        if seen.insert(&entry.id, ()).is_some() {
            return Err(StateError::DuplicateEntry(entry.id.clone()));
        }
    }
    Ok(())
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StateError {
    #[error("snapshot belongs to foreign session `{0}`")]
    ForeignSession(String),
    #[error("entry `{0}` appears more than once")]
    DuplicateEntry(String),
    #[error("entry `{0}` does not exist")]
    UnknownEntry(String),
    #[error("completed entry `{0}` is immutable")]
    CompletedEntryMutation(String),
    #[error("entry `{0}` update has a stale revision")]
    StaleRevision(String),
    #[error("entry `{0}` update does not grow monotonically")]
    NonMonotonicStream(String),
}

pub struct EventMailbox {
    pub sender: mpsc::Sender<ServerEvent>,
    pub receiver: mpsc::Receiver<ServerEvent>,
    pub resize_sender: watch::Sender<(u16, u16)>,
    pub resize_receiver: watch::Receiver<(u16, u16)>,
}

impl EventMailbox {
    #[must_use]
    pub fn bounded(capacity: usize, initial_size: (u16, u16)) -> Self {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let (resize_sender, resize_receiver) = watch::channel(initial_size);
        Self {
            sender,
            receiver,
            resize_sender,
            resize_receiver,
        }
    }

    pub fn try_send(&self, event: ServerEvent) -> Result<(), MailboxError> {
        self.sender.try_send(event).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => MailboxError::Full,
            mpsc::error::TrySendError::Closed(_) => MailboxError::Closed,
        })
    }

    pub fn resize(&self, width: u16, height: u16) {
        self.resize_sender
            .send_replace((width.max(1), height.max(1)));
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum MailboxError {
    #[error("TUI event queue is full")]
    Full,
    #[error("TUI event queue is closed")]
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, revision: u64, text: &str, status: EntryStatus) -> TranscriptEntry {
        TranscriptEntry {
            id: id.to_owned(),
            revision,
            kind: TranscriptKind::AssistantMessage,
            text: text.to_owned(),
            status,
            details: Value::Null,
        }
    }

    #[test]
    fn gaps_require_one_canonical_snapshot_without_partial_mutation() {
        let mut state = TuiState::new("session");
        state
            .apply(ServerEvent::EntryAdded {
                event_id: 1,
                entry: entry("one", 1, "a", EntryStatus::Streaming),
            })
            .expect("first event applies");
        assert_eq!(
            state
                .apply(ServerEvent::EntryAdded {
                    event_id: 3,
                    entry: entry("three", 1, "c", EntryStatus::Completed),
                })
                .expect("gap is observable"),
            ApplyResult::ResyncRequired
        );
        assert_eq!(state.entries.len(), 1);
        state
            .apply(ServerEvent::Snapshot(TuiSnapshot {
                session_id: "session".to_owned(),
                event_id: 3,
                entries: vec![entry("canonical", 1, "state", EntryStatus::Completed)],
                cursor_before: Some("older".to_owned()),
                cursor_after: None,
                waiting: false,
            }))
            .expect("snapshot replaces local state");
        assert_eq!(state.entries[0].id, "canonical");
        assert!(!state.resync_required);
    }

    #[test]
    fn watermark_only_events_preserve_live_sequence_without_mutating_projection() {
        let mut state = TuiState::new("session");
        state
            .apply(ServerEvent::EntryAdded {
                event_id: 1,
                entry: entry("user", 1, "hello", EntryStatus::Completed),
            })
            .expect("user message applies");
        assert_eq!(
            state
                .apply(ServerEvent::Watermark { event_id: 2 })
                .expect("title-only projection advances the watermark"),
            ApplyResult::Applied
        );
        state
            .apply(ServerEvent::EntryAdded {
                event_id: 3,
                entry: entry("assistant", 1, "answer", EntryStatus::Streaming),
            })
            .expect("model text applies after the title watermark");

        assert_eq!(state.watermark, 3);
        assert_eq!(state.entries.len(), 2);
        assert!(!state.resync_required);
    }

    #[test]
    fn streamed_content_is_monotonic_and_completed_entries_are_immutable() {
        let mut state = TuiState::new("session");
        state
            .apply(ServerEvent::EntryAdded {
                event_id: 1,
                entry: entry("one", 1, "a", EntryStatus::Streaming),
            })
            .expect("stream starts");
        state
            .apply(ServerEvent::EntryUpdated {
                event_id: 2,
                entry: entry("one", 2, "answer", EntryStatus::Completed),
            })
            .expect("stream completes");
        assert!(matches!(
            state.apply(ServerEvent::EntryUpdated {
                event_id: 3,
                entry: entry("one", 3, "answer changed", EntryStatus::Completed),
            }),
            Err(StateError::CompletedEntryMutation(_))
        ));
        assert_eq!(state.watermark, 2);
        assert_eq!(
            state
                .apply(ServerEvent::Diagnostic {
                    event_id: 3,
                    message: "recovered".to_owned(),
                })
                .expect("invalid mutation did not consume the sequence"),
            ApplyResult::Applied
        );
    }

    #[test]
    fn settling_replaces_streamed_output_while_live_growth_stays_monotonic() {
        let mut state = TuiState::new("session");
        state
            .apply(ServerEvent::EntryAdded {
                event_id: 1,
                entry: entry("effect", 1, "shell\npartial", EntryStatus::Streaming),
            })
            .expect("effect starts");
        assert!(matches!(
            state.apply(ServerEvent::EntryUpdated {
                event_id: 2,
                entry: entry("effect", 2, "shell\nrewritten", EntryStatus::Streaming),
            }),
            Err(StateError::NonMonotonicStream(_))
        ));
        state
            .apply(ServerEvent::EntryUpdated {
                event_id: 2,
                entry: entry("effect", 2, "shell\npermission denied", EntryStatus::Failed),
            })
            .expect("terminal projection replaces the stream");
        assert_eq!(state.entries[0].status, EntryStatus::Failed);
        assert_eq!(state.entries[0].text, "shell\npermission denied");
    }

    #[test]
    fn canonical_replacement_preserves_diagnostics_but_not_stale_projection_data() {
        let mut state = TuiState::new("session");
        state.push_diagnostic("keep this diagnostic");
        state.resize(41, 9);
        state
            .apply(ServerEvent::EntryAdded {
                event_id: 1,
                entry: entry("stale", 1, "old", EntryStatus::Completed),
            })
            .expect("stale entry applies");

        let mut replacement = TuiState::new("session");
        replacement
            .apply(ServerEvent::Snapshot(TuiSnapshot {
                session_id: "session".to_owned(),
                event_id: 8,
                entries: vec![entry("canonical", 1, "new", EntryStatus::Completed)],
                cursor_before: None,
                cursor_after: None,
                waiting: false,
            }))
            .expect("canonical state");
        state
            .replace_projection_preserving_diagnostics(replacement)
            .expect("same-session replacement");

        assert_eq!(state.watermark, 8);
        assert_eq!(state.entries[0].id, "canonical");
        assert_eq!(state.viewport, (41, 9));
        assert_eq!(
            state.diagnostics().collect::<Vec<_>>(),
            vec!["keep this diagnostic"]
        );
    }

    #[test]
    fn a_resync_replaces_history_without_disturbing_local_observability() {
        let mut state = TuiState::new("session");
        state.waiting = true;
        state.sync_activity(5_000);
        state.debug_console = Some(super::super::debug_console::DebugConsole::default());
        state
            .transcript_view
            .publish(3, vec!["See https://ratatui.rs".to_owned()]);
        state
            .transcript_view
            .begin_selection(super::super::transcript_view::Cell { line: 0, column: 0 });
        state
            .transcript_view
            .extend_selection(super::super::transcript_view::Cell { line: 0, column: 3 });
        assert!(state.transcript_view.has_selection());

        let mut replacement = TuiState::new("session");
        replacement
            .apply(ServerEvent::Snapshot(TuiSnapshot {
                session_id: "session".to_owned(),
                event_id: 2,
                entries: vec![entry("canonical", 1, "new", EntryStatus::Completed)],
                cursor_before: None,
                cursor_after: None,
                waiting: true,
            }))
            .expect("canonical state");
        state
            .replace_projection_preserving_diagnostics(replacement)
            .expect("same-session replacement");

        assert!(state.debug_console.is_some(), "the console stayed open");
        assert!(
            state.transcript_view.has_selection(),
            "the operator's selection survived the resync"
        );
        // The turn clock keeps running: the resync did not restart the turn.
        state.sync_activity(9_000);
        assert_eq!(
            state
                .activity
                .as_ref()
                .map(|activity| activity.elapsed_seconds),
            Some(4)
        );
    }

    #[test]
    fn canonical_replacement_restores_local_entries_at_their_explicit_anchors() {
        let mut state = TuiState::new("session");
        state
            .apply(ServerEvent::EntryAdded {
                event_id: 1,
                entry: entry("server-one", 1, "one", EntryStatus::Completed),
            })
            .expect("first server entry");
        state.append_local(entry("ignored", 1, "local one", EntryStatus::Completed));
        state.append_local(entry("ignored", 1, "local two", EntryStatus::Cancelled));
        state
            .apply(ServerEvent::EntryAdded {
                event_id: 2,
                entry: entry("server-two", 1, "two", EntryStatus::Completed),
            })
            .expect("second server entry");

        let mut replacement = TuiState::new("session");
        replacement
            .apply(ServerEvent::Snapshot(TuiSnapshot {
                session_id: "session".to_owned(),
                event_id: 3,
                entries: vec![
                    entry("server-one", 1, "one", EntryStatus::Completed),
                    entry("server-two", 1, "two", EntryStatus::Completed),
                ],
                cursor_before: None,
                cursor_after: None,
                waiting: false,
            }))
            .expect("canonical snapshot");
        state
            .replace_projection_preserving_diagnostics(replacement)
            .expect("local positions restore");

        assert_eq!(
            state
                .entries
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "local one", "local two", "two"]
        );
    }

    #[test]
    fn local_callback_notice_settles_without_advancing_server_watermark() {
        let mut state = TuiState::new("session");
        let id = state.append_local(entry(
            "",
            1,
            "Awaiting a Unicode answer: café?",
            EntryStatus::Streaming,
        ));
        state
            .settle_local(&id, EntryStatus::Completed)
            .expect("local callback settles");

        assert_eq!(state.entries[0].status, EntryStatus::Completed);
        assert_eq!(state.entries[0].revision, 2);
        assert_eq!(state.watermark, 0);
    }

    #[test]
    fn paging_preserves_order_and_requests_older_history_at_the_top() {
        let mut state = TuiState::new("session");
        state
            .apply(ServerEvent::Snapshot(TuiSnapshot {
                session_id: "session".to_owned(),
                event_id: 4,
                entries: vec![
                    entry("three", 1, "three", EntryStatus::Completed),
                    entry("four", 1, "four", EntryStatus::Completed),
                ],
                cursor_before: Some("before-three".to_owned()),
                cursor_after: None,
                waiting: false,
            }))
            .expect("snapshot applies");
        state.set_scroll_line_limit(1);
        state.scroll_up(10);
        assert!(state.needs_older_history());
        state
            .prepend_history(
                vec![
                    entry("one", 1, "one", EntryStatus::Completed),
                    entry("two", 1, "two", EntryStatus::Completed),
                ],
                None,
            )
            .expect("older history prepends");
        assert_eq!(
            state
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three", "four"]
        );
    }

    #[test]
    fn bounded_mailbox_rejects_overflow_and_coalesces_resize_storms() {
        let mailbox = EventMailbox::bounded(1, (80, 24));
        mailbox
            .try_send(ServerEvent::Ready)
            .expect("first event fits");
        assert_eq!(
            mailbox.try_send(ServerEvent::Ready),
            Err(MailboxError::Full)
        );
        for size in 1..=1_000 {
            mailbox.resize(size, size);
        }
        assert_eq!(*mailbox.resize_receiver.borrow(), (1_000, 1_000));
    }
}

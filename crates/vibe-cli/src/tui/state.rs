use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::attention::{AttentionEffect, AttentionNotifier};
use super::controls::CallbackPresentation;
use super::debug_console::DebugConsole;
use super::diagnostics::{Activity, ErrorLog};
use super::interaction::{Overlay, PromptQueue, QuitConfirmation};
use super::narrator::NarratorManager;
use super::rewind::RewindState;
use super::session_picker::SessionDeleteState;
use super::transcript_view::TranscriptView;
use vibe_app_server::client::{
    NoticeDetail, PublicEffectState, PublicHistoryEntry, PublicNoticeLevel,
};

const MAX_DIAGNOSTICS: usize = 100;

/// Reference `HISTORY_RESUME_TAIL_MESSAGES` and `LOAD_MORE_BATCH_SIZE`
/// (`vibe/cli/textual_ui/windowing/state.py`).
pub const HISTORY_RESUME_TAIL_MESSAGES: usize = 20;
pub const LOAD_MORE_BATCH_SIZE: usize = 10;

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
    /// How a published effect settles on the transcript. The effect's own state
    /// is authoritative: a completed generation whose effect failed, was
    /// cancelled, or was skipped must never settle as a success.
    #[must_use]
    pub const fn of_effect(state: &PublicEffectState) -> Self {
        match state {
            PublicEffectState::Pending => Self::Pending,
            PublicEffectState::Running { .. } => Self::Streaming,
            PublicEffectState::Blocked { .. } => Self::Blocked,
            PublicEffectState::Completed { .. } => Self::Completed,
            PublicEffectState::Failed { .. } => Self::Failed,
            PublicEffectState::Cancelled { .. } => Self::Cancelled,
            PublicEffectState::Skipped { .. } => Self::Skipped,
        }
    }

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
    /// A Markdown document this client wrote itself, which `/help` mounts the
    /// way reference `_show_help` mounts a `UserCommandMessage`.
    Document,
    /// The submitted command line itself, mounted above the handler's own
    /// output the way reference `_handle_command` mounts a
    /// `SlashCommandMessage`.
    Command,
}

/// Where a transcript entry came from.
///
/// A server entry keeps the canonical [`PublicHistoryEntry`] it arrived as, so
/// every semantic projection reads its typed fields rather than re-parsing a
/// JSON rendering of them. The other two variants name the only entries that
/// have no canonical form: what a saved conversation was replayed from, and
/// what this client wrote itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntrySource {
    Server(Box<PublicHistoryEntry>),
    /// Replayed from saved history, which records the text and nothing else.
    Restored,
    /// A notice this client wrote itself.
    Notice {
        /// How the notice reads, which is what the renderer colors it by.
        level: PublicNoticeLevel,
        /// The callback whose lifetime this notice tracks, when it tracks one.
        /// Settling that callback is what settles the notice.
        callback_id: Option<String>,
    },
}

impl EntrySource {
    /// A notice reporting a condition rather than progress.
    #[must_use]
    pub const fn notice(level: PublicNoticeLevel) -> Self {
        Self::Notice {
            level,
            callback_id: None,
        }
    }

    /// A notice whose lifetime follows `callback_id`.
    #[must_use]
    pub fn tracking(callback_id: impl Into<String>) -> Self {
        Self::Notice {
            level: PublicNoticeLevel::Info,
            callback_id: Some(callback_id.into()),
        }
    }

    /// The canonical entry this projects, when the server published one.
    #[must_use]
    pub fn server(&self) -> Option<&PublicHistoryEntry> {
        match self {
            Self::Server(entry) => Some(entry),
            Self::Restored | Self::Notice { .. } => None,
        }
    }

    /// The callback whose lifetime a locally written notice tracks.
    #[must_use]
    pub fn tracked_callback(&self) -> Option<&str> {
        match self {
            Self::Notice { callback_id, .. } => callback_id.as_deref(),
            Self::Server(_) | Self::Restored => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub id: String,
    pub revision: u64,
    pub kind: TranscriptKind,
    pub text: String,
    pub status: EntryStatus,
    pub source: EntrySource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Reference `DEFAULT_NOTICE_TIMEOUT`.
pub const INLINE_NOTICE_MS: u64 = 4_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineNotice {
    pub text: String,
    pub expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueSelection {
    /// The selected prompt's queue id.
    pub selected: String,
    /// Its place counted from the newest, kept when a promotion takes it.
    pub position: usize,
    /// What the composer held before the queue took it, restored on exit.
    pub original: String,
    pub editing: bool,
    /// The edited prompt started before the edit was saved.
    pub consumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiState {
    pub session_id: String,
    pub watermark: u64,
    pub entries: Vec<TranscriptEntry>,
    pub cursor_before: Option<String>,
    pub cursor_after: Option<String>,
    /// Reference `SessionWindowing`: a resumed transcript shows its latest
    /// entries, and this names the oldest one shown. Everything before it is
    /// revealed a batch at a time by "Load more messages".
    pub history_first_visible: Option<String>,
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
    /// Whether each tool section, tool group and reasoning block is folded,
    /// by the key the renderer gives it when it first paints.
    pub folds: BTreeMap<String, bool>,
    /// Advances every tenth of a second, which is the interval reference
    /// `SpinnerMixin` animates a running indicator at.
    pub animation_frame: u64,
    /// Reference `LoadingWidget`, rebuilt for every turn.
    pub loading: super::loading::LoadingAnimation,
    /// Reference `PetitChat`, the banner's cat.
    pub banner_cat: super::loading::PetitChat,
    /// What the last update appended to each running effect's output, which
    /// reference `set_stream_message` shows under the call.
    pub effect_streams: BTreeMap<String, String>,
    /// When a key last reached the composer, which reference
    /// `time_since_last_keystroke` reads.
    pub last_keystroke_ms: u64,
    /// Reference `_wait_for_typing_pause`: a callback that arrived while the
    /// operator was typing waits, and the composer keeps the keyboard.
    pub typing_hold: bool,
    /// The callback the typing pause last released, so it is held only once.
    pub revealed_callback: Option<String>,
    /// Reference `InlineNotice`: a muted line at the right end of the loading
    /// row that hides itself once its timeout passes.
    pub inline_notice: Option<InlineNotice>,
    /// Reference `ChatInputBody` queue mode: the queued prompt Up selected, and
    /// whether the composer is editing it.
    pub queue_selection: Option<QueueSelection>,
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
    /// Terminal writes the notifier decided and the next frame performs.
    pub pending_attention: Vec<AttentionEffect>,
    /// Narration lifecycle, owned locally so a resync cannot revive a summary.
    pub narrator: NarratorManager,
    /// Whether this session already reported that narration cannot be spoken.
    pub speech_notice_shown: bool,
    /// Reference `_is_file_watcher_enabled`: the gate the completion index
    /// reads on every query, refreshed with the rest of the preferences.
    pub file_watcher_for_autocomplete: bool,
    /// Reference `_retry_presentation`: a turn failed in a way `/retry` can
    /// continue, and no prompt has been sent since.
    pub retry_offered: bool,
    /// The settings field whose value the composer is holding.
    pub value_edit: Option<super::interaction::ValueEdit>,
    /// The `/log-level` picker's draft, while it is open.
    pub(in crate::tui) log_level_picker: Option<super::command_handlers::log_level::Picker>,
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
            history_first_visible: None,
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
            folds: BTreeMap::new(),
            animation_frame: 0,
            loading: super::loading::LoadingAnimation::default(),
            banner_cat: super::loading::PetitChat::default(),
            effect_streams: BTreeMap::new(),
            last_keystroke_ms: 0,
            typing_hold: false,
            revealed_callback: None,
            inline_notice: None,
            queue_selection: None,
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
            pending_attention: Vec::new(),
            narrator: NarratorManager::default(),
            speech_notice_shown: false,
            file_watcher_for_autocomplete: false,
            retry_offered: false,
            value_edit: None,
            log_level_picker: None,
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
                if let Some(title) = self.entries.iter().rev().find_map(session_title_update) {
                    let title = title.to_owned();
                    self.retitle(&title);
                }
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
                let title = session_title_update(&entry).map(ToOwned::to_owned);
                self.entries.push(entry);
                if let Some(title) = title {
                    self.retitle(&title);
                }
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
                // A running effect's output is patched with `replace` when it
                // is not a prefix of the next (reference `make_json_patch`),
                // so only messages and reasoning are held to growing.
                if !entry.status.is_terminal()
                    && entry.kind != TranscriptKind::Effect
                    && !entry.text.starts_with(&current.text)
                {
                    return Err(StateError::NonMonotonicStream(entry.id));
                }
                self.watermark = event_id;
                self.track_stream(index, &entry);
                self.entries[index] = entry;
                Ok(ApplyResult::Applied)
            }
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

    /// Queues one attention write for the next frame.
    pub fn attend(&mut self, effect: AttentionEffect) {
        self.pending_attention.push(effect);
    }

    /// Reference `_on_session_title_changed`: the session title becomes the
    /// tab's default title.
    pub fn retitle(&mut self, title: &str) {
        if let Some(effect) = self.notifier.set_default_title(title) {
            self.attend(effect);
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
            // Reference `_ensure_loading_widget` mounts a fresh widget per turn.
            self.loading = super::loading::LoadingAnimation::default();
        }
        let started = *self.turn_started_ms.get_or_insert(now_ms);
        let (month, day) = super::loading::month_day(now_ms);
        let status = self
            .loading
            .status(
                &super::transcript::activity_status(&self.entries),
                month,
                day,
            )
            .to_owned();
        self.activity = Some(Activity::new(
            status,
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
        self.has_older_history() && self.scroll_offset >= self.scroll_line_limit
    }

    /// Reference `create_resume_plan`: a resumed transcript shows its latest
    /// `HISTORY_RESUME_TAIL_MESSAGES` entries and holds the rest back.
    pub fn window_history_tail(&mut self) {
        self.history_first_visible = self
            .entries
            .len()
            .checked_sub(HISTORY_RESUME_TAIL_MESSAGES)
            .filter(|start| *start > 0)
            .and_then(|start| self.entries.get(start))
            .map(|entry| entry.id.clone());
    }

    /// How many leading entries the window holds back.
    #[must_use]
    pub fn hidden_history(&self) -> usize {
        self.history_first_visible
            .as_ref()
            .and_then(|id| self.entry_indexes.get(id))
            .copied()
            .unwrap_or_default()
    }

    /// Reference `_has_older_history`: entries held back here, or a page the
    /// server has not sent yet.
    #[must_use]
    pub fn has_older_history(&self) -> bool {
        self.hidden_history() > 0 || self.cursor_before.is_some()
    }

    /// Reference `_history_backfill_remaining`: the count shown on the button,
    /// known only once no page is left on the server.
    #[must_use]
    pub fn older_history_remaining(&self) -> Option<usize> {
        if self.cursor_before.is_some() {
            return None;
        }
        Some(self.hidden_history()).filter(|remaining| *remaining > 0)
    }

    /// Reference `SessionWindowing.next_load_more_batch`: reveals the
    /// `LOAD_MORE_BATCH_SIZE` entries above the oldest one shown. Answers
    /// whether anything held back here was revealed.
    pub fn reveal_older_history(&mut self) -> bool {
        let hidden = self.hidden_history();
        if hidden == 0 {
            return false;
        }
        let start = hidden.saturating_sub(LOAD_MORE_BATCH_SIZE);
        self.history_first_visible = (start > 0)
            .then(|| self.entries.get(start).map(|entry| entry.id.clone()))
            .flatten();
        true
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
        // A page fetched from the server is shown whole, as the reference
        // mounts the batch it just loaded.
        self.history_first_visible = None;
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
        // The window survives a resync while the entry it starts at does.
        replacement.history_first_visible = self
            .history_first_visible
            .take()
            .filter(|id| replacement.entries.iter().any(|entry| &entry.id == id));
        replacement.overlay = self.overlay.take();
        replacement.rewind = self.rewind.take();
        replacement.session_delete = self.session_delete.take();
        replacement.prompt_queue = std::mem::take(&mut self.prompt_queue);
        replacement.quit_confirmation = std::mem::take(&mut self.quit_confirmation);
        replacement.rewind_confirmation = std::mem::take(&mut self.rewind_confirmation);
        replacement.tools_collapsed = self.tools_collapsed;
        replacement.folds = std::mem::take(&mut self.folds);
        replacement.animation_frame = self.animation_frame;
        replacement.loading = std::mem::take(&mut self.loading);
        replacement.banner_cat = std::mem::take(&mut self.banner_cat);
        replacement.last_keystroke_ms = self.last_keystroke_ms;
        replacement.typing_hold = self.typing_hold;
        replacement.revealed_callback = self.revealed_callback.take();
        replacement.inline_notice = self.inline_notice.take();
        replacement.queue_selection = self.queue_selection.take();
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
        replacement.file_watcher_for_autocomplete = self.file_watcher_for_autocomplete;
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

    /// Reference `InlineNotice.show`: replaces the notice, which hides itself
    /// after `timeout_ms` or stays until hidden when there is none.
    pub fn show_inline_notice(
        &mut self,
        text: impl Into<String>,
        timeout_ms: Option<u64>,
        now_ms: u64,
    ) {
        self.inline_notice = Some(InlineNotice {
            text: text.into(),
            expires_at_ms: timeout_ms.map(|timeout| now_ms.saturating_add(timeout)),
        });
    }

    pub fn expire_inline_notice(&mut self, now_ms: u64) {
        if self
            .inline_notice
            .as_ref()
            .and_then(|notice| notice.expires_at_ms)
            .is_some_and(|expires| now_ms >= expires)
        {
            self.inline_notice = None;
        }
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

    /// Takes a local entry back out of the transcript. A later local entry
    /// anchored after it is anchored where it was, so a resync still places it.
    pub fn remove_local(&mut self, entry_id: &str) {
        let Some(index) = self.entry_indexes.remove(entry_id) else {
            return;
        };
        self.entries.remove(index);
        for position in self.entry_indexes.values_mut() {
            if *position > index {
                *position -= 1;
            }
        }
        let Some(removed) = self
            .local_entries
            .iter()
            .position(|position| position.entry_id == entry_id)
            .map(|position| self.local_entries.remove(position))
        else {
            return;
        };
        for position in &mut self.local_entries {
            if position.after_entry_id.as_deref() == Some(entry_id) {
                position.after_entry_id.clone_from(&removed.after_entry_id);
            }
        }
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

    /// Swaps a local entry for a newer projection of the same thing, keeping
    /// its identifier and position. A settled entry stays as it settled.
    pub fn replace_local(
        &mut self,
        entry_id: &str,
        mut replacement: TranscriptEntry,
    ) -> Result<(), StateError> {
        let Some(index) = self.entry_indexes.get(entry_id).copied() else {
            return Err(StateError::UnknownEntry(entry_id.to_owned()));
        };
        let entry = &mut self.entries[index];
        if entry.status.is_terminal() {
            return Ok(());
        }
        replacement.id = entry.id.clone();
        replacement.revision = entry.revision.saturating_add(1);
        self.track_stream(index, &replacement);
        self.entries[index] = replacement;
        Ok(())
    }

    /// Reference `set_stream_message`: a running effect keeps what its next
    /// revision appended to its output, and a settled one drops it.
    fn track_stream(&mut self, index: usize, next: &TranscriptEntry) {
        if next.status.is_terminal() {
            self.effect_streams.remove(&next.id);
            return;
        }
        let previous = super::transcript::running_output(&self.entries[index]).unwrap_or_default();
        if let Some(appended) = super::transcript::running_output(next)
            .and_then(|output| output.strip_prefix(previous))
            .filter(|appended| !appended.is_empty())
        {
            self.effect_streams
                .insert(next.id.clone(), appended.to_owned());
        }
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

/// The title a `session_title_updated` notice announces, which is what the
/// reference event handler forwards to the tab title.
fn session_title_update(entry: &TranscriptEntry) -> Option<&str> {
    match entry.source.server()? {
        PublicHistoryEntry::Notice {
            detail: NoticeDetail::SessionTitleUpdated { title },
            ..
        } => Some(title),
        _ => None,
    }
}

#[cfg(test)]
mod tests {

    use super::super::diagnostics;
    use super::*;

    fn entry(id: &str, revision: u64, text: &str, status: EntryStatus) -> TranscriptEntry {
        TranscriptEntry {
            id: id.to_owned(),
            revision,
            kind: TranscriptKind::AssistantMessage,
            text: text.to_owned(),
            status,
            source: EntrySource::Restored,
        }
    }

    fn history(count: usize) -> TuiState {
        let mut state = TuiState::new("session");
        state.entries = (0..count)
            .map(|index| entry(&format!("entry-{index}"), 1, "text", EntryStatus::Completed))
            .collect();
        state.reindex();
        state
    }

    #[test]
    fn a_resumed_transcript_shows_its_tail_and_reveals_the_rest_ten_at_a_time() {
        let mut state = history(35);
        state.window_history_tail();
        assert_eq!(state.hidden_history(), 15);
        assert_eq!(state.older_history_remaining(), Some(15));

        assert!(state.reveal_older_history());
        assert_eq!(state.hidden_history(), 5);
        assert!(state.reveal_older_history());
        assert_eq!(state.hidden_history(), 0);
        assert!(!state.has_older_history());
        assert!(!state.reveal_older_history());

        // A short transcript holds nothing back.
        let mut short = history(20);
        short.window_history_tail();
        assert!(!short.has_older_history());
    }

    #[test]
    fn the_button_names_no_count_while_the_server_holds_older_pages() {
        let mut state = history(25);
        state.window_history_tail();
        state.cursor_before = Some("5".to_owned());
        assert!(state.has_older_history());
        assert_eq!(state.older_history_remaining(), None);
        // A page fetched from the server is shown whole.
        state
            .prepend_history(
                vec![entry("older", 1, "text", EntryStatus::Completed)],
                None,
            )
            .expect("older page prepends");
        assert_eq!(state.hidden_history(), 0);
        assert!(!state.has_older_history());
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

    /// Reference `_appended_text(update.patch, "/state/outputText")`: the call
    /// shows what each update appended, and nothing once it settles.
    #[test]
    fn a_running_effect_streams_what_each_update_appended() {
        let effect = |revision: u64, state: serde_json::Value| {
            let mut entry = crate::tui::hydration::published_fixture(
                "call",
                serde_json::json!({
                    "type": "effect",
                    "title": "bash",
                    "detail": vibe_app_server::client::EffectDetail::for_call(
                        "bash",
                        &serde_json::json!({"command": "make"}),
                    ),
                    "state": state,
                    "generationStatus": "in_progress",
                }),
            );
            entry.revision = revision;
            entry
        };
        let mut state = TuiState::new("session");
        state
            .apply(ServerEvent::EntryAdded {
                event_id: 1,
                entry: effect(
                    1,
                    serde_json::json!({"status": "running", "outputText": "a\n"}),
                ),
            })
            .expect("the call starts");
        assert!(state.effect_streams.is_empty());
        state
            .apply(ServerEvent::EntryUpdated {
                event_id: 2,
                entry: effect(
                    2,
                    serde_json::json!({"status": "running", "outputText": "a\nb\nc\n"}),
                ),
            })
            .expect("output grows");
        assert_eq!(
            state.effect_streams.get("call").map(String::as_str),
            Some("b\nc\n")
        );
        state
            .apply(ServerEvent::EntryUpdated {
                event_id: 3,
                entry: effect(
                    3,
                    serde_json::json!({
                        "status": "failed",
                        "error": {"message": "exit 2"},
                        "outputText": "a\nb\nc\n",
                        "display": {"success": false, "message": "make"},
                    }),
                ),
            })
            .expect("the call settles");
        assert!(state.effect_streams.is_empty());
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
    fn activity_reports_the_running_effect_and_clears_the_moment_work_settles() {
        let mut state = TuiState::new("session");
        state.waiting = true;
        state.entries.push(crate::tui::hydration::published_fixture(
            "effect",
            serde_json::json!({
                "type": "effect",
                "title": "read",
                "detail": vibe_app_server::client::EffectDetail::for_call(
                    "read_file",
                    &serde_json::json!({"file_path": "a.rs"}),
                ),
                "state": {"status": "running", "outputText": ""},
            }),
        ));
        state.sync_activity(1_000);
        let activity = state.activity.clone().expect("an active turn reports work");
        assert_eq!(activity.status, "Reading file");
        assert_eq!(activity.elapsed_seconds, 0);

        state.sync_activity(4_000);
        let activity = state.activity.clone().expect("the turn is still active");
        assert_eq!(activity.elapsed_seconds, 3);
        assert_eq!(activity.hint(), "(3s Esc/Ctrl+C to interrupt)");

        state.waiting = false;
        state.sync_activity(9_000);
        assert_eq!(state.activity, None, "a settled turn leaves no indicator");

        // The next turn restarts its own clock instead of resuming the old one.
        state.waiting = true;
        state.entries.clear();
        state.sync_activity(20_000);
        let activity = state.activity.clone().expect("the next turn reports work");
        assert_eq!(activity.elapsed_seconds, 0);
        assert_eq!(activity.status, diagnostics::DEFAULT_ACTIVITY_STATUS);
    }
}

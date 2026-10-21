use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::attention::{AttentionEffect, AttentionNotifier};
use super::controls::CallbackPresentation;
use super::debug_console::DebugConsole;
use super::diagnostics::{Activity, ErrorLog};
use super::interaction::{Overlay, PromptQueue, QuitConfirmation};
use super::rewind::RewindState;
use super::session_picker::SessionDeleteState;
use super::transcript_view::TranscriptView;
use vibe_app_server::client::{
    NoticeDetail, PublicEffectState, PublicHistoryEntry, PublicNoticeLevel,
};
use vibe_voice::narrator::NarratorManager;

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
    /// Reference `TodoTracker`, which only a Unified Harness session keeps.
    pub(in crate::tui) todos: Option<super::todos::TodoTracker>,
    /// Where the last frame painted the pinned todo line, which a click opens.
    pub(in crate::tui) todo_row: Option<ratatui::layout::Rect>,
    /// Reference `SubagentList` and the child transcripts it opens.
    pub(in crate::tui) subagents: super::subagents::Subagents,
    /// Where the last frame painted the subagent list's rows, and which row
    /// the first of them is.
    pub(in crate::tui) subagent_rows: Option<(ratatui::layout::Rect, usize)>,
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
            file_watcher_for_autocomplete: false,
            retry_offered: false,
            value_edit: None,
            log_level_picker: None,
            todos: None,
            todo_row: None,
            subagents: super::subagents::Subagents::default(),
            subagent_rows: None,
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
                if let Some(todos) = self.todos.as_mut() {
                    todos.follow(&entry);
                }
                self.subagents.remember_parent_instruction(&entry);
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
                if let Some(todos) = self.todos.as_mut() {
                    todos.follow(&entry);
                }
                self.subagents.remember_parent_instruction(&entry);
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

    /// A canonical resync: the snapshot replaces the server's entries, and
    /// every entry this client wrote keeps the place it was anchored at.
    /// Everything else is the operator's and survives, except what the
    /// replaced revisions derived: the stream deltas, and the scroll position,
    /// which returns to the newest entry. Nothing changes when the snapshot is
    /// rejected.
    pub fn resync(&mut self, mut snapshot: TuiSnapshot) -> Result<(), StateError> {
        // The window survives a resync while the entry it starts at does.
        let first_visible = self
            .history_first_visible
            .clone()
            .filter(|id| snapshot.entries.iter().any(|entry| &entry.id == id));
        // A local copy of an entry the session published, a manual command's,
        // gives way to the canonical one the snapshot now holds.
        let retained = self
            .local_entries
            .iter()
            .filter(|position| {
                !self
                    .entry_indexes
                    .get(&position.entry_id)
                    .and_then(|index| self.entries.get(*index)?.source.server())
                    .is_some_and(|published| {
                        let id = &published.metadata().id;
                        snapshot.entries.iter().any(|entry| &entry.id == id)
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        // Entries written before any server entry keep their place at the top.
        let mut leading = 0;
        for position in &retained {
            let Some(entry) = self
                .entry_indexes
                .get(&position.entry_id)
                .and_then(|index| self.entries.get(*index))
            else {
                continue;
            };
            let insertion_index = match position.after_entry_id.as_ref() {
                None => {
                    leading += 1;
                    leading - 1
                }
                Some(anchor) => snapshot
                    .entries
                    .iter()
                    .position(|candidate| candidate.id == *anchor)
                    .map_or(snapshot.entries.len(), |index| index.saturating_add(1)),
            };
            snapshot.entries.insert(insertion_index, entry.clone());
        }
        self.apply(ServerEvent::Snapshot(snapshot))?;
        self.local_entries = retained;
        self.history_first_visible = first_visible;
        self.effect_streams.clear();
        self.scroll_offset = 0;
        Ok(())
    }

    /// The entry `id` names, when the transcript holds one.
    #[must_use]
    pub fn entry(&self, id: &str) -> Option<&TranscriptEntry> {
        self.entry_indexes
            .get(id)
            .and_then(|index| self.entries.get(*index))
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
        // A running effect first drawn here appended all it streamed so far.
        if let Some(output) =
            super::transcript::running_output(&entry).filter(|output| !output.is_empty())
        {
            self.effect_streams.insert(id.clone(), output.to_owned());
        }
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
mod state_tests;

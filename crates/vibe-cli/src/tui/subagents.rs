//! The subagent list under the composer and the child transcripts it opens.
//!
//! Reference `SubagentList` (`vibe/cli/textual_ui/widgets/chat_input/subagent_list.py`),
//! `SubagentTranscripts` (`vibe/cli/textual_ui/widgets/subagent_transcripts.py`)
//! and the `VibeApp` methods that drive them (`_refresh_subagent_list`,
//! `_show_main_chat`, `_show_subagent_chat`, `_refresh_subagent_transcript`).
//! Only the unified core runs child sessions, so on any other backend the
//! list never has a row to show. The list names the main conversation and the
//! children of the current batch; opening a child swaps the transcript for its
//! own, read only, until Esc or the main row brings the conversation back.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use vibe_core::events::{
    PublicHistoryEntry, PublicMessageRole, PublicMessageSource, PublicNoticeLevel, ToolEffectKind,
};

use super::state::{EntrySource, EntryStatus, TranscriptEntry, TranscriptKind};

/// Reference `MAX_CACHED_SUBAGENT_TRANSCRIPTS`.
const CACHED_TRANSCRIPTS: usize = 5;
/// Reference `HISTORY_RESUME_TAIL_MESSAGES`: the tail a child is first read with.
const HISTORY_TAIL: usize = 20;
/// The page size the rest of a child's history is read in.
const HISTORY_PAGE: usize = 500;
/// Reference `#subagent-list { max-height: 7 }`.
pub(super) const MAX_LIST_HEIGHT: usize = 7;

/// What a child session reports about itself. Reference `PublicSessionStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChildStatus {
    Idle,
    Running,
    Blocked,
    Failed,
    Archived,
    Unknown,
}

impl ChildStatus {
    fn parse(status: &Value) -> Self {
        match status.get("type").and_then(Value::as_str) {
            Some("idle") => Self::Idle,
            Some("running") => Self::Running,
            Some("blocked") => Self::Blocked,
            Some("failed") => Self::Failed,
            Some("archived") => Self::Archived,
            _ => Self::Unknown,
        }
    }

    /// Reference `_status`: the label a row brackets.
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Archived => "stopped",
            Self::Idle => "ready",
            Self::Unknown => "unknown",
        }
    }

    /// Reference `_is_active`.
    const fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Blocked)
    }

    /// Reference `subagent_loading_status`: what the loading line says while a
    /// viewed child works.
    pub(super) const fn loading_status(self) -> Option<&'static str> {
        match self {
            Self::Running => Some("Running"),
            Self::Blocked => Some("Waiting for input"),
            _ => None,
        }
    }
}

/// Reference `PublicChildSession`, as much of it as the terminal shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChildSession {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) agent_type: String,
    pub(super) status: ChildStatus,
    pub(super) context_tokens: u64,
    created_at: i64,
    /// The record as published, which is what a change is detected on.
    raw: Value,
}

impl ChildSession {
    fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_str()?.to_owned(),
            name: value.get("name")?.as_str()?.to_owned(),
            agent_type: value.get("agentType")?.as_str()?.to_owned(),
            status: ChildStatus::parse(value.get("status")?),
            context_tokens: value
                .pointer("/contextUsage/totalTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            created_at: value.get("createdAt").and_then(Value::as_i64).unwrap_or(0),
            raw: value.clone(),
        })
    }
}

/// The children a published session state names, in the reference's order:
/// by creation time, then by identifier (`vibe/app_server/events.py`).
pub(super) fn children_of(state: &Value) -> Vec<ChildSession> {
    let mut children = state
        .get("childSessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(ChildSession::parse)
        .collect::<Vec<_>>();
    children.sort_by(|left, right| (left.created_at, &left.id).cmp(&(right.created_at, &right.id)));
    children
}

/// Reference `_display_name`: a single identifier reads as capitalized words.
pub(super) fn display_name(value: &str) -> String {
    if value.contains(' ') {
        return value.to_owned();
    }
    let characters = value.chars().collect::<Vec<_>>();
    let mut words = Vec::<String>::new();
    let mut word = String::new();
    for (index, &character) in characters.iter().enumerate() {
        if character == '-' || character == '_' {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            continue;
        }
        let previous = index.checked_sub(1).map(|at| characters[at]);
        let next = characters.get(index + 1).copied();
        let boundary = character.is_ascii_uppercase()
            && previous.is_some_and(|previous| {
                previous.is_ascii_lowercase()
                    || previous.is_ascii_digit()
                    || (previous.is_ascii_uppercase()
                        && next.is_some_and(|next| next.is_ascii_lowercase()))
            });
        if boundary && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        word.push(character);
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
        .into_iter()
        .map(|word| {
            if word.chars().all(|character| !character.is_uppercase()) {
                let mut characters = word.chars();
                characters.next().map_or_else(String::new, |first| {
                    first.to_uppercase().chain(characters).collect()
                })
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One child's transcript as the terminal last read it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ChildTranscript {
    /// `None` until the first read lands.
    history: Option<Vec<TranscriptEntry>>,
    /// Whether `history` reaches the child's first entry.
    complete: bool,
    /// Lines this terminal wrote under the child's history.
    local: Vec<TranscriptEntry>,
    /// The child could not be read and nothing was read before.
    closed: bool,
}

/// What a key did to the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ListKey {
    /// The list did not take the key.
    Ignored,
    /// The list took the key.
    Handled,
    /// The list gave the keyboard back to the composer.
    FocusInput,
}

/// Reference `SubagentList` with the parts of `VibeApp` that own its views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) struct Subagents {
    /// Reference `show_subagent_status_list`.
    pub(super) enabled: bool,
    /// Every child the session publishes, as last seen.
    all: Vec<ChildSession>,
    /// The children the list shows.
    shown: Vec<ChildSession>,
    known: BTreeSet<String>,
    batch: BTreeSet<String>,
    viewed: Option<String>,
    focused: bool,
    /// The highlighted row: 0 is the main conversation.
    highlighted: usize,
    /// Least recently viewed first.
    transcripts: Vec<(String, ChildTranscript)>,
    parent_instructions: BTreeMap<String, TranscriptEntry>,
    active: BTreeSet<String>,
    /// The viewed child's transcript must be read again.
    refresh: bool,
    /// Each view's scroll position, kept while another is shown.
    scrolls: BTreeMap<Option<String>, usize>,
    /// The view the last frame painted.
    painted_view: Option<String>,
}

impl Default for Subagents {
    fn default() -> Self {
        Self {
            enabled: true,
            all: Vec::new(),
            shown: Vec::new(),
            known: BTreeSet::new(),
            batch: BTreeSet::new(),
            viewed: None,
            focused: false,
            highlighted: 0,
            transcripts: Vec::new(),
            parent_instructions: BTreeMap::new(),
            active: BTreeSet::new(),
            refresh: false,
            scrolls: BTreeMap::new(),
            painted_view: None,
        }
    }
}

impl Subagents {
    /// The children the list shows, after the main conversation's row.
    pub(super) fn shown(&self) -> &[ChildSession] {
        &self.shown
    }

    /// Whether the list has rows to paint.
    pub(super) fn visible(&self) -> bool {
        !self.shown.is_empty()
    }

    pub(super) const fn focused(&self) -> bool {
        self.focused
    }

    pub(super) const fn highlighted(&self) -> usize {
        self.highlighted
    }

    /// The child whose transcript replaces the conversation, if one does.
    pub(super) fn viewed(&self) -> Option<&ChildSession> {
        let id = self.viewed.as_deref()?;
        self.all.iter().find(|child| child.id == id)
    }

    pub(super) const fn is_viewing(&self) -> bool {
        self.viewed.is_some()
    }

    /// Adopts what the session now publishes. Reference `_on_app_server_event`
    /// for `ChildSessionUpdated`, then `_refresh_subagent_list`.
    pub(super) fn sync(&mut self, children: Vec<ChildSession>) {
        for child in &children {
            let changed = self
                .all
                .iter()
                .find(|known| known.id == child.id)
                .is_none_or(|known| known.raw != child.raw);
            if !changed {
                continue;
            }
            self.announce_ready_transition(child);
            if self.viewed.as_deref() == Some(child.id.as_str()) {
                self.refresh = true;
            }
        }
        self.all = children;
        self.refresh_list();
    }

    /// Reference `_refresh_subagent_list`.
    fn refresh_list(&mut self) {
        if !self.enabled {
            if self.viewed.is_some() {
                self.show_main(true);
                return;
            }
            self.update_sessions(&[], None);
            return;
        }
        if let Some(viewed) = &self.viewed
            && self.all.iter().all(|child| &child.id != viewed)
        {
            self.show_main(true);
            return;
        }
        let all = self.all.clone();
        let viewed = self.viewed.clone();
        self.update_sessions(&all, viewed.as_deref());
    }

    /// Reference `SubagentList.update_sessions`.
    fn update_sessions(&mut self, sessions: &[ChildSession], selected: Option<&str>) {
        let previous_selected = self.viewed_row_id();
        let highlighted_id = self.row_id(self.highlighted);
        self.shown = self.active_batch(sessions, selected);
        if self.shown.is_empty() {
            self.focused = false;
            self.highlighted = 0;
            return;
        }
        let rows = std::iter::once(None)
            .chain(self.shown.iter().map(|child| Some(child.id.clone())))
            .collect::<Vec<_>>();
        let selected_row = selected.map(ToOwned::to_owned);
        let preferred = if previous_selected == selected_row && rows.contains(&highlighted_id) {
            highlighted_id
        } else {
            selected_row
        };
        self.highlighted = rows.iter().position(|row| *row == preferred).unwrap_or(0);
    }

    /// Reference `_active_batch`: while any child works, the list keeps every
    /// child of that batch; once all are done, only the ready ones stay.
    fn active_batch(
        &mut self,
        sessions: &[ChildSession],
        selected: Option<&str>,
    ) -> Vec<ChildSession> {
        let current = sessions
            .iter()
            .map(|child| child.id.clone())
            .collect::<BTreeSet<_>>();
        let new = current.difference(&self.known).cloned().collect::<Vec<_>>();
        self.known.clone_from(&current);
        self.batch.retain(|id| current.contains(id));
        let active = sessions
            .iter()
            .filter(|child| child.status.is_active())
            .map(|child| child.id.clone())
            .collect::<Vec<_>>();
        if !active.is_empty() {
            self.batch.extend(active);
            self.batch.extend(new);
        } else if selected.is_none_or(|selected| !self.batch.contains(selected)) {
            self.batch.clear();
        }
        sessions
            .iter()
            .filter(|child| self.batch.contains(&child.id) || child.status == ChildStatus::Idle)
            .cloned()
            .collect()
    }

    fn row_id(&self, row: usize) -> Option<String> {
        row.checked_sub(1)
            .and_then(|index| self.shown.get(index))
            .map(|child| child.id.clone())
    }

    fn viewed_row_id(&self) -> Option<String> {
        self.viewed.clone()
    }

    /// Reference `ChatTextArea.NavigateBelow` reaching `focus_first`: Down on
    /// the composer's last line moves onto the main row, when there is a list.
    pub(super) fn focus_first(&mut self) -> bool {
        if !self.visible() {
            return false;
        }
        self.highlighted = 0;
        self.focused = true;
        true
    }

    /// The keys the list takes while it holds the keyboard.
    pub(super) fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> ListKey {
        use crossterm::event::{KeyCode, KeyModifiers};
        if !self.focused {
            return ListKey::Ignored;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return ListKey::Ignored;
        }
        match key.code {
            // Reference `action_cursor_up`: past the main row, a list beside
            // the conversation hands the keyboard back.
            KeyCode::Up => {
                if self.highlighted == 0 {
                    if self.viewed.is_none() {
                        self.focused = false;
                        return ListKey::FocusInput;
                    }
                } else {
                    self.highlighted -= 1;
                }
                ListKey::Handled
            }
            KeyCode::Down => {
                if self.highlighted < self.shown.len() {
                    self.highlighted += 1;
                }
                ListKey::Handled
            }
            KeyCode::Enter => {
                self.select_row(self.highlighted);
                if self.focused {
                    ListKey::Handled
                } else {
                    ListKey::FocusInput
                }
            }
            // Esc belongs to the interrupt ladder, which returns from a view.
            KeyCode::Esc => ListKey::Ignored,
            // Reference: the list holds the focus, so typing reaches nothing.
            _ => ListKey::Handled,
        }
    }

    /// Reference `on_option_list_option_selected` then `on_subagent_list_selected`.
    pub(super) fn select_row(&mut self, row: usize) {
        let target = self.row_id(row);
        self.highlighted = row.min(self.shown.len());
        if target == self.viewed {
            return;
        }
        match target {
            None => self.show_main(false),
            Some(id) if self.all.iter().any(|child| child.id == id) => self.show_child(id),
            Some(_) => self.show_main(true),
        }
    }

    /// Reference `_show_main_chat`. `focus_input` hands the keyboard back.
    pub(super) fn show_main(&mut self, focus_input: bool) {
        if let Some(previous) = self.viewed.take()
            && let Some(transcript) = self.transcript_mut(&previous)
        {
            transcript.complete = false;
        }
        if focus_input {
            self.focused = false;
        }
        let shown = if self.enabled {
            self.all.clone()
        } else {
            Vec::new()
        };
        self.update_sessions(&shown, None);
    }

    /// Reference `_show_subagent_chat`.
    fn show_child(&mut self, id: String) {
        if !self.enabled {
            return;
        }
        if let Some(previous) = self.viewed.replace(id.clone())
            && previous != id
            && let Some(transcript) = self.transcript_mut(&previous)
        {
            transcript.complete = false;
        }
        self.focused = true;
        self.prepare(&id);
        self.refresh = true;
        let all = self.all.clone();
        self.update_sessions(&all, Some(&id));
    }

    /// Reference `_remember_transcript_scroll` and `_restore_transcript_scroll`:
    /// once the view changes, the scroll position it left is kept and the one
    /// it shows is restored, or anchored at the bottom when it has none.
    pub(super) fn settle_scroll(&mut self, scroll: usize) -> usize {
        if self.painted_view == self.viewed {
            return scroll;
        }
        let leaving = std::mem::replace(&mut self.painted_view, self.viewed.clone());
        self.scrolls.insert(leaving, scroll);
        self.scrolls.get(&self.viewed).copied().unwrap_or(0)
    }

    /// Takes the request to read the viewed child again.
    pub(super) fn take_refresh(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.refresh) {
            return None;
        }
        self.viewed.clone()
    }

    fn prepare(&mut self, id: &str) {
        if let Some(index) = self.transcripts.iter().position(|(known, _)| known == id) {
            let entry = self.transcripts.remove(index);
            self.transcripts.push(entry);
            return;
        }
        self.transcripts
            .push((id.to_owned(), ChildTranscript::default()));
        // Reference `_evict_overflow`: the viewed transcript is never evicted.
        while self.transcripts.len() > CACHED_TRANSCRIPTS {
            let Some(index) = self
                .transcripts
                .iter()
                .position(|(known, _)| self.viewed.as_deref() != Some(known.as_str()))
            else {
                break;
            };
            self.transcripts.remove(index);
        }
    }

    fn transcript(&self, id: &str) -> Option<&ChildTranscript> {
        self.transcripts
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, transcript)| transcript)
    }

    fn transcript_mut(&mut self, id: &str) -> Option<&mut ChildTranscript> {
        self.transcripts
            .iter_mut()
            .find(|(known, _)| known == id)
            .map(|(_, transcript)| transcript)
    }

    /// Reference `needs_complete_history`: a tail that shares nothing with what
    /// was shown, or a transcript never read whole, is read from its start.
    fn needs_complete_history(&self, id: &str, latest: &[TranscriptEntry]) -> bool {
        let Some(transcript) = self.transcript(id).filter(|transcript| transcript.complete) else {
            return true;
        };
        let Some(history) = transcript.history.as_ref().filter(|_| !latest.is_empty()) else {
            return false;
        };
        let known = history
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<BTreeSet<_>>();
        !latest.iter().any(|entry| known.contains(entry.id.as_str()))
    }

    /// Reference `replace_history`.
    fn replace_history(&mut self, id: &str, latest: Vec<TranscriptEntry>, complete: bool) {
        self.prepare(id);
        if let Some(instruction) = latest.iter().find(|entry| is_turn_start(entry)) {
            self.parent_instructions
                .insert(id.to_owned(), instruction.clone());
        }
        let instruction = self.parent_instructions.get(id).cloned();
        let Some(transcript) = self.transcript_mut(id) else {
            return;
        };
        let mut rendered = match transcript.history.as_ref() {
            Some(history) if transcript.complete && !complete => merge_tail(history, latest),
            _ => latest,
        };
        if let Some(instruction) = instruction
            && rendered.iter().all(|entry| entry.id != instruction.id)
        {
            rendered.insert(0, instruction);
        }
        transcript.complete = complete || transcript.complete;
        transcript.history = Some(rendered);
        transcript.closed = false;
    }

    /// Reference `show_error`: a child that cannot be read, and was never read.
    fn show_error(&mut self, id: &str) {
        self.prepare(id);
        if let Some(transcript) = self.transcript_mut(id)
            && transcript.history.is_none()
        {
            transcript.closed = true;
        }
    }

    /// Reference `_remember_subagent_instruction`: the task a spawn handed its
    /// child opens the child's transcript, as the child never records it.
    pub(super) fn remember_parent_instruction(&mut self, entry: &TranscriptEntry) {
        let Some(PublicHistoryEntry::Effect { detail, .. }) = entry.source.server() else {
            return;
        };
        if detail.kind != ToolEffectKind::Subagent || detail.tool_name != "subagent.spawn" {
            return;
        }
        let Some(child) = detail.child_session_id.as_deref() else {
            return;
        };
        let Some(task) = detail.input.get("task").and_then(Value::as_str) else {
            return;
        };
        self.parent_instructions
            .entry(child.to_owned())
            .or_insert_with(|| TranscriptEntry {
                id: format!("parent-instruction:{child}"),
                revision: 1,
                kind: TranscriptKind::UserMessage,
                text: task.to_owned(),
                status: EntryStatus::Completed,
                source: EntrySource::Restored,
            });
    }

    /// Reference `announce_ready_transition`: a child that worked and is now
    /// ready says so once under its transcript.
    fn announce_ready_transition(&mut self, child: &ChildSession) {
        if child.status.is_active() {
            self.active.insert(child.id.clone());
            return;
        }
        if child.status != ChildStatus::Idle {
            self.active.remove(&child.id);
            return;
        }
        if self.active.remove(&child.id) {
            self.append_local(&child.id, READY_MESSAGE, PublicNoticeLevel::Info);
        }
    }

    /// Reference `_show_subagent_read_only_message`.
    pub(super) fn refuse_input(&mut self) {
        if let Some(id) = self.viewed.clone() {
            self.append_local(&id, READ_ONLY_MESSAGE, PublicNoticeLevel::Error);
        }
    }

    fn append_local(&mut self, id: &str, text: &str, level: PublicNoticeLevel) {
        self.prepare(id);
        if let Some(transcript) = self.transcript_mut(id) {
            let index = transcript.local.len();
            transcript.local.push(TranscriptEntry {
                id: format!("subagent-local:{id}:{index}"),
                revision: 1,
                kind: TranscriptKind::Notice,
                text: text.to_owned(),
                status: EntryStatus::Completed,
                source: EntrySource::notice(level),
            });
        }
    }

    /// What the transcript area shows while `id` is viewed.
    pub(super) fn view_entries(&self) -> Option<Vec<TranscriptEntry>> {
        let id = self.viewed.as_deref()?;
        let line = |text: &str| TranscriptEntry {
            id: format!("subagent-placeholder:{id}"),
            revision: 1,
            kind: TranscriptKind::Notice,
            text: text.to_owned(),
            status: EntryStatus::Completed,
            source: EntrySource::notice(PublicNoticeLevel::Info),
        };
        let Some(transcript) = self.transcript(id) else {
            return Some(vec![line(LOADING_MESSAGE)]);
        };
        if transcript.closed {
            return Some(vec![line(CLOSED_MESSAGE)]);
        }
        let Some(history) = transcript.history.as_ref() else {
            let mut entries = vec![line(LOADING_MESSAGE)];
            entries.extend(transcript.local.iter().cloned());
            return Some(entries);
        };
        let mut entries = history.clone();
        entries.extend(transcript.local.iter().cloned());
        if entries.is_empty() {
            entries.push(line(EMPTY_MESSAGE));
        }
        Some(entries)
    }
}

/// Follows the session's children once per loop turn, and reads the viewed
/// child again when it changed.
pub(super) fn poll(
    runtime: &mut super::runtime::InteractiveRuntime,
    state: &mut super::state::TuiState,
) {
    let children = runtime
        .service
        .unified_children(&runtime.session_id)
        .map(|children| children_of(&json!({"childSessions": children})))
        .unwrap_or_default();
    state.subagents.sync(children);
    if let Some(id) = state.subagents.take_refresh() {
        let service = &mut runtime.service;
        refresh_transcript(&mut state.subagents, &id, &mut |method, params| {
            service
                .public_call(method, params)
                .map(super::map_value)
                .map_err(|error| error.to_string())
        });
    }
}

/// Reference `_refresh_subagent_transcript`: the child's latest tail, and its
/// whole history when the tail no longer meets what was shown.
pub(super) fn refresh_transcript(
    subagents: &mut Subagents,
    id: &str,
    read: &mut dyn FnMut(&str, Value) -> Result<Value, String>,
) {
    let tail = read(
        "session/read",
        json!({"sessionId": id, "history": {"limit": HISTORY_TAIL}}),
    )
    .and_then(|result| entries_at(&result, "/state/history"));
    let mut history = match tail {
        Ok(history) => history,
        Err(_) => {
            if subagents.viewed.as_deref() == Some(id) {
                subagents.show_error(id);
            }
            return;
        }
    };
    let mut complete = history.len() < HISTORY_TAIL;
    if !complete && subagents.needs_complete_history(id, &history) {
        let mut cursor = history.first().map(|entry| entry.id.clone());
        while let Some(before) = cursor {
            let page = read(
                "session/history/list",
                json!({"sessionId": id, "page": {"cursor": before, "limit": HISTORY_PAGE}}),
            );
            let Ok(page) = page else {
                if subagents.viewed.as_deref() == Some(id) {
                    subagents.show_error(id);
                }
                return;
            };
            let Ok(mut older) = entries_at(&page, "/items") else {
                return;
            };
            older.append(&mut history);
            history = older;
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
        complete = true;
    }
    if subagents.viewed.as_deref() != Some(id) {
        return;
    }
    subagents.replace_history(id, history, complete);
}

fn entries_at(result: &Value, pointer: &str) -> Result<Vec<TranscriptEntry>, String> {
    let items = result
        .pointer(pointer)
        .cloned()
        .unwrap_or_else(|| json!([]));
    serde_json::from_value::<Vec<PublicHistoryEntry>>(items)
        .map(|entries| {
            entries
                .into_iter()
                .map(super::hydration::history_entry)
                .collect()
        })
        .map_err(|error| error.to_string())
}

/// A user message that opened a turn: the instruction a child was given.
fn is_turn_start(entry: &TranscriptEntry) -> bool {
    matches!(
        entry.source.server(),
        Some(PublicHistoryEntry::Message {
            role: PublicMessageRole::User,
            source: Some(PublicMessageSource::TurnStart),
            ..
        })
    )
}

/// Reference `_merge_history_tail`: what was shown up to where the tail starts,
/// then the tail.
fn merge_tail(history: &[TranscriptEntry], latest: Vec<TranscriptEntry>) -> Vec<TranscriptEntry> {
    if latest.is_empty() {
        return Vec::new();
    }
    let overlap = latest
        .iter()
        .find_map(|entry| history.iter().position(|known| known.id == entry.id))
        .unwrap_or(history.len());
    history[..overlap].iter().cloned().chain(latest).collect()
}

/// Said in a child's transcript when the operator tries to write to it.
const READ_ONLY_MESSAGE: &str = "A subagent only takes instructions from the main agent. Go back \
     to Main conversation and ask the main agent to stop it.";
/// Said in a child's transcript once it finishes its work.
const READY_MESSAGE: &str = "This subagent finished and is waiting. Go back to Main conversation \
     and ask the main agent to give it a new goal or to stop it.";
const LOADING_MESSAGE: &str = "Reading the subagent's transcript…";
const EMPTY_MESSAGE: &str = "Nothing has been written to this transcript yet.";
const CLOSED_MESSAGE: &str = "This subagent has closed, and its transcript can no longer be read.";

#[cfg(test)]
#[path = "subagents_tests.rs"]
mod subagents_tests;

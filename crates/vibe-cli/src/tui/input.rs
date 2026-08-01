use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fs;
use std::fs::OpenOptions;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use vibe_app_server::client::{PublicContentBlock, TurnRequest};

use super::commands::{command_aliases, command_description};

pub const MAX_PASTE_BYTES: usize = 256 * 1024;
pub const MAX_RESOURCE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_COMPLETION_CANDIDATES: usize = 64;
pub const MAX_VISIBLE_COMPLETIONS: usize = 10;
pub(crate) const COMPOSER_HORIZONTAL_OVERHEAD: u16 = 9;
const MAX_COMPLETION_SCAN_ENTRIES: usize = 4_096;
const IMAGE_EXTENSIONS: &[&str] = &["gif", "jpeg", "jpg", "png", "webp"];

#[must_use]
pub(crate) fn composer_content_width(viewport_width: u16) -> usize {
    usize::from(
        viewport_width
            .saturating_sub(COMPOSER_HORIZONTAL_OVERHEAD)
            .max(1),
    )
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptEditor {
    text: String,
    cursor: usize,
    selection: Option<Range<usize>>,
    selection_anchor: Option<usize>,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
    history_loaded: bool,
    cursor_moved_since_history_load: bool,
    discard_joined_character: bool,
}

impl PromptEditor {
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    #[must_use]
    pub fn selection(&self) -> Option<Range<usize>> {
        self.selection.clone()
    }

    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        let start = grapheme_byte(&self.text, selection.start);
        let end = grapheme_byte(&self.text, selection.end);
        (start < end).then(|| self.text[start..end].to_owned())
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = grapheme_count(&self.text);
        self.selection = None;
        self.selection_anchor = None;
        self.reset_history_navigation();
        self.discard_joined_character = false;
    }

    pub fn select(&mut self, range: Range<usize>) {
        let end = grapheme_count(&self.text);
        let start = range.start.min(end);
        let target = range.end.min(end);
        self.selection_anchor = Some(start);
        self.selection = Some(start.min(target)..start.max(target));
        self.mark_history_cursor_moved(target);
        self.cursor = target;
    }

    pub fn insert(&mut self, text: &str) {
        self.delete_selection();
        let byte = grapheme_byte(&self.text, self.cursor);
        self.text.insert_str(byte, text);
        self.cursor = self.cursor.saturating_add(grapheme_count(text));
        self.finish_edit();
    }

    /// Inserts one terminal key character using Textual-compatible filtering.
    ///
    /// Combining/control-width scalar events are ignored by the reference
    /// widget. A ZWJ also causes the following scalar to be ignored because
    /// Textual receives that attempted sequence as one unsupported key.
    pub fn insert_key_character(&mut self, character: char) -> bool {
        if self.discard_joined_character {
            self.discard_joined_character = false;
            return false;
        }
        if UnicodeWidthChar::width(character).is_none_or(|width| width == 0) {
            self.discard_joined_character = character == '\u{200d}';
            return false;
        }
        self.insert(&character.to_string());
        true
    }

    pub fn paste(&mut self, text: &str) -> Result<(), InputError> {
        if text.len() > MAX_PASTE_BYTES {
            return Err(InputError::PasteTooLarge {
                bytes: text.len(),
                limit: MAX_PASTE_BYTES,
            });
        }
        self.insert(text);
        Ok(())
    }

    pub fn move_left(&mut self, extend_selection: bool) {
        self.move_left_bounded(extend_selection, 0);
    }

    pub fn move_left_bounded(&mut self, extend_selection: bool, floor: usize) {
        self.move_cursor(self.cursor.saturating_sub(1).max(floor), extend_selection);
    }

    pub fn move_right(&mut self, extend_selection: bool) {
        self.move_cursor(
            self.cursor
                .saturating_add(1)
                .min(grapheme_count(&self.text)),
            extend_selection,
        );
    }

    pub fn move_home(&mut self, extend_selection: bool) {
        self.move_home_bounded(extend_selection, 0);
    }

    pub fn move_home_bounded(&mut self, extend_selection: bool, floor: usize) {
        let start = self
            .text
            .graphemes(true)
            .enumerate()
            .take(self.cursor)
            .filter_map(|(index, grapheme)| (grapheme == "\n").then_some(index))
            .last()
            .map_or(0, |index| index.saturating_add(1));
        self.move_cursor(start.max(floor), extend_selection);
    }

    pub fn move_end(&mut self, extend_selection: bool) {
        let end = self
            .text
            .graphemes(true)
            .enumerate()
            .skip(self.cursor)
            .find_map(|(index, grapheme)| (grapheme == "\n").then_some(index))
            .unwrap_or_else(|| grapheme_count(&self.text));
        self.move_cursor(end, extend_selection);
    }

    pub fn move_word_left(&mut self) {
        self.move_word_left_bounded(0);
    }

    pub fn move_word_left_bounded(&mut self, floor: usize) {
        let graphemes = self.text.graphemes(true).collect::<Vec<_>>();
        let mut target = self.cursor.min(graphemes.len());
        while target > floor && !is_word_grapheme(graphemes[target - 1]) {
            target -= 1;
        }
        while target > floor && is_word_grapheme(graphemes[target - 1]) {
            target -= 1;
        }
        self.move_cursor(target, false);
    }

    pub fn move_word_right(&mut self) {
        let graphemes = self.text.graphemes(true).collect::<Vec<_>>();
        let mut target = self.cursor.min(graphemes.len());
        while target < graphemes.len() && !is_word_grapheme(graphemes[target]) {
            target += 1;
        }
        while target < graphemes.len() && is_word_grapheme(graphemes[target]) {
            target += 1;
        }
        self.move_cursor(target, false);
    }

    /// Moves by a rendered visual line, preserving the current cell column.
    pub fn move_visual_up(&mut self, width: usize, prefix: usize) -> bool {
        self.move_visual_line(width, prefix, true)
    }

    /// Moves by a rendered visual line, preserving the current cell column.
    pub fn move_visual_down(&mut self, width: usize, prefix: usize) -> bool {
        self.move_visual_line(width, prefix, false)
    }

    /// Places the cursor at a visual cell. Coordinates are relative to the
    /// editor body; out-of-bounds positions are rejected without mutation.
    pub fn move_to_visual_cell(
        &mut self,
        row: usize,
        column: usize,
        width: usize,
        prefix: usize,
        extend_selection: bool,
    ) -> bool {
        let lines = visual_lines(&self.text, width.max(1), prefix);
        let Some(line) = lines.get(row) else {
            return false;
        };
        let target = cursor_at_cell(&self.text, line.clone(), column);
        self.move_cursor(target, extend_selection);
        true
    }

    pub fn delete_backward(&mut self) {
        self.delete_backward_bounded(0);
    }

    pub fn delete_backward_bounded(&mut self, floor: usize) {
        if self.delete_selection() {
            self.finish_edit();
            return;
        }
        if self.cursor <= floor {
            return;
        }
        let start = grapheme_byte(&self.text, self.cursor - 1);
        let end = grapheme_byte(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
        self.finish_edit();
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            self.finish_edit();
            return;
        }
        if self.cursor == grapheme_count(&self.text) {
            return;
        }
        let start = grapheme_byte(&self.text, self.cursor);
        let end = grapheme_byte(&self.text, self.cursor + 1);
        self.text.replace_range(start..end, "");
        self.finish_edit();
    }

    pub fn replace_history(&mut self, entries: Vec<String>) {
        self.history = entries
            .into_iter()
            .rev()
            .take(100)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        self.reset_history_navigation();
    }

    #[must_use]
    pub fn history_entries(&self) -> &[String] {
        &self.history
    }

    pub fn history_previous(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let index = match self.history_index {
            Some(0) => return false,
            Some(index) => index - 1,
            None => {
                self.draft.clone_from(&self.text);
                self.history.len() - 1
            }
        };
        self.history_index = Some(index);
        let entry = self.history[index].clone();
        self.load_history_text(entry);
        true
    }

    pub fn history_next(&mut self) -> bool {
        let Some(index) = self.history_index else {
            return false;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            let entry = self.history[index + 1].clone();
            self.load_history_text(entry);
        } else {
            self.history_index = None;
            self.load_history_text(self.draft.clone());
        }
        true
    }

    pub fn submit(&mut self) -> Option<String> {
        let submitted = self.text.trim().to_owned();
        if submitted.is_empty() {
            return None;
        }
        self.text.clear();
        self.cursor = 0;
        self.selection = None;
        self.selection_anchor = None;
        self.reset_history_navigation();
        if self.history.last() != Some(&submitted) {
            self.history.push(submitted.clone());
            if self.history.len() > 100 {
                self.history.remove(0);
            }
        }
        Some(submitted)
    }

    pub fn take_unrecorded(&mut self) -> Option<String> {
        let submitted = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.selection = None;
        self.selection_anchor = None;
        self.reset_history_navigation();
        (!submitted.trim().is_empty()).then_some(submitted)
    }

    /// Whether Up/Down are currently walking history rather than the draft.
    #[must_use]
    pub fn history_navigating(&self) -> bool {
        self.history_index.is_some()
    }

    #[must_use]
    pub fn history_loaded(&self) -> bool {
        self.history_loaded
    }

    #[must_use]
    pub fn cursor_moved_since_history_load(&self) -> bool {
        self.cursor_moved_since_history_load
    }

    pub fn reset_history_navigation(&mut self) {
        self.history_index = None;
        self.draft.clear();
        self.history_loaded = false;
        self.cursor_moved_since_history_load = false;
    }

    fn load_history_text(&mut self, text: String) {
        self.text = text;
        self.cursor = self
            .text
            .graphemes(true)
            .position(|grapheme| grapheme == "\n")
            .unwrap_or_else(|| grapheme_count(&self.text));
        self.selection = None;
        self.selection_anchor = None;
        self.history_loaded = true;
        self.cursor_moved_since_history_load = false;
        self.discard_joined_character = false;
    }

    fn move_visual_line(&mut self, width: usize, prefix: usize, up: bool) -> bool {
        let lines = visual_lines(&self.text, width.max(1), prefix);
        let Some(line_index) = lines
            .iter()
            .rposition(|line| line.start <= self.cursor && self.cursor <= line.end)
        else {
            return false;
        };
        let target_index = if up {
            line_index.checked_sub(1)
        } else if line_index + 1 < lines.len() {
            Some(line_index + 1)
        } else {
            None
        };
        let Some(target_index) = target_index else {
            return false;
        };
        let column = visual_cell_width(&self.text, lines[line_index].start..self.cursor);
        let target = cursor_at_cell(&self.text, lines[target_index].clone(), column);
        self.move_cursor(target, false);
        true
    }

    fn move_cursor(&mut self, target: usize, extend_selection: bool) {
        if extend_selection {
            let anchor = *self.selection_anchor.get_or_insert(self.cursor);
            self.selection = (anchor != target).then_some(anchor.min(target)..anchor.max(target));
        } else {
            self.selection = None;
            self.selection_anchor = None;
        }
        self.mark_history_cursor_moved(target);
        self.cursor = target;
    }

    fn delete_selection(&mut self) -> bool {
        let Some(selection) = self.selection.take() else {
            return false;
        };
        self.selection_anchor = None;
        if selection.is_empty() {
            return false;
        }
        let start = grapheme_byte(&self.text, selection.start);
        let end = grapheme_byte(&self.text, selection.end);
        self.text.replace_range(start..end, "");
        self.cursor = selection.start;
        true
    }

    fn finish_edit(&mut self) {
        self.reset_history_navigation();
        self.discard_joined_character = false;
    }

    fn mark_history_cursor_moved(&mut self, target: usize) {
        if self.history_loaded && target != self.cursor {
            self.cursor_moved_since_history_load = true;
        }
    }
}

fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

fn grapheme_byte(text: &str, index: usize) -> usize {
    text.grapheme_indices(true)
        .nth(index)
        .map_or(text.len(), |(byte, _)| byte)
}

fn is_word_grapheme(grapheme: &str) -> bool {
    grapheme
        .chars()
        .all(|character| character == '_' || character.is_alphanumeric())
}

pub(crate) fn visual_cell_width(text: &str, range: Range<usize>) -> usize {
    text.graphemes(true)
        .enumerate()
        .filter(|(index, _)| range.contains(index))
        .map(|(_, grapheme)| UnicodeWidthStr::width(grapheme).max(1))
        .sum()
}

fn cursor_at_cell(text: &str, line: Range<usize>, column: usize) -> usize {
    let graphemes = text.graphemes(true).collect::<Vec<_>>();
    let mut cursor = line.start;
    let mut cells = 0usize;
    while cursor < line.end {
        let width = UnicodeWidthStr::width(graphemes[cursor]).max(1);
        if cells.saturating_add(width) > column {
            break;
        }
        cells = cells.saturating_add(width);
        cursor += 1;
    }
    cursor
}

/// Grapheme ranges for Textual-style word wrapping. A soft wrap starts after
/// the latest whitespace that fits; hard newlines always start a new range.
pub(crate) fn visual_lines(text: &str, width: usize, prefix: usize) -> Vec<Range<usize>> {
    let graphemes = text.graphemes(true).collect::<Vec<_>>();
    let start = prefix.min(graphemes.len());
    let mut starts = vec![start];
    let mut line_start = start;
    let mut line_width = 0usize;
    let mut last_break = None;
    let mut index = start;
    while index < graphemes.len() {
        let grapheme = graphemes[index];
        if grapheme == "\n" {
            let next = index.saturating_add(1);
            starts.push(next);
            line_start = next;
            line_width = 0;
            last_break = None;
            index = next;
            continue;
        }
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if line_width.saturating_add(grapheme_width) > width {
            let next = last_break
                .filter(|candidate| *candidate > line_start)
                .unwrap_or(index);
            if starts.last().copied() != Some(next) {
                starts.push(next);
            }
            line_start = next;
            line_width = visual_cell_width(text, next..index);
            last_break = (next..index)
                .rev()
                .find(|candidate| graphemes[*candidate].chars().all(char::is_whitespace))
                .map(|candidate| candidate.saturating_add(1));
        }
        line_width = line_width.saturating_add(grapheme_width);
        if grapheme.chars().all(char::is_whitespace) {
            last_break = Some(index.saturating_add(1));
        }
        index += 1;
    }
    let end = graphemes.len();
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let mut line_end = starts.get(index + 1).copied().unwrap_or(end);
            if line_end > *start && graphemes.get(line_end - 1) == Some(&"\n") {
                line_end -= 1;
            }
            *start..line_end
        })
        .collect()
}

#[must_use]
pub(crate) fn visual_cursor_cell(
    text: &str,
    cursor: usize,
    width: usize,
    prefix: usize,
) -> (usize, usize) {
    let lines = visual_lines(text, width.max(1), prefix);
    let row = lines
        .iter()
        .rposition(|line| line.start <= cursor && cursor <= line.end)
        .unwrap_or_default();
    let column = lines
        .get(row)
        .map_or(0, |line| visual_cell_width(text, line.start..cursor));
    (row, column)
}

#[must_use]
pub(crate) fn visual_text_lines(text: &str, width: usize, prefix: usize) -> Vec<String> {
    let graphemes = text.graphemes(true).collect::<Vec<_>>();
    visual_lines(text, width.max(1), prefix)
        .into_iter()
        .map(|line| graphemes[line].concat())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionKind {
    SlashCommand,
    Path,
    Agent,
    Skill,
    Mention,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionCandidate {
    pub id: String,
    pub kind: CompletionKind,
    pub label: String,
    pub insertion: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub generation: u64,
    pub candidates: Vec<CompletionCandidate>,
}

#[derive(Debug, Default)]
pub struct CompletionEngine {
    generation: u64,
    active: Option<ActiveCompletion>,
    worker: Option<CompletionWorker>,
    user_skills: Vec<CompletionCandidate>,
}

#[derive(Debug, Clone)]
struct ActiveCompletion {
    token_range: Range<usize>,
    result: CompletionResult,
    selected: usize,
}

#[derive(Debug)]
struct CompletionWorker {
    state: Arc<(Mutex<CompletionWorkerState>, Condvar)>,
    results: Receiver<CompletionResponse>,
}

#[derive(Debug, Default)]
struct CompletionWorkerState {
    pending: Option<CompletionRequest>,
    shutdown: bool,
}

#[derive(Debug)]
struct CompletionRequest {
    generation: u64,
    token_range: Range<usize>,
    query: String,
    workspace: PathBuf,
}

#[derive(Debug)]
struct CompletionResponse {
    generation: u64,
    token_range: Range<usize>,
    query: String,
    candidates: Result<Vec<CompletionCandidate>, InputError>,
}

impl CompletionWorker {
    fn spawn() -> Result<Self, InputError> {
        let state = Arc::new((Mutex::new(CompletionWorkerState::default()), Condvar::new()));
        let thread_state = Arc::clone(&state);
        let (results_tx, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("vibe-path-completion".to_owned())
            .spawn(move || completion_worker_loop(&thread_state, &results_tx))
            .map_err(|error| InputError::CompletionWorker(error.to_string()))?;
        Ok(Self { state, results })
    }

    fn submit(&self, request: CompletionRequest) -> Result<(), InputError> {
        let (state, ready) = self.state.as_ref();
        let mut state = state
            .lock()
            .map_err(|_| InputError::CompletionWorker("worker lock is poisoned".to_owned()))?;
        state.pending = Some(request);
        ready.notify_one();
        Ok(())
    }

    fn cancel_pending(&self) {
        let (state, _) = self.state.as_ref();
        if let Ok(mut state) = state.lock() {
            state.pending = None;
        }
    }
}

impl Drop for CompletionWorker {
    fn drop(&mut self) {
        let (state, ready) = self.state.as_ref();
        if let Ok(mut state) = state.lock() {
            state.shutdown = true;
            state.pending = None;
            ready.notify_one();
        }
    }
}

fn completion_worker_loop(
    shared: &Arc<(Mutex<CompletionWorkerState>, Condvar)>,
    results: &mpsc::Sender<CompletionResponse>,
) {
    loop {
        let request = {
            let (state, ready) = shared.as_ref();
            let Ok(mut state) = state.lock() else {
                return;
            };
            while state.pending.is_none() && !state.shutdown {
                let Ok(next) = ready.wait(state) else {
                    return;
                };
                state = next;
            }
            if state.shutdown {
                return;
            }
            state.pending.take()
        };
        let Some(request) = request else {
            continue;
        };
        let candidates = prompt_candidates(&request.workspace, &request.query);
        if results
            .send(CompletionResponse {
                generation: request.generation,
                token_range: request.token_range,
                query: request.query,
                candidates,
            })
            .is_err()
        {
            return;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionAction {
    Applied,
    Submit,
}

/// The keys an open completion popup claims before the editor sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKey {
    Escape,
    Up,
    Down,
    Tab,
    Enter,
}

/// What the caller must do after routing a key through the popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKeyOutcome {
    /// The popup handled the key; the editor must not see it.
    Consumed,
    /// A slash command was accepted and the caller must submit the prompt.
    Submit,
    /// No popup was open, or it does not claim this key.
    Ignored,
}

#[derive(Debug, Clone, Copy)]
pub struct CompletionView<'a> {
    pub candidates: &'a [CompletionCandidate],
    pub selected: usize,
}

impl CompletionEngine {
    pub fn set_user_skills<'a>(&mut self, skills: impl IntoIterator<Item = (&'a str, &'a str)>) {
        self.user_skills = skills
            .into_iter()
            .map(|(name, _description)| {
                let command = format!("/{name}");
                CompletionCandidate {
                    id: format!("skill:{name}"),
                    kind: CompletionKind::Skill,
                    label: command.clone(),
                    insertion: command,
                }
            })
            .collect();
        self.cancel();
    }

    pub fn complete(
        &mut self,
        query: &str,
        candidates: impl IntoIterator<Item = CompletionCandidate>,
    ) -> CompletionResult {
        self.generation = self.generation.saturating_add(1);
        self.active = None;
        ranked_completion(self.generation, query, candidates)
    }

    pub fn complete_prompt(
        &mut self,
        editor: &mut PromptEditor,
        workspace: &Path,
    ) -> Result<bool, InputError> {
        let Some((token_range, query)) = active_token(editor) else {
            self.cancel();
            return Ok(false);
        };
        let candidates = self.prompt_candidates(workspace, &query)?;
        let result = self.complete(&query, candidates);
        if result.candidates.is_empty() {
            return Ok(false);
        }
        editor.select(token_range);
        self.apply(editor, &result, 0)?;
        Ok(true)
    }

    pub fn refresh(&mut self, editor: &PromptEditor, workspace: &Path) -> Result<(), InputError> {
        let Some((token_range, query)) = active_token(editor) else {
            self.cancel();
            return Ok(());
        };
        self.generation = self.generation.saturating_add(1);
        self.active = None;
        if query.starts_with('/') {
            let candidates = self.prompt_candidates(workspace, &query)?;
            self.install(self.generation, token_range, &query, candidates);
            return Ok(());
        }
        let worker = match self.worker.as_ref() {
            Some(worker) => worker,
            None => {
                self.worker = Some(CompletionWorker::spawn()?);
                self.worker.as_ref().ok_or_else(|| {
                    InputError::CompletionWorker("worker is unavailable".to_owned())
                })?
            }
        };
        worker.submit(CompletionRequest {
            generation: self.generation,
            token_range,
            query,
            workspace: workspace.to_path_buf(),
        })?;
        Ok(())
    }

    /// Dispatches lookup work requested by the deterministic input reducer.
    ///
    /// Slash candidates are in-memory and install immediately. Filesystem path
    /// lookup stays on the existing worker so the terminal event path never
    /// blocks on I/O.
    pub(crate) fn dispatch_request(
        &mut self,
        generation: u64,
        token_range: Range<usize>,
        query: String,
        workspace: &Path,
    ) -> Result<(), InputError> {
        if generation != self.generation {
            return Err(InputError::StaleCompletion);
        }
        if query.starts_with('/') {
            let candidates = self.prompt_candidates(workspace, &query)?;
            self.install(generation, token_range, &query, candidates);
            return Ok(());
        }
        let worker = match self.worker.as_ref() {
            Some(worker) => worker,
            None => {
                self.worker = Some(CompletionWorker::spawn()?);
                self.worker.as_ref().ok_or_else(|| {
                    InputError::CompletionWorker("worker is unavailable".to_owned())
                })?
            }
        };
        worker.submit(CompletionRequest {
            generation,
            token_range,
            query,
            workspace: workspace.to_path_buf(),
        })
    }

    fn prompt_candidates(
        &self,
        workspace: &Path,
        query: &str,
    ) -> Result<Vec<CompletionCandidate>, InputError> {
        let mut candidates = prompt_candidates(workspace, query)?;
        if query.starts_with('/') {
            candidates.extend(self.user_skills.iter().cloned());
        }
        Ok(candidates)
    }

    pub fn poll(&mut self) -> Result<bool, InputError> {
        let mut responses = Vec::new();
        if let Some(worker) = &self.worker {
            while let Ok(response) = worker.results.try_recv() {
                responses.push(response);
            }
        }
        let mut changed = false;
        for response in responses {
            if response.generation != self.generation {
                continue;
            }
            let candidates = response.candidates?;
            self.install(
                response.generation,
                response.token_range,
                &response.query,
                candidates,
            );
            changed = true;
        }
        Ok(changed)
    }

    /// Generation of the most recent completion request.
    ///
    /// Callers that resolve candidates outside the engine tag their request
    /// with it and hand it back, so a late answer can be recognised as stale.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Ranks candidates and presents them without touching the filesystem.
    ///
    /// Candidates arrive raw: ranking, the mention cap and the empty-result
    /// rule are part of the observable contract and stay in this layer
    /// whether the lookup ran on the worker thread or in a replay adapter.
    pub fn install(
        &mut self,
        generation: u64,
        token_range: Range<usize>,
        query: &str,
        candidates: Vec<CompletionCandidate>,
    ) {
        let mut result = ranked_completion(generation, query, candidates);
        if result
            .candidates
            .first()
            .is_some_and(|candidate| candidate.kind == CompletionKind::Mention)
        {
            result.candidates.truncate(MAX_VISIBLE_COMPLETIONS);
        }
        if !result.candidates.is_empty() {
            self.active = Some(ActiveCompletion {
                token_range,
                result,
                selected: 0,
            });
        }
    }

    #[must_use]
    pub fn view(&self) -> Option<CompletionView<'_>> {
        self.active.as_ref().map(|active| CompletionView {
            candidates: &active.result.candidates,
            selected: active.selected,
        })
    }

    pub fn move_selection(&mut self, delta: isize) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        let count = active.result.candidates.len();
        if count == 0 {
            return false;
        }
        let distance = delta.unsigned_abs() % count;
        active.selected = if delta.is_negative() {
            active
                .selected
                .saturating_add(count)
                .saturating_sub(distance)
                % count
        } else {
            active.selected.saturating_add(distance) % count
        };
        true
    }

    /// Routes a key through an open popup, mirroring the reference precedence.
    ///
    /// The interactive loop and the deterministic replay boundary both enter
    /// here, so selection, acceptance and dismissal cannot drift apart.
    pub fn handle_key(
        &mut self,
        key: CompletionKey,
        editor: &mut PromptEditor,
    ) -> Result<CompletionKeyOutcome, InputError> {
        if self.active.is_none() {
            return Ok(CompletionKeyOutcome::Ignored);
        }
        match key {
            CompletionKey::Escape => {
                self.cancel();
                Ok(CompletionKeyOutcome::Consumed)
            }
            CompletionKey::Up => {
                self.move_selection(-1);
                Ok(CompletionKeyOutcome::Consumed)
            }
            CompletionKey::Down => {
                self.move_selection(1);
                Ok(CompletionKeyOutcome::Consumed)
            }
            CompletionKey::Tab => {
                self.accept(editor)?;
                Ok(CompletionKeyOutcome::Consumed)
            }
            CompletionKey::Enter => match self.accept(editor)? {
                CompletionAction::Applied => Ok(CompletionKeyOutcome::Consumed),
                CompletionAction::Submit => Ok(CompletionKeyOutcome::Submit),
            },
        }
    }

    pub fn accept(&mut self, editor: &mut PromptEditor) -> Result<CompletionAction, InputError> {
        let active = self.active.take().ok_or(InputError::MissingCompletion)?;
        if active.result.generation != self.generation {
            return Err(InputError::StaleCompletion);
        }
        let candidate = active
            .result
            .candidates
            .get(active.selected)
            .ok_or(InputError::MissingCompletion)?
            .clone();
        editor.select(active.token_range);
        editor.insert(&candidate.insertion);
        if candidate.kind == CompletionKind::Mention && !candidate.insertion.ends_with('/') {
            editor.insert(" ");
        }
        self.generation = self.generation.saturating_add(1);
        Ok(if candidate.kind == CompletionKind::SlashCommand {
            CompletionAction::Submit
        } else {
            CompletionAction::Applied
        })
    }

    pub fn apply(
        &self,
        editor: &mut PromptEditor,
        result: &CompletionResult,
        selected: usize,
    ) -> Result<(), InputError> {
        if result.generation != self.generation {
            return Err(InputError::StaleCompletion);
        }
        let candidate = result
            .candidates
            .get(selected)
            .ok_or(InputError::MissingCompletion)?;
        editor.insert(&candidate.insertion);
        Ok(())
    }

    pub fn cancel(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.active = None;
        if let Some(worker) = &self.worker {
            worker.cancel_pending();
        }
    }

    #[cfg(test)]
    fn wait_for_pending(&mut self) -> Result<(), InputError> {
        let response = self
            .worker
            .as_ref()
            .ok_or_else(|| InputError::CompletionWorker("worker is unavailable".to_owned()))?
            .results
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|error| InputError::CompletionWorker(error.to_string()))?;
        if response.generation == self.generation {
            let candidates = response.candidates?;
            self.install(
                response.generation,
                response.token_range,
                &response.query,
                candidates,
            );
        }
        Ok(())
    }
}

fn ranked_completion(
    generation: u64,
    query: &str,
    candidates: impl IntoIterator<Item = CompletionCandidate>,
) -> CompletionResult {
    let normalized = query.to_lowercase();
    let mut candidates = candidates
        .into_iter()
        .filter(|candidate| fuzzy_completion_score(&candidate.label, &normalized).is_some())
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        completion_rank(left, &normalized)
            .cmp(&completion_rank(right, &normalized))
            .then_with(|| {
                fuzzy_completion_score(&right.label, &normalized)
                    .cmp(&fuzzy_completion_score(&left.label, &normalized))
            })
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.label.to_lowercase().cmp(&right.label.to_lowercase()))
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates.truncate(MAX_COMPLETION_CANDIDATES);
    CompletionResult {
        generation,
        candidates,
    }
}

fn completion_rank(candidate: &CompletionCandidate, query: &str) -> u8 {
    let label = candidate.label.to_lowercase();
    match label.as_str().cmp(query) {
        Ordering::Equal => 0,
        _ if label.starts_with(query) => 1,
        _ => 2,
    }
}

fn fuzzy_completion_score(candidate: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let candidate = candidate.to_lowercase();
    let mut candidate_chars = candidate.chars().enumerate();
    let mut score = 0usize;
    let mut previous = None;
    for query_character in query.chars() {
        let (index, _) = candidate_chars
            .find(|(_, candidate_character)| *candidate_character == query_character)?;
        score = score
            .saturating_add(previous.map_or(1, |previous| index.saturating_sub(previous).max(1)));
        previous = Some(index);
    }
    Some(usize::MAX.saturating_sub(score))
}

pub(crate) fn active_token(editor: &PromptEditor) -> Option<(Range<usize>, String)> {
    let text = editor.text();
    let cursor_byte = grapheme_byte(text, editor.cursor());
    if text.starts_with('/') {
        let end_byte = text.find(char::is_whitespace).unwrap_or(text.len());
        let query_end = cursor_byte.min(end_byte);
        return Some((
            0..grapheme_count(&text[..query_end]),
            text[..query_end].to_owned(),
        ));
    }
    let start_byte = text[..cursor_byte]
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(byte, character)| byte + character.len_utf8());
    let query = text[start_byte..cursor_byte].to_owned();
    if !query.starts_with('@') || query.len() == 1 && cursor_byte != text.len() {
        return None;
    }
    Some((
        grapheme_count(&text[..start_byte])..grapheme_count(&text[..cursor_byte]),
        query,
    ))
}

/// Filesystem-backed candidate lookup used by the completion adapter.
pub fn prompt_candidates(
    workspace: &Path,
    query: &str,
) -> Result<Vec<CompletionCandidate>, InputError> {
    if query.starts_with('/') {
        return Ok(command_aliases()
            .map(|command| CompletionCandidate {
                id: format!("command:{command}"),
                kind: CompletionKind::SlashCommand,
                label: command.to_owned(),
                insertion: command.to_owned(),
            })
            .collect());
    }

    let Some(raw_query) = query.strip_prefix('@') else {
        return Ok(Vec::new());
    };
    let canonical_workspace =
        fs::canonicalize(workspace).map_err(|error| InputError::Workspace(error.to_string()))?;
    if raw_query.is_empty() || raw_query.ends_with('/') || raw_query.contains('/') {
        return directory_candidates(&canonical_workspace, raw_query);
    }
    recursive_candidates(&canonical_workspace, raw_query)
}

fn directory_candidates(
    canonical_workspace: &Path,
    raw_query: &str,
) -> Result<Vec<CompletionCandidate>, InputError> {
    let (parent_display, leaf) = raw_query
        .rsplit_once('/')
        .map_or(("", raw_query), |(parent, leaf)| {
            (&raw_query[..parent.len().saturating_add(1)], leaf)
        });
    let parent = parent_display.trim_end_matches('/');
    let Ok(relative_parent) = safe_relative_path(parent) else {
        return Ok(Vec::new());
    };
    let directory = canonical_workspace.join(relative_parent);
    let Ok(canonical_directory) = fs::canonicalize(directory) else {
        return Ok(Vec::new());
    };
    if !canonical_directory.starts_with(canonical_workspace) || !canonical_directory.is_dir() {
        return Ok(Vec::new());
    }

    let entries = fs::read_dir(canonical_directory)
        .map_err(|error| InputError::Resource(error.to_string()))?;
    let mut entries = entries
        .take(MAX_COMPLETION_SCAN_ENTRIES.saturating_add(1))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| InputError::Resource(error.to_string()))?;
    entries.truncate(MAX_COMPLETION_SCAN_ENTRIES);
    entries.sort_by_key(|entry| entry.file_name());
    let mut candidates = Vec::new();
    for entry in entries {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') && !leaf.starts_with('.') {
            continue;
        }
        if fuzzy_completion_score(&name, leaf).is_none() {
            continue;
        }
        let Some((is_directory, _)) = safe_entry_kind(&entry, canonical_workspace) else {
            continue;
        };
        candidates.push(mention_candidate(
            format!("{parent_display}{name}"),
            is_directory,
        ));
    }
    Ok(candidates)
}

fn recursive_candidates(
    canonical_workspace: &Path,
    raw_query: &str,
) -> Result<Vec<CompletionCandidate>, InputError> {
    let mut directories = VecDeque::from([canonical_workspace.to_path_buf()]);
    let mut candidates = Vec::new();
    let mut scanned = 0usize;
    while let Some(directory) = directories.pop_front() {
        let entries =
            fs::read_dir(directory).map_err(|error| InputError::Resource(error.to_string()))?;
        for entry in entries {
            if scanned >= MAX_COMPLETION_SCAN_ENTRIES {
                return Ok(candidates);
            }
            scanned = scanned.saturating_add(1);
            let entry = entry.map_err(|error| InputError::Resource(error.to_string()))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') && !raw_query.starts_with('.') {
                continue;
            }
            let Some((is_directory, should_recurse)) = safe_entry_kind(&entry, canonical_workspace)
            else {
                continue;
            };
            if should_recurse {
                directories.push_back(entry.path());
            }
            let entry_path = entry.path();
            let Ok(relative) = entry_path.strip_prefix(canonical_workspace) else {
                continue;
            };
            let display = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if fuzzy_completion_score(&display, raw_query).is_some() {
                candidates.push(mention_candidate(display, is_directory));
            }
        }
    }
    Ok(candidates)
}

fn safe_entry_kind(entry: &fs::DirEntry, workspace: &Path) -> Option<(bool, bool)> {
    let file_type = entry.file_type().ok()?;
    if !file_type.is_symlink() {
        return Some((file_type.is_dir(), file_type.is_dir()));
    }
    let canonical = fs::canonicalize(entry.path()).ok()?;
    canonical
        .starts_with(workspace)
        .then(|| (canonical.is_dir(), false))
}

fn mention_candidate(path: String, is_directory: bool) -> CompletionCandidate {
    let suffix = if is_directory { "/" } else { "" };
    let insertion = format!("@{path}{suffix}");
    CompletionCandidate {
        id: format!("mention:{insertion}"),
        kind: CompletionKind::Mention,
        label: insertion.clone(),
        insertion,
    }
}

#[must_use]
pub fn completion_description(label: &str) -> &'static str {
    command_description(label)
}

#[must_use]
pub fn normalize_pasted_text(workspace: &Path, pasted: &str) -> String {
    let trimmed = pasted.trim();
    if trimmed.is_empty()
        || trimmed.contains('\n')
        || trimmed.contains('\r')
        || trimmed.starts_with('@')
        || (trimmed.contains('\'') && !has_matched_quotes(trimmed, '\''))
        || (trimmed.contains('"') && !has_matched_quotes(trimmed, '"'))
    {
        return pasted.to_owned();
    }
    let unquoted = if has_matched_quotes(trimmed, '\'') || has_matched_quotes(trimmed, '"') {
        &trimmed[1..trimmed.len().saturating_sub(1)]
    } else {
        trimmed
    };
    let candidate = unquoted.replace("\\ ", " ");
    let path = expand_tilde_path(&candidate);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !path.is_absolute() || !IMAGE_EXTENSIONS.contains(&extension.as_str()) || !path.is_file() {
        return pasted.to_owned();
    }
    let Ok(workspace) = fs::canonicalize(workspace) else {
        return pasted.to_owned();
    };
    let Ok(path) = fs::canonicalize(path) else {
        return pasted.to_owned();
    };
    let Ok(relative) = path.strip_prefix(workspace) else {
        return pasted.to_owned();
    };
    let display = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if display.contains('\'') {
        return pasted.to_owned();
    }
    if display.chars().any(char::is_whitespace) {
        format!("@'{display}'")
    } else {
        format!("@{display}")
    }
}

fn has_matched_quotes(value: &str, quote: char) -> bool {
    value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote)
}

fn expand_tilde_path(value: &str) -> PathBuf {
    let path = Path::new(value);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(first)) if first == "~") {
        return path.to_path_buf();
    }
    let Some(mut home) = user_home_directory() else {
        return path.to_path_buf();
    };
    home.extend(components);
    home
}

fn user_home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .or_else(|| {
                let mut home = PathBuf::from(std::env::var_os("HOMEDRIVE")?);
                home.push(std::env::var_os("HOMEPATH")?);
                Some(home)
            })
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionMetric {
    pub token: String,
    pub kind: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedSubmission {
    pub turn: TurnRequest,
    pub metrics: Vec<MentionMetric>,
    pub cleanup_paths: Vec<PathBuf>,
}

pub fn prepare_submission(
    workspace: &Path,
    prompt: &str,
) -> Result<PreparedSubmission, InputError> {
    let canonical_workspace =
        fs::canonicalize(workspace).map_err(|error| InputError::Workspace(error.to_string()))?;
    let mut input = vec![PublicContentBlock::Text {
        text: prompt.to_owned(),
    }];
    let mut metrics = Vec::new();
    let mut cleanup_paths = Vec::new();
    for (token, raw) in mention_tokens(prompt) {
        if raw.is_empty() {
            continue;
        }
        let relative = safe_relative_path(&raw)?;
        let path = canonical_workspace.join(relative);
        let canonical =
            fs::canonicalize(&path).map_err(|_| InputError::MissingMention(raw.clone()))?;
        if !canonical.starts_with(&canonical_workspace) {
            return Err(InputError::MentionOutsideWorkspace(raw));
        }
        let metadata =
            fs::metadata(&canonical).map_err(|error| InputError::Resource(error.to_string()))?;
        if !metadata.is_file() {
            return Err(InputError::InvalidMention(raw));
        }
        if metadata.len() > MAX_RESOURCE_BYTES {
            return Err(InputError::ResourceTooLarge {
                path: raw,
                bytes: metadata.len(),
                limit: MAX_RESOURCE_BYTES,
            });
        }
        let bytes =
            fs::read(&canonical).map_err(|error| InputError::Resource(error.to_string()))?;
        let extension = canonical
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let uri = format!("file://{}", canonical.to_string_lossy());
        if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
            let media_type = match extension.as_str() {
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                _ => "image/png",
            };
            input.push(PublicContentBlock::Image {
                attachment: json!({
                    "uri": uri,
                    "name": &raw,
                    "bytes": metadata.len(),
                    "mediaType": media_type,
                    "data": BASE64_STANDARD.encode(&bytes),
                }),
            });
            if is_ephemeral_clipboard_image(&canonical_workspace, &canonical) {
                cleanup_paths.push(canonical.clone());
            }
            metrics.push(MentionMetric {
                token: token.to_owned(),
                kind: "image".to_owned(),
                bytes: metadata.len(),
            });
        } else {
            if bytes.contains(&0) {
                return Err(InputError::BinaryMention(raw.to_owned()));
            }
            let text = String::from_utf8(bytes)
                .map_err(|_| InputError::InvalidUnicodeMention(raw.to_owned()))?;
            input.push(PublicContentBlock::Resource {
                resource: json!({
                    "uri": uri,
                    "name": &raw,
                    "text": text,
                }),
            });
            metrics.push(MentionMetric {
                token: token.to_owned(),
                kind: "file".to_owned(),
                bytes: metadata.len(),
            });
        }
    }
    let turn = TurnRequest {
        prompt: prompt.to_owned(),
        input,
        client_user_message_id: None,
        auto_title: None,
        user_display_content: Some(json!({"type": "text", "text": prompt})),
        mention_stats: Some(serde_json::to_value(&metrics).map_err(InputError::Json)?),
    };
    Ok(PreparedSubmission {
        turn,
        metrics,
        cleanup_paths,
    })
}

fn is_ephemeral_clipboard_image(workspace: &Path, path: &Path) -> bool {
    path.parent() == Some(workspace)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".vibe-clipboard-") && name.ends_with(".png"))
}

fn mention_tokens(prompt: &str) -> Vec<(String, String)> {
    let mut tokens = Vec::new();
    let mut cursor = 0usize;
    while cursor < prompt.len() {
        let Some(relative_start) = prompt[cursor..].find('@') else {
            break;
        };
        let start = cursor.saturating_add(relative_start);
        let boundary = prompt[..start].chars().next_back();
        if boundary
            .is_some_and(|character| !character.is_whitespace() && !"(<[".contains(character))
        {
            cursor = start.saturating_add(1);
            continue;
        }
        let value_start = start.saturating_add(1);
        let Some(first) = prompt[value_start..].chars().next() else {
            break;
        };
        let (raw, end) = if matches!(first, '\'' | '"') {
            let content_start = value_start.saturating_add(first.len_utf8());
            let Some(relative_end) = prompt[content_start..].find(first) else {
                break;
            };
            let content_end = content_start.saturating_add(relative_end);
            (
                prompt[content_start..content_end].to_owned(),
                content_end.saturating_add(first.len_utf8()),
            )
        } else {
            let relative_end = prompt[value_start..]
                .find(char::is_whitespace)
                .unwrap_or(prompt.len().saturating_sub(value_start));
            let end = value_start.saturating_add(relative_end);
            (
                prompt[value_start..end]
                    .trim_matches(|character: char| ",.;:!?)]}".contains(character))
                    .to_owned(),
                end,
            )
        };
        tokens.push((prompt[start..end].to_owned(), raw));
        cursor = end;
    }
    tokens
}

fn safe_relative_path(raw: &str) -> Result<PathBuf, InputError> {
    let path = Path::new(raw);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(InputError::MentionOutsideWorkspace(raw.to_owned()));
    }
    Ok(path.to_path_buf())
}

pub trait ClipboardPort {
    fn read_text(&mut self) -> Result<String, String>;
    fn write_text(&mut self, value: &str) -> Result<(), String>;
}

pub trait ExternalEditorPort {
    fn edit(&mut self, initial: &str) -> Result<String, String>;
}

pub struct SystemExternalEditor {
    command: String,
}

impl SystemExternalEditor {
    #[must_use]
    pub fn from_environment() -> Self {
        Self::from_sources(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok())
    }

    fn from_sources(visual: Option<String>, editor: Option<String>) -> Self {
        Self {
            command: visual
                .filter(|value| !value.trim().is_empty())
                .or_else(|| editor.filter(|value| !value.trim().is_empty()))
                .unwrap_or_else(|| "nano".to_owned()),
        }
    }

    fn command_parts(&self) -> Result<Vec<String>, String> {
        let parts = shlex::split(&self.command)
            .filter(|parts| !parts.is_empty())
            .ok_or_else(|| "External editor command is invalid".to_owned())?;
        Ok(parts)
    }
}

impl ExternalEditorPort for SystemExternalEditor {
    fn edit(&mut self, initial: &str) -> Result<String, String> {
        let command = self.command_parts()?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mistral-vibe-rs-editor-{}-{stamp}.md",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|error| error.to_string())?;
        std::io::Write::write_all(&mut file, initial.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        let result = Command::new(&command[0])
            .args(&command[1..])
            .arg(&path)
            .status();
        let edited = match result {
            Ok(status) if status.success() => fs::read_to_string(&path)
                .map(|content| content.trim_end().to_owned())
                .map_err(|error| error.to_string()),
            Ok(status) => Err(format!("External editor exited with {status}")),
            Err(error) => Err(format!("External editor could not start: {error}")),
        };
        let _ = fs::remove_file(&path);
        edited
    }
}

#[derive(Debug, Error)]
pub enum InputError {
    #[error("paste contains {bytes} bytes; limit is {limit}")]
    PasteTooLarge { bytes: usize, limit: usize },
    #[error("completion result is stale")]
    StaleCompletion,
    #[error("selected completion does not exist")]
    MissingCompletion,
    #[error("completion worker failed: {0}")]
    CompletionWorker(String),
    #[error("workspace is unavailable: {0}")]
    Workspace(String),
    #[error("mentioned path `{0}` does not exist")]
    MissingMention(String),
    #[error("mentioned path `{0}` is outside the workspace")]
    MentionOutsideWorkspace(String),
    #[error("mentioned path `{0}` is not a regular file")]
    InvalidMention(String),
    #[error("mentioned file `{path}` contains {bytes} bytes; limit is {limit}")]
    ResourceTooLarge {
        path: String,
        bytes: u64,
        limit: u64,
    },
    #[error("mentioned file `{0}` is binary")]
    BinaryMention(String),
    #[error("mentioned file `{0}` is not valid UTF-8")]
    InvalidUnicodeMention(String),
    #[error("resource could not be read: {0}")]
    Resource(String),
    #[error("mention metrics could not be encoded: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_uses_unicode_graphemes_and_preserves_multiline_history() {
        let mut editor = PromptEditor::default();
        editor.insert("a");
        editor.insert("e\u{301}");
        editor.insert("\nβ");
        assert_eq!(editor.cursor(), 4);
        editor.move_left(false);
        editor.move_left(false);
        editor.delete_backward();
        assert_eq!(editor.text(), "a\nβ");
        let submitted = editor.submit().expect("non-empty prompt");
        assert_eq!(submitted, "a\nβ");
        editor.set_text("draft");
        editor.history_previous();
        assert_eq!(editor.text(), "a\nβ");
        editor.history_next();
        assert_eq!(editor.text(), "draft");
    }

    #[test]
    fn external_editor_prefers_visual_parses_arguments_and_defaults_to_nano() {
        let configured = SystemExternalEditor::from_sources(
            Some("code --wait".to_owned()),
            Some("nvim".to_owned()),
        );
        assert_eq!(
            configured.command_parts(),
            Ok(vec!["code".to_owned(), "--wait".to_owned()])
        );

        let fallback = SystemExternalEditor::from_sources(None, None);
        assert_eq!(fallback.command_parts(), Ok(vec!["nano".to_owned()]));

        let invalid = SystemExternalEditor::from_sources(Some("'".to_owned()), None);
        assert_eq!(
            invalid.command_parts(),
            Err("External editor command is invalid".to_owned())
        );
    }

    #[test]
    fn huge_paste_is_rejected_without_losing_existing_input() {
        let mut editor = PromptEditor::default();
        editor.insert("keep");
        let paste = "x".repeat(MAX_PASTE_BYTES + 1);
        assert!(matches!(
            editor.paste(&paste),
            Err(InputError::PasteTooLarge { .. })
        ));
        assert_eq!(editor.text(), "keep");
    }

    #[test]
    fn secret_submission_never_enters_prompt_history() {
        let mut editor = PromptEditor::default();
        editor.set_text("secret-api-key");
        assert_eq!(editor.take_unrecorded().as_deref(), Some("secret-api-key"));
        editor.set_text("ordinary prompt");
        assert_eq!(editor.submit().as_deref(), Some("ordinary prompt"));
        assert_eq!(editor.history, vec!["ordinary prompt"]);
    }

    #[test]
    fn completions_are_stable_and_racing_results_are_rejected() {
        let mut engine = CompletionEngine::default();
        let candidates = vec![
            CompletionCandidate {
                id: "skill".to_owned(),
                kind: CompletionKind::Skill,
                label: "review".to_owned(),
                insertion: "/review".to_owned(),
            },
            CompletionCandidate {
                id: "command".to_owned(),
                kind: CompletionKind::SlashCommand,
                label: "reload".to_owned(),
                insertion: "/reload".to_owned(),
            },
        ];
        let stale = engine.complete("re", candidates.clone());
        let current = engine.complete("rev", candidates);
        let mut editor = PromptEditor::default();
        assert!(matches!(
            engine.apply(&mut editor, &stale, 0),
            Err(InputError::StaleCompletion)
        ));
        engine
            .apply(&mut editor, &current, 0)
            .expect("current completion applies");
        assert_eq!(editor.text(), "/review");
    }

    #[test]
    fn runtime_completion_applies_slash_and_at_mention_candidates() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir(temporary.path().join("src")).expect("path fixture");
        let mut engine = CompletionEngine::default();
        let mut editor = PromptEditor::default();

        editor.set_text("/clo");
        assert!(
            engine
                .complete_prompt(&mut editor, temporary.path())
                .expect("slash completion")
        );
        assert_eq!(editor.text(), "/close");

        editor.set_text("inspect sr");
        assert!(
            !engine
                .complete_prompt(&mut editor, temporary.path())
                .expect("bare paths do not activate completion")
        );
        assert_eq!(editor.text(), "inspect sr");

        editor.set_text("inspect @sr");
        assert!(
            engine
                .complete_prompt(&mut editor, temporary.path())
                .expect("mention completion")
        );
        assert_eq!(editor.text(), "inspect @src/");
    }

    #[test]
    fn live_completion_tracks_selection_and_applies_official_spacing() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(temporary.path().join("notes.txt"), "context").expect("mention fixture");
        let mut engine = CompletionEngine::default();
        let mut editor = PromptEditor::default();

        editor.set_text("/cl");
        engine
            .refresh(&editor, temporary.path())
            .expect("slash suggestions");
        let view = engine.view().expect("visible slash suggestions");
        assert_eq!(view.candidates[view.selected].label, "/clear");
        let suggestion_count = view.candidates.len();
        assert!(engine.move_selection(-1));
        assert_eq!(
            engine.view().expect("wrapped selection").selected,
            suggestion_count - 1
        );
        assert!(engine.move_selection(1));
        assert_eq!(engine.view().expect("rewrapped selection").selected, 0);
        assert_eq!(
            engine.accept(&mut editor).expect("slash completion"),
            CompletionAction::Submit
        );
        assert_eq!(editor.text(), "/clear");
        assert!(engine.view().is_none());

        editor.set_text("inspect @no");
        engine
            .refresh(&editor, temporary.path())
            .expect("mention suggestions");
        engine.wait_for_pending().expect("completed mention search");
        assert_eq!(
            engine.accept(&mut editor).expect("mention completion"),
            CompletionAction::Applied
        );
        assert_eq!(editor.text(), "inspect @notes.txt ");

        fs::create_dir(temporary.path().join("src")).expect("nested directory");
        fs::write(temporary.path().join("src/lib.rs"), "pub fn nested() {}")
            .expect("nested mention fixture");
        editor.set_text("inspect @lib");
        engine
            .refresh(&editor, temporary.path())
            .expect("recursive mention suggestions");
        engine
            .wait_for_pending()
            .expect("completed recursive search");
        assert_eq!(
            engine
                .view()
                .expect("recursive suggestion")
                .candidates
                .first()
                .map(|candidate| candidate.label.as_str()),
            Some("@src/lib.rs")
        );

        editor.set_text("inspect @noXYZ");
        editor.move_left(false);
        editor.move_left(false);
        editor.move_left(false);
        engine
            .refresh(&editor, temporary.path())
            .expect("partial mention suggestions");
        engine.wait_for_pending().expect("completed partial search");
        engine.accept(&mut editor).expect("partial completion");
        assert_eq!(editor.text(), "inspect @notes.txt XYZ");
    }

    #[test]
    fn completion_and_selection_state_stay_bounded_and_stable() {
        let mut engine = CompletionEngine::default();
        let result = engine.complete(
            "",
            (0..MAX_COMPLETION_CANDIDATES.saturating_mul(2)).map(|index| CompletionCandidate {
                id: format!("candidate-{index}"),
                kind: CompletionKind::Path,
                label: format!("candidate-{index}"),
                insertion: format!("candidate-{index}"),
            }),
        );
        assert_eq!(result.candidates.len(), MAX_COMPLETION_CANDIDATES);
        engine.cancel();
        assert!(matches!(
            engine.apply(&mut PromptEditor::default(), &result, 0),
            Err(InputError::StaleCompletion)
        ));

        let mut editor = PromptEditor::default();
        editor.set_text("abcd");
        editor.move_left(false);
        editor.move_left(true);
        assert_eq!(editor.selection(), Some(2..3));
        editor.move_right(true);
        assert_eq!(editor.selection(), None);
        editor.move_right(true);
        assert_eq!(editor.selection(), Some(3..4));
    }

    #[test]
    fn mentions_keep_display_text_and_model_resources_separate() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(temporary.path().join("notes.txt"), "safe context").expect("text fixture");
        fs::write(temporary.path().join("image.png"), b"image").expect("image fixture");
        let prepared = prepare_submission(temporary.path(), "inspect @notes.txt and @image.png")
            .expect("mentions prepare");
        assert_eq!(prepared.turn.prompt, "inspect @notes.txt and @image.png");
        assert_eq!(prepared.turn.input.len(), 3);
        assert_eq!(prepared.metrics.len(), 2);
        let PublicContentBlock::Image { attachment } = &prepared.turn.input[2] else {
            return;
        };
        assert_eq!(attachment["mediaType"], "image/png");
        assert_eq!(
            BASE64_STANDARD
                .decode(attachment["data"].as_str().unwrap_or_default())
                .expect("canonical image data"),
            b"image"
        );
        assert_eq!(
            prepared
                .turn
                .user_display_content
                .as_ref()
                .and_then(|value| value.get("text"))
                .and_then(serde_json::Value::as_str),
            Some("inspect @notes.txt and @image.png")
        );
    }

    #[test]
    fn pasted_workspace_images_become_quoted_mentions() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let image = temporary.path().join("image one.png");
        fs::write(&image, b"image").expect("image fixture");

        let pasted =
            normalize_pasted_text(temporary.path(), &format!("'{}'", image.to_string_lossy()));
        assert_eq!(pasted, "@'image one.png'");

        let prepared = prepare_submission(temporary.path(), &format!("inspect {pasted}"))
            .expect("quoted image mention prepares");
        assert_eq!(prepared.turn.input.len(), 2);
        assert_eq!(prepared.metrics[0].kind, "image");

        let text = temporary.path().join("notes one.txt");
        fs::write(&text, "context").expect("text fixture");
        assert_eq!(
            normalize_pasted_text(temporary.path(), &text.to_string_lossy()),
            text.to_string_lossy()
        );
    }

    #[test]
    fn clipboard_images_are_marked_for_cleanup_after_their_bytes_are_embedded() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        let image = temporary.path().join(".vibe-clipboard-1.png");
        fs::write(&image, b"image").expect("clipboard image");

        let prepared = prepare_submission(temporary.path(), "inspect @'.vibe-clipboard-1.png'")
            .expect("clipboard image prepares");

        assert_eq!(prepared.cleanup_paths, [image]);
        assert!(matches!(
            prepared.turn.input.get(1),
            Some(PublicContentBlock::Image { .. })
        ));
    }

    #[test]
    fn binary_and_external_mentions_fail_without_partial_submission() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(temporary.path().join("binary"), b"a\0b").expect("binary fixture");
        assert!(matches!(
            prepare_submission(temporary.path(), "@binary"),
            Err(InputError::BinaryMention(_))
        ));
        assert!(matches!(
            prepare_submission(temporary.path(), "@../outside"),
            Err(InputError::MentionOutsideWorkspace(_))
        ));
    }
}

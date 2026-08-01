use std::fs;
use std::fs::OpenOptions;
use std::ops::Range;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) const COMPOSER_HORIZONTAL_OVERHEAD: u16 = 9;

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

    pub fn paste(&mut self, text: &str) {
        self.insert(text);
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

pub(super) fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

pub(super) fn grapheme_byte(text: &str, index: usize) -> usize {
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
    #[error("completion result is stale")]
    StaleCompletion,
    #[error("selected completion does not exist")]
    MissingCompletion,
    #[error("completion worker failed: {0}")]
    CompletionWorker(String),
    #[error("workspace is unavailable: {0}")]
    Workspace(String),
}

#[cfg(test)]
#[path = "input/tests.rs"]
mod tests;

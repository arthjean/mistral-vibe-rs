use std::cell::RefCell;
use std::fs;
use std::fs::OpenOptions;
use std::ops::Range;
use std::process::Command;
use std::sync::Arc;
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

#[derive(Debug, Clone, Default)]
pub struct PromptEditor {
    text: String,
    grapheme_len: usize,
    syntax: SyntaxCounts,
    cursor: usize,
    selection: Option<Range<usize>>,
    selection_anchor: Option<usize>,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
    history_loaded: bool,
    cursor_moved_since_history_load: bool,
    discard_joined_character: bool,
    revision: u64,
    visual_layouts: RefCell<Vec<CachedVisualLayout>>,
}

impl PartialEq for PromptEditor {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.grapheme_len == other.grapheme_len
            && self.syntax == other.syntax
            && self.cursor == other.cursor
            && self.selection == other.selection
            && self.selection_anchor == other.selection_anchor
            && self.history == other.history
            && self.history_index == other.history_index
            && self.draft == other.draft
            && self.history_loaded == other.history_loaded
            && self.cursor_moved_since_history_load == other.cursor_moved_since_history_load
            && self.discard_joined_character == other.discard_joined_character
    }
}

impl Eq for PromptEditor {}

#[derive(Debug, Clone)]
struct CachedVisualLayout {
    width: usize,
    prefix: usize,
    layout: Arc<VisualLayout>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SyntaxCounts {
    path: usize,
    mention: usize,
}

impl SyntaxCounts {
    fn in_text(text: &str) -> Self {
        text.bytes().fold(Self::default(), |mut counts, byte| {
            if matches!(byte, b'/' | b'~' | b'\'' | b'"') {
                counts.path = counts.path.saturating_add(1);
            }
            if byte == b'@' {
                counts.mention = counts.mention.saturating_add(1);
            }
            counts
        })
    }

    fn add(&mut self, text: &str) {
        let added = Self::in_text(text);
        self.path = self.path.saturating_add(added.path);
        self.mention = self.mention.saturating_add(added.mention);
    }

    fn remove(&mut self, text: &str) {
        let removed = Self::in_text(text);
        self.path = self.path.saturating_sub(removed.path);
        self.mention = self.mention.saturating_sub(removed.mention);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct VisualLayout {
    graphemes: Vec<Range<usize>>,
    lines: Vec<Range<usize>>,
}

impl VisualLayout {
    #[must_use]
    pub(crate) fn lines(&self) -> &[Range<usize>] {
        &self.lines
    }

    #[must_use]
    pub(crate) fn grapheme<'a>(&self, text: &'a str, index: usize) -> &'a str {
        self.graphemes
            .get(index)
            .and_then(|range| text.get(range.clone()))
            .unwrap_or_default()
    }

    #[must_use]
    pub(crate) fn cell_width(&self, text: &str, range: Range<usize>) -> usize {
        range.fold(0, |column, index| {
            column.saturating_add(grapheme_cell_width(self.grapheme(text, index), column))
        })
    }

    #[must_use]
    pub(crate) fn cursor_cell(&self, text: &str, cursor: usize) -> (usize, usize) {
        let row = self
            .lines
            .iter()
            .rposition(|line| line.start <= cursor && cursor <= line.end)
            .unwrap_or_default();
        let column = self
            .lines
            .get(row)
            .map_or(0, |line| self.cell_width(text, line.start..cursor));
        (row, column)
    }

    #[must_use]
    pub(crate) fn cursor_at_cell(&self, text: &str, line: Range<usize>, column: usize) -> usize {
        let mut cursor = line.start;
        let mut cells = 0usize;
        while cursor < line.end {
            let width = grapheme_cell_width(self.grapheme(text, cursor), cells);
            if cells.saturating_add(width) > column {
                break;
            }
            cells = cells.saturating_add(width);
            cursor += 1;
        }
        cursor
    }

    #[must_use]
    pub(crate) fn text_lines(&self, text: &str) -> Vec<String> {
        self.lines
            .iter()
            .map(|line| {
                line.clone()
                    .map(|index| self.grapheme(text, index))
                    .collect()
            })
            .collect()
    }

    fn refresh_appended(&mut self, text: &str, width: usize, prefix: usize) {
        let rebuild_index = self.graphemes.len().saturating_sub(1);
        let rebuild_byte = self
            .graphemes
            .get(rebuild_index)
            .map_or(0, |grapheme| grapheme.start);
        let line_index = self
            .lines
            .iter()
            .rposition(|line| line.start <= rebuild_index)
            .unwrap_or_default();
        let line_start = self
            .lines
            .get(line_index)
            .map_or(prefix.min(rebuild_index), |line| line.start);
        self.lines.truncate(line_index);
        self.graphemes.truncate(rebuild_index);
        self.graphemes
            .extend(
                text[rebuild_byte..]
                    .grapheme_indices(true)
                    .map(|(byte, grapheme)| {
                        let start = rebuild_byte.saturating_add(byte);
                        start..start.saturating_add(grapheme.len())
                    }),
            );
        self.lines.extend(build_visual_lines(
            text,
            &self.graphemes,
            width.max(1),
            line_start,
        ));
    }
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
    pub(crate) const fn cursor_at_end(&self) -> bool {
        self.cursor == self.grapheme_len
    }

    #[must_use]
    pub(crate) fn cursor_byte(&self) -> usize {
        if self.cursor_at_end() {
            self.text.len()
        } else {
            grapheme_byte(&self.text, self.cursor)
        }
    }

    #[must_use]
    pub(crate) const fn has_path_syntax(&self) -> bool {
        self.syntax.path != 0
    }

    #[must_use]
    pub(crate) const fn has_mention_syntax(&self) -> bool {
        self.syntax.mention != 0
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
        let text = text.into();
        if self.text != text {
            self.text = text;
            self.grapheme_len = grapheme_count(&self.text);
            self.syntax = SyntaxCounts::in_text(&self.text);
            self.mark_text_changed();
        }
        self.cursor = self.grapheme_len;
        self.selection = None;
        self.selection_anchor = None;
        self.reset_history_navigation();
        self.discard_joined_character = false;
    }

    pub fn select(&mut self, range: Range<usize>) {
        let end = self.grapheme_len;
        let start = range.start.min(end);
        let target = range.end.min(end);
        self.selection_anchor = Some(start);
        self.selection = Some(start.min(target)..start.max(target));
        self.mark_history_cursor_moved(target);
        self.cursor = target;
    }

    pub fn insert(&mut self, text: &str) {
        let appending = self.selection.is_none() && self.cursor == self.grapheme_len;
        let append_rebuild_byte = if appending {
            self.text
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(byte, _)| byte)
        } else {
            0
        };
        let had_append_tail = appending && !self.text.is_empty();
        let deleted = self.delete_selection();
        let byte = if self.cursor == self.grapheme_len {
            self.text.len()
        } else {
            grapheme_byte(&self.text, self.cursor)
        };
        if !text.is_empty() {
            self.text.insert_str(byte, text);
            self.syntax.add(text);
        }
        if deleted || !text.is_empty() {
            if appending && !deleted {
                self.grapheme_len = self
                    .grapheme_len
                    .saturating_sub(usize::from(had_append_tail))
                    .saturating_add(grapheme_count(&self.text[append_rebuild_byte..]));
                self.cursor = self.grapheme_len;
                self.finish_append();
            } else {
                let cursor_byte = byte.saturating_add(text.len());
                self.cursor = grapheme_count(&self.text[..cursor_byte.min(self.text.len())]);
                self.grapheme_len = grapheme_count(&self.text);
                self.finish_edit();
            }
        }
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
            self.cursor.saturating_add(1).min(self.grapheme_len),
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
            .unwrap_or(self.grapheme_len);
        self.move_cursor(end, extend_selection);
    }

    pub fn move_visual_home(&mut self, extend_selection: bool, width: usize, prefix: usize) {
        let layout = self.visual_layout(width, prefix);
        if let Some(line) = layout
            .lines()
            .iter()
            .rfind(|line| line.start <= self.cursor && self.cursor <= line.end)
        {
            self.move_cursor(line.start.max(prefix), extend_selection);
        }
    }

    pub fn move_visual_end(&mut self, extend_selection: bool, width: usize, prefix: usize) {
        let layout = self.visual_layout(width, prefix);
        if let Some(line) = layout
            .lines()
            .iter()
            .rfind(|line| line.start <= self.cursor && self.cursor <= line.end)
        {
            self.move_cursor(line.end, extend_selection);
        }
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
        let layout = self.visual_layout(width, prefix);
        let Some(line) = layout.lines().get(row) else {
            return false;
        };
        let target = layout.cursor_at_cell(&self.text, line.clone(), column);
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
        self.syntax.remove(&self.text[start..end]);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
        self.grapheme_len = self.grapheme_len.saturating_sub(1);
        self.finish_edit();
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            self.finish_edit();
            return;
        }
        if self.cursor == self.grapheme_len {
            return;
        }
        let start = grapheme_byte(&self.text, self.cursor);
        let end = grapheme_byte(&self.text, self.cursor + 1);
        self.syntax.remove(&self.text[start..end]);
        self.text.replace_range(start..end, "");
        self.grapheme_len = self.grapheme_len.saturating_sub(1);
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
        self.grapheme_len = 0;
        self.syntax = SyntaxCounts::default();
        self.mark_text_changed();
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
        if !submitted.is_empty() {
            self.grapheme_len = 0;
            self.syntax = SyntaxCounts::default();
            self.mark_text_changed();
        }
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
        if self.text != text {
            self.text = text;
            self.grapheme_len = grapheme_count(&self.text);
            self.syntax = SyntaxCounts::in_text(&self.text);
            self.mark_text_changed();
        }
        self.cursor = self
            .text
            .graphemes(true)
            .position(|grapheme| grapheme == "\n")
            .unwrap_or(self.grapheme_len);
        self.selection = None;
        self.selection_anchor = None;
        self.history_loaded = true;
        self.cursor_moved_since_history_load = false;
        self.discard_joined_character = false;
    }

    fn move_visual_line(&mut self, width: usize, prefix: usize, up: bool) -> bool {
        let layout = self.visual_layout(width, prefix);
        let lines = layout.lines();
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
        let column = layout.cell_width(&self.text, lines[line_index].start..self.cursor);
        let target = layout.cursor_at_cell(&self.text, lines[target_index].clone(), column);
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
        self.syntax.remove(&self.text[start..end]);
        self.text.replace_range(start..end, "");
        self.grapheme_len = self.grapheme_len.saturating_sub(selection.len());
        self.cursor = selection.start;
        true
    }

    fn finish_edit(&mut self) {
        self.mark_text_changed();
        self.reset_history_navigation();
        self.discard_joined_character = false;
    }

    fn finish_append(&mut self) {
        self.revision = self.revision.saturating_add(1);
        for cached in self.visual_layouts.borrow_mut().iter_mut() {
            Arc::make_mut(&mut cached.layout).refresh_appended(
                &self.text,
                cached.width,
                cached.prefix,
            );
        }
        self.reset_history_navigation();
        self.discard_joined_character = false;
    }

    fn mark_history_cursor_moved(&mut self, target: usize) {
        if self.history_loaded && target != self.cursor {
            self.cursor_moved_since_history_load = true;
        }
    }

    fn mark_text_changed(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.visual_layouts.borrow_mut().clear();
    }

    #[must_use]
    pub(crate) const fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn visual_layout(&self, width: usize, prefix: usize) -> Arc<VisualLayout> {
        let width = width.max(1);
        if let Some(cached) = self
            .visual_layouts
            .borrow()
            .iter()
            .find(|cached| cached.width == width && cached.prefix == prefix)
        {
            return Arc::clone(&cached.layout);
        }
        let layout = Arc::new(build_visual_layout(&self.text, width, prefix));
        let mut layouts = self.visual_layouts.borrow_mut();
        if layouts.len() == 2 {
            layouts.remove(0);
        }
        layouts.push(CachedVisualLayout {
            width,
            prefix,
            layout: Arc::clone(&layout),
        });
        layout
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

fn build_visual_layout(text: &str, width: usize, prefix: usize) -> VisualLayout {
    let graphemes = text
        .grapheme_indices(true)
        .map(|(byte, grapheme)| byte..byte.saturating_add(grapheme.len()))
        .collect::<Vec<_>>();
    let lines = build_visual_lines(text, &graphemes, width.max(1), prefix.min(graphemes.len()));
    VisualLayout { graphemes, lines }
}

fn build_visual_lines(
    text: &str,
    graphemes: &[Range<usize>],
    width: usize,
    start: usize,
) -> Vec<Range<usize>> {
    let mut starts = vec![start];
    let mut line_start = start;
    let mut line_width = 0usize;
    let mut last_break = None;
    let mut index = start;
    while index < graphemes.len() {
        let grapheme = text.get(graphemes[index].clone()).unwrap_or_default();
        if grapheme == "\n" {
            let next = index.saturating_add(1);
            starts.push(next);
            line_start = next;
            line_width = 0;
            last_break = None;
            index = next;
            continue;
        }
        let grapheme_width = wrap_grapheme_width(grapheme);
        if line_width.saturating_add(grapheme_width) > width {
            let next = last_break
                .filter(|candidate| *candidate > line_start)
                .unwrap_or(index);
            if starts.last().copied() != Some(next) {
                starts.push(next);
            }
            line_start = next;
            line_width = grapheme_range_wrap_width(text, graphemes, next..index);
            last_break = (next..index)
                .rev()
                .find(|candidate| {
                    text.get(graphemes[*candidate].clone())
                        .is_some_and(|grapheme| grapheme.chars().all(char::is_whitespace))
                })
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
            if line_end > *start
                && graphemes
                    .get(line_end - 1)
                    .and_then(|range| text.get(range.clone()))
                    == Some("\n")
            {
                line_end -= 1;
            }
            *start..line_end
        })
        .collect()
}

fn grapheme_range_wrap_width(text: &str, graphemes: &[Range<usize>], range: Range<usize>) -> usize {
    range.fold(0, |column, index| {
        let grapheme = graphemes
            .get(index)
            .and_then(|range| text.get(range.clone()))
            .unwrap_or_default();
        column.saturating_add(wrap_grapheme_width(grapheme))
    })
}

fn wrap_grapheme_width(grapheme: &str) -> usize {
    // Textual's word wrapper treats a tab as a four-cell token. Cursor and
    // mouse coordinates below use the terminal's next-four-column tab stop.
    if grapheme == "\t" {
        4
    } else if grapheme.chars().all(char::is_control) {
        0
    } else {
        UnicodeWidthStr::width(grapheme).max(1)
    }
}

pub(crate) fn grapheme_cell_width(grapheme: &str, column: usize) -> usize {
    if grapheme == "\t" {
        4 - (column % 4)
    } else if grapheme.chars().all(char::is_control) {
        0
    } else {
        UnicodeWidthStr::width(grapheme).max(1)
    }
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

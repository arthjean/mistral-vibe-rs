//! Where the composer's text wraps: the visual lines a prompt occupies at a
//! width, and the terminal cells each grapheme takes on them.

use std::ops::Range;
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone)]
pub(super) struct CachedVisualLayout {
    pub(super) width: usize,
    pub(super) prefix: usize,
    pub(super) layout: Arc<VisualLayout>,
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

    pub(super) fn refresh_appended(&mut self, text: &str, width: usize, prefix: usize) {
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

pub(super) fn build_visual_layout(text: &str, width: usize, prefix: usize) -> VisualLayout {
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

fn grapheme_cell_width(grapheme: &str, column: usize) -> usize {
    if grapheme == "\t" {
        4 - (column % 4)
    } else if grapheme.chars().all(char::is_control) {
        0
    } else {
        UnicodeWidthStr::width(grapheme).max(1)
    }
}

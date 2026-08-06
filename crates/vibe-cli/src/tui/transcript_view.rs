//! Transcript interaction: what the last frame painted, what the operator
//! selected in it, and which of its links may be activated.
//!
//! The reference selects text with the mouse, copies it explicitly or
//! automatically on release, and opens only validated links. This view keeps
//! the same contract without any I/O: the renderer publishes the visible lines,
//! and every action resolves against them.

use super::diagnostics::safe_link_spans;

/// A cell in the painted transcript: the visible line and the column inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cell {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptView {
    lines: Vec<String>,
    origin_row: u16,
    anchor: Option<Cell>,
    head: Option<Cell>,
    dragged: bool,
}

impl TranscriptView {
    /// Publishes what the frame painted. An in-flight selection is dropped when
    /// the transcript reflows, because its anchor no longer describes anything.
    pub fn publish(&mut self, origin_row: u16, lines: Vec<String>) {
        if self.lines != lines {
            let reflowed = self.lines.len() != lines.len();
            self.lines = lines;
            if reflowed {
                self.clear_selection();
            }
        }
        self.origin_row = origin_row;
    }

    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
        self.head = None;
        self.dragged = false;
    }

    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.selected_text().is_some()
    }

    /// Maps a terminal coordinate onto a painted cell. Coordinates outside the
    /// transcript return `None` rather than inventing a nearest target.
    #[must_use]
    pub fn cell_at(&self, column: u16, row: u16) -> Option<Cell> {
        let line = usize::from(row.checked_sub(self.origin_row)?);
        let text = self.lines.get(line)?;
        Some(Cell {
            line,
            column: usize::from(column).min(text.chars().count()),
        })
    }

    pub fn begin_selection(&mut self, cell: Cell) {
        self.anchor = Some(cell);
        self.head = Some(cell);
        self.dragged = false;
    }

    pub fn extend_selection(&mut self, cell: Cell) {
        if self.anchor.is_none() {
            self.begin_selection(cell);
            return;
        }
        if self.head != Some(cell) {
            self.dragged = true;
        }
        self.head = Some(cell);
    }

    /// A press and release on the same cell is a click, not a selection: the
    /// reference treats that as link activation.
    #[must_use]
    pub fn is_click(&self) -> bool {
        !self.dragged
    }

    /// Grows or shrinks the selection by whole lines from the bottom of the
    /// transcript, so selecting and copying never require a pointer. Bringing
    /// the head back to its anchor clears the selection.
    pub fn move_selection(&mut self, lines: isize) -> bool {
        let Some(last) = self.lines.len().checked_sub(1) else {
            return false;
        };
        let anchor = *self.anchor.get_or_insert(Cell {
            line: last,
            column: self.lines.get(last).map_or(0, |text| text.chars().count()),
        });
        let head = self.head.unwrap_or(anchor);
        // How many whole lines are selected above the anchor, so one step of
        // the first move selects the line the anchor sits on.
        let selected = if head == anchor {
            0
        } else {
            anchor.line.saturating_sub(head.line).saturating_add(1)
        };
        let target = selected
            .saturating_add_signed(lines)
            .min(anchor.line.saturating_add(1));
        self.head = Some(if target == 0 {
            anchor
        } else {
            Cell {
                line: anchor.line.saturating_add(1).saturating_sub(target),
                column: 0,
            }
        });
        self.dragged = true;
        target != selected
    }

    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.ordered_bounds()?;
        if start == end {
            return None;
        }
        let mut selected: Vec<String> = Vec::new();
        for line in start.line..=end.line {
            let text = self.lines.get(line)?;
            let characters = text.chars().collect::<Vec<_>>();
            let from = if line == start.line { start.column } else { 0 };
            let to = if line == end.line {
                end.column.min(characters.len())
            } else {
                characters.len()
            };
            selected.push(
                characters
                    .get(from..to)
                    .unwrap_or_default()
                    .iter()
                    .collect(),
            );
        }
        let selected: String = selected.join("\n");
        (!selected.trim().is_empty()).then_some(selected)
    }

    /// The selected column range of every touched line, so the frame can show
    /// exactly what a copy would take.
    #[must_use]
    pub fn selection_ranges(&self) -> Vec<(usize, usize, usize)> {
        let Some((start, end)) = self.ordered_bounds() else {
            return Vec::new();
        };
        (start.line..=end.line)
            .filter_map(|line| {
                let width = self.lines.get(line)?.chars().count();
                let from = if line == start.line { start.column } else { 0 };
                let to = if line == end.line {
                    end.column.min(width)
                } else {
                    width
                };
                (from < to).then_some((line, from, to))
            })
            .collect()
    }

    /// The validated link under a cell, if any. Unsafe schemes never resolve,
    /// so activation cannot reach the system opener with one.
    #[must_use]
    pub fn link_at(&self, cell: Cell) -> Option<String> {
        let text = self.lines.get(cell.line)?;
        let characters = text.chars().collect::<Vec<_>>();
        for (start, end) in safe_link_spans(text) {
            let start = text[..start].chars().count();
            let end = text[..end].chars().count();
            if (start..end).contains(&cell.column) {
                return Some(characters.get(start..end)?.iter().collect());
            }
        }
        None
    }

    fn ordered_bounds(&self) -> Option<(Cell, Cell)> {
        let (anchor, head) = (self.anchor?, self.head?);
        Some(if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> TranscriptView {
        let mut view = TranscriptView::default();
        view.publish(
            2,
            vec![
                "> deploy the service".to_owned(),
                "See https://ratatui.rs for details".to_owned(),
                "and file:///etc/passwd is not a link".to_owned(),
            ],
        );
        view
    }

    #[test]
    fn coordinates_outside_the_transcript_never_resolve_to_a_cell() {
        let view = view();
        assert_eq!(view.cell_at(0, 1), None);
        assert_eq!(view.cell_at(0, 5), None);
        assert_eq!(view.cell_at(2, 2), Some(Cell { line: 0, column: 2 }));
        // A column past the painted text clamps to the end of that line.
        assert_eq!(
            view.cell_at(200, 2),
            Some(Cell {
                line: 0,
                column: "> deploy the service".len()
            })
        );
    }

    #[test]
    fn dragging_selects_across_lines_and_a_click_selects_nothing() {
        let mut view = view();
        view.begin_selection(Cell { line: 0, column: 2 });
        assert!(view.is_click());
        assert_eq!(view.selected_text(), None);

        view.extend_selection(Cell { line: 1, column: 3 });
        assert!(!view.is_click());
        assert_eq!(
            view.selected_text().as_deref(),
            Some("deploy the service\nSee")
        );

        // Selecting backward yields the same text.
        let mut backward = view.clone();
        backward.begin_selection(Cell { line: 1, column: 3 });
        backward.extend_selection(Cell { line: 0, column: 2 });
        assert_eq!(backward.selected_text(), view.selected_text());

        view.clear_selection();
        assert!(!view.has_selection());
    }

    #[test]
    fn the_selection_is_reachable_and_reversible_without_a_pointer() {
        let mut view = view();
        assert!(view.move_selection(1), "the first move opens a selection");
        assert_eq!(
            view.selected_text().as_deref(),
            Some("and file:///etc/passwd is not a link")
        );
        assert!(view.move_selection(1));
        assert_eq!(
            view.selected_text().as_deref(),
            Some("See https://ratatui.rs for details\nand file:///etc/passwd is not a link")
        );
        // The top of the transcript bounds the selection.
        assert!(view.move_selection(9));
        assert!(!view.move_selection(9));
        assert_eq!(view.selected_text().unwrap().lines().count(), 3);
        // Returning to the anchor leaves nothing selected.
        view.move_selection(-9);
        assert_eq!(view.selected_text(), None);

        assert!(!TranscriptView::default().move_selection(1));
    }

    #[test]
    fn only_safe_links_activate_and_only_under_their_own_span() {
        let view = view();
        assert_eq!(
            view.link_at(Cell { line: 1, column: 6 }).as_deref(),
            Some("https://ratatui.rs")
        );
        assert_eq!(view.link_at(Cell { line: 1, column: 0 }), None);
        assert_eq!(
            view.link_at(Cell {
                line: 1,
                column: 30
            }),
            None
        );
        assert_eq!(view.link_at(Cell { line: 2, column: 6 }), None);
    }

    #[test]
    fn a_reflowed_transcript_drops_a_selection_it_can_no_longer_describe() {
        let mut view = view();
        view.begin_selection(Cell { line: 0, column: 0 });
        view.extend_selection(Cell { line: 1, column: 3 });
        assert!(view.has_selection());

        // Same height: streaming rewrote a line, the anchor still means something.
        view.publish(
            2,
            vec![
                "> deploy the service".to_owned(),
                "See https://ratatui.rs for details now".to_owned(),
                "and file:///etc/passwd is not a link".to_owned(),
            ],
        );
        assert!(view.has_selection());

        // A resize changed the line count, so the selection cannot survive.
        view.publish(2, vec!["> deploy the".to_owned()]);
        assert!(!view.has_selection());
        assert_eq!(view.cell_at(2, 3), None);
    }
}

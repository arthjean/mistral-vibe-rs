use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::truncate_width;
use crate::tui::completion::{CompletionEngine, CompletionView};
use crate::tui::setup::ResolvedTheme;

const MAX_HEIGHT: u16 = 12;
const LABEL_WIDTH_PERCENT: u16 = 30;
const HORIZONTAL_CHROME: usize = 4;
const COLUMN_GAP: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PopupRow {
    label: String,
    description: String,
    selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PopupLayout {
    has_descriptions: bool,
    label_width: usize,
    rows: Vec<PopupRow>,
}

pub(super) fn requested_height(completion: &CompletionEngine) -> u16 {
    completion.view().map_or(0, |view| {
        u16::try_from(view.candidates.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(MAX_HEIGHT)
    })
}

pub(super) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    completion: &CompletionEngine,
    theme: ResolvedTheme,
) {
    let Some(view) = completion.view() else {
        return;
    };
    let PopupLayout {
        has_descriptions,
        label_width,
        rows,
    } = popup_layout(view, area.width, area.height);
    let lines = rows
        .into_iter()
        .map(|row| {
            let label_style = if row.selected {
                theme
                    .base()
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::REVERSED)
            } else {
                theme.base().add_modifier(Modifier::BOLD)
            };
            let description_style = if row.selected {
                theme.base().add_modifier(Modifier::ITALIC)
            } else {
                theme.muted()
            };
            if has_descriptions {
                Line::from(vec![
                    Span::styled(pad_to_width(row.label, label_width), label_style),
                    Span::raw("  "),
                    Span::styled(row.description, description_style),
                ])
            } else {
                Line::from(Span::styled(row.label, label_style))
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .padding(Padding::horizontal(1))
                .border_style(theme.muted()),
        ),
        area,
    );
}

fn popup_layout(view: CompletionView<'_>, width: u16, height: u16) -> PopupLayout {
    let inner_width = usize::from(width).saturating_sub(HORIZONTAL_CHROME);
    let has_descriptions = view
        .candidates
        .iter()
        .any(|candidate| !candidate.description.is_empty());
    let label_cap = if has_descriptions {
        usize::from(width.saturating_mul(LABEL_WIDTH_PERCENT) / 100)
    } else {
        inner_width
    };
    let label_width = view
        .candidates
        .iter()
        .map(|candidate| UnicodeWidthStr::width(candidate.label.trim_start_matches('@')))
        .max()
        .unwrap_or_default()
        .min(label_cap);
    let visible_rows = usize::from(height.saturating_sub(2));
    let first_visible = view.selected.saturating_add(1).saturating_sub(visible_rows);
    let description_width = inner_width
        .saturating_sub(label_width)
        .saturating_sub(COLUMN_GAP);
    let rows = view
        .candidates
        .iter()
        .enumerate()
        .skip(first_visible)
        .take(visible_rows)
        .map(|(index, candidate)| PopupRow {
            label: truncate_width(candidate.label.trim_start_matches('@'), label_width),
            description: truncate_width(&candidate.description, description_width),
            selected: index == view.selected,
        })
        .collect();
    PopupLayout {
        has_descriptions,
        label_width,
        rows,
    }
}

fn pad_to_width(mut value: String, width: usize) -> String {
    let padding = width.saturating_sub(UnicodeWidthStr::width(value.as_str()));
    value.extend(std::iter::repeat_n(' ', padding));
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::completion::{CompletionCandidate, CompletionKind};

    fn candidate(label: &str, description: &str) -> CompletionCandidate {
        CompletionCandidate {
            id: label.to_owned(),
            kind: CompletionKind::SlashCommand,
            label: label.to_owned(),
            insertion: label.to_owned(),
            description: description.to_owned(),
        }
    }

    #[test]
    fn layout_enforces_label_cap_description_width_and_scroll_window() {
        let candidates = vec![
            candidate("/short", "first"),
            candidate("/extraordinarily-long-command", "a very long description"),
            candidate("/last", "third"),
        ];
        let layout = popup_layout(
            CompletionView {
                candidates: &candidates,
                selected: 2,
            },
            20,
            4,
        );

        assert_eq!(layout.label_width, 6);
        assert!(layout.has_descriptions);
        assert_eq!(
            layout.rows,
            vec![
                PopupRow {
                    label: "/extr…".to_owned(),
                    description: "a very …".to_owned(),
                    selected: false,
                },
                PopupRow {
                    label: "/last".to_owned(),
                    description: "third".to_owned(),
                    selected: true,
                },
            ]
        );
    }

    #[test]
    fn layout_counts_terminal_cells_for_wide_labels() {
        let candidates = vec![candidate("@界界界", "path")];
        let layout = popup_layout(
            CompletionView {
                candidates: &candidates,
                selected: 0,
            },
            20,
            3,
        );

        assert_eq!(layout.label_width, 6);
        assert_eq!(layout.rows[0].label, "界界界");
    }

    #[test]
    fn layout_gives_the_full_inner_width_to_labels_without_descriptions() {
        let candidates = vec![candidate("@abcdefghijklmnopqr", "")];
        let layout = popup_layout(
            CompletionView {
                candidates: &candidates,
                selected: 0,
            },
            20,
            3,
        );

        assert!(!layout.has_descriptions);
        assert_eq!(layout.label_width, 16);
        assert_eq!(layout.rows[0].label, "abcdefghijklmno…");
    }
}

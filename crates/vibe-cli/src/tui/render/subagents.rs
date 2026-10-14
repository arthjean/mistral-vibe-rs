//! Paints reference `SubagentList`: the main conversation's row, then one row
//! per child of the current batch, under a rule that names the child being
//! viewed.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::super::setup::ResolvedTheme;
use super::super::subagents::{
    ChildSession, ChildStatus, MAX_LIST_HEIGHT, Subagents, display_name,
};
use super::format_token_count;
use super::text::{sanitize_inline, truncate_width};

/// The rows the list takes, its rule included; 0 hides it.
pub(super) fn list_height(subagents: &Subagents) -> u16 {
    if !subagents.visible() {
        return 0;
    }
    let rows = 1 + subagents.shown().len() + usize::from(subagents.is_viewing());
    u16::try_from(rows.min(MAX_LIST_HEIGHT)).unwrap_or(u16::MAX)
}

/// Paints the list into `area`, answering where its rows landed and which
/// row the first of them is.
pub(super) fn draw_list(
    frame: &mut Frame<'_>,
    area: Rect,
    subagents: &Subagents,
    theme: ResolvedTheme,
) -> Option<(Rect, usize)> {
    if area.height == 0 || !subagents.visible() {
        return None;
    }
    // Reference `padding: 0 1`.
    let inner = Rect {
        x: area.x.saturating_add(1),
        width: area.width.saturating_sub(2),
        ..area
    };
    let width = usize::from(inner.width.max(1));
    let mut lines = Vec::new();
    if let Some(viewed) = subagents.viewed() {
        lines.push(rule(
            &display_name(&viewed.name),
            usize::from(area.width),
            theme,
        ));
    }
    let row_capacity = usize::from(area.height).saturating_sub(lines.len());
    let rows = 1 + subagents.shown().len();
    // Keep the highlighted row on screen once the rows outgrow the list.
    let first = subagents
        .highlighted()
        .saturating_add(1)
        .saturating_sub(row_capacity)
        .min(rows.saturating_sub(row_capacity));
    let viewed_id = subagents.viewed().map(|child| child.id.as_str());
    for row in (first..rows).take(row_capacity) {
        let marked = subagents.focused() && row == subagents.highlighted();
        let highlighted = row == subagents.highlighted();
        let (spans, selected) = match row.checked_sub(1) {
            None => (
                vec![Span::styled(
                    "Main conversation",
                    theme.base().add_modifier(Modifier::BOLD),
                )],
                viewed_id.is_none(),
            ),
            Some(index) => {
                let child = &subagents.shown()[index];
                (
                    child_spans(child, theme),
                    viewed_id == Some(child.id.as_str()),
                )
            }
        };
        let mut line = vec![Span::raw(if marked { "> " } else { "  " })];
        line.extend(spans);
        let mut line = truncated(Line::from(line), width);
        if selected || highlighted {
            line = line.patch_style(theme.orange());
        }
        lines.push(line);
    }
    let rows_area = Rect {
        y: inner.y.saturating_add(u16::from(subagents.is_viewing())),
        height: inner
            .height
            .saturating_sub(u16::from(subagents.is_viewing())),
        ..inner
    };
    let rule_lines = usize::from(subagents.is_viewing());
    if subagents.is_viewing() {
        frame.render_widget(
            Paragraph::new(lines[..rule_lines].to_vec()),
            Rect { height: 1, ..area },
        );
    }
    frame.render_widget(Paragraph::new(lines[rule_lines..].to_vec()), rows_area);
    Some((rows_area, first))
}

/// Reference `_child_row`.
fn child_spans(child: &ChildSession, theme: ResolvedTheme) -> Vec<Span<'static>> {
    let status_style = match child.status {
        ChildStatus::Running => theme.success(),
        ChildStatus::Blocked => theme.warning(),
        ChildStatus::Failed => theme.error(),
        ChildStatus::Archived | ChildStatus::Idle | ChildStatus::Unknown => theme.muted(),
    };
    vec![
        Span::styled(
            sanitize_inline(&display_name(&child.agent_type)),
            theme.base().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" ({}) ", sanitize_inline(&display_name(&child.name))),
            theme.base(),
        ),
        Span::styled(format!("[{}]", child.status.label()), status_style),
        Span::styled(
            format!(" · {} tokens", format_token_count(child.context_tokens)),
            theme.muted(),
        ),
    ]
}

/// Reference `border-top` with its title aligned right.
fn rule(name: &str, width: usize, theme: ResolvedTheme) -> Line<'static> {
    let title = truncate_width(
        &format!(
            " Viewing {} · read-only · Esc to return ",
            sanitize_inline(name)
        ),
        width.saturating_sub(2),
    );
    let fill = width.saturating_sub(title.width()).saturating_sub(1);
    Line::from(vec![
        Span::styled("─".repeat(fill), theme.base()),
        Span::styled(title, theme.base()),
        Span::styled("─", theme.base()),
    ])
}

/// Cuts a styled line to `width` columns, closing it with an ellipsis.
fn truncated(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    let mut remaining = width.saturating_sub(1);
    let mut spans = Vec::new();
    for span in line.spans {
        let mut text = String::new();
        for character in span.content.chars() {
            let columns = UnicodeWidthChar::width(character).unwrap_or(0);
            if columns > remaining {
                remaining = 0;
                break;
            }
            remaining -= columns;
            text.push(character);
        }
        spans.push(Span::styled(text, span.style));
        if remaining == 0 {
            break;
        }
    }
    spans.push(Span::styled("…", Style::default()));
    Line::from(spans)
}

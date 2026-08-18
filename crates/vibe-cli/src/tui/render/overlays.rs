//! The two floating panels the frame paints over the transcript: the callback
//! prompt and the generic selection overlay.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use super::text::{sanitize_inline, truncate_width, wrap_visual_line};
use crate::tui::controls::CallbackPresentation;
use crate::tui::interaction::{Overlay, OverlayKind};
use crate::tui::session_picker::SessionDeleteState;
use crate::tui::setup::ResolvedTheme;

pub(super) fn draw_callback_overlay(
    frame: &mut Frame<'_>,
    presentation: &CallbackPresentation,
    scroll_offset: isize,
    theme: ResolvedTheme,
) {
    let outer = frame.area();
    let width = outer.width.saturating_sub(4).clamp(1, 96);
    let available_height = outer.height.saturating_sub(2).max(1);
    let inner_width = usize::from(width.saturating_sub(4).max(1));
    let mut content = Vec::new();
    let mut focus_line = 0;
    for (index, line) in presentation.lines.iter().enumerate() {
        if index == presentation.focus_line {
            focus_line = content.len();
        }
        wrap_visual_line(&sanitize_inline(line), inner_width, &mut content);
    }
    let height = u16::try_from(content.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(available_height);
    let area = Rect::new(
        outer
            .x
            .saturating_add(outer.width.saturating_sub(width) / 2),
        outer
            .y
            .saturating_add(outer.height.saturating_sub(height) / 2),
        width,
        height,
    );
    let visible_height = usize::from(height.saturating_sub(2).max(1));
    let target_line = focus_line.saturating_add_signed(scroll_offset);
    let start = target_line
        .saturating_sub(visible_height / 2)
        .min(content.len().saturating_sub(visible_height));
    let lines = content
        .iter()
        .skip(start)
        .take(visible_height)
        .map(|line| Line::raw(line.clone()))
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Action required ")
                    .padding(Padding::horizontal(1))
                    .style(theme.base()),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

pub(super) fn draw_overlay(
    frame: &mut Frame<'_>,
    overlay: &Overlay,
    session_delete: Option<&SessionDeleteState>,
    theme: ResolvedTheme,
) {
    let outer = frame.area();
    let width = outer.width.saturating_sub(4).clamp(1, 86);
    let visible = overlay.visible_items();
    // Rows, plus the optional filter and notice blocks, the blank separator, the
    // help line, and the border. Under-counting silently drops the help line.
    let content_height = u16::try_from(visible.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4)
        .saturating_add(if overlay.query.is_empty() { 0 } else { 2 })
        .saturating_add(if overlay.notice.is_some() { 2 } else { 0 });
    let available_height = outer.height.saturating_sub(2).max(1);
    let height = content_height.min(available_height);
    let area = Rect::new(
        outer
            .x
            .saturating_add(outer.width.saturating_sub(width) / 2),
        outer
            .y
            .saturating_add(outer.height.saturating_sub(height) / 2),
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let inner_width = usize::from(width.saturating_sub(4).max(1));
    let mut lines = Vec::with_capacity(visible.len().saturating_add(3));
    if !overlay.query.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Filter: ", theme.muted()),
            Span::raw(truncate_width(
                &overlay.query,
                inner_width.saturating_sub(8),
            )),
        ]));
        lines.push(Line::default());
    }
    for (selected, item) in visible {
        let description = session_delete
            .filter(|delete| delete.session_id == item.id)
            .map_or(item.description.as_str(), SessionDeleteState::message);
        let marker = if selected { "▸ " } else { "  " };
        let style = if item.disabled {
            theme.muted()
        } else if selected {
            theme.secondary().add_modifier(Modifier::BOLD)
        } else {
            theme.base()
        };
        let description_width = inner_width.saturating_sub(item.label.width().saturating_add(5));
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(truncate_width(&item.label, inner_width), style),
            Span::raw("  "),
            Span::styled(
                truncate_width(description, description_width),
                theme.muted(),
            ),
        ]));
    }
    if let Some(notice) = &overlay.notice {
        lines.push(Line::default());
        lines.push(Line::styled(
            truncate_width(notice, inner_width),
            theme.warning(),
        ));
    }
    lines.push(Line::default());
    let help = match overlay.kind {
        OverlayKind::Config => "↑↓/jk Navigate  Enter Edit  Ctrl+R Reset  Esc Close",
        OverlayKind::Sessions => "↑↓/jk Navigate  Enter Resume  Delete Remove  Esc Close",
        OverlayKind::Mcp | OverlayKind::Connectors => {
            "↑↓/jk Navigate  Enter Show tools  d Disable  e Enable  Ctrl+R Refresh  Esc Close"
        }
        OverlayKind::McpDetail => "↑↓/jk Navigate  d Disable  e Enable  Backspace Back  Esc Close",
        OverlayKind::RemoteProjectCreate => {
            "↑↓/Tab Field  Type Edit  Enter Next/Create  Esc Cancel"
        }
        _ => "↑↓/jk Navigate  Enter Select  Esc Close",
    };
    lines.push(Line::styled(help, theme.muted()));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", overlay.title))
                    .padding(Padding::horizontal(1))
                    .style(theme.base()),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

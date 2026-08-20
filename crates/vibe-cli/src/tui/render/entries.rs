//! One transcript entry, projected to the lines the frame paints.
//!
//! [`super::transcript`] decides what an entry *means*; this module decides how
//! that meaning is laid out. The split is what keeps the semantic projection
//! free of ratatui and testable without a terminal.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::markdown::markdown_lines;
use super::text::{MAX_RENDER_LINES, sanitize_inline, truncate_width, wrapped_terminal_lines};
use crate::tui::composer_layout::PROMPT_WIDTH;
use crate::tui::setup::ResolvedTheme;
use crate::tui::state::{EntryStatus, TranscriptEntry};
use crate::tui::transcript;

/// Renders one canonical entry into its reference semantic region. The caller
/// owns the separation between regions, so these lines never open with a blank.
pub(super) fn semantic_lines(
    entry: &TranscriptEntry,
    width: u16,
    theme: ResolvedTheme,
    tools_collapsed: bool,
) -> Vec<Line<'static>> {
    match transcript::region(entry) {
        transcript::Region::UserMessage => user_message_lines(entry, width, theme),
        transcript::Region::AssistantMessage => assistant_message_lines(entry, width, theme),
        transcript::Region::Reasoning => {
            prefixed_lines(&entry.text, "  ⋮ ", width, theme.muted(), theme, entry)
        }
        transcript::Region::Effect(effect) => effect_lines(&effect, width, theme, tools_collapsed),
        transcript::Region::Callback { title, detail } => {
            let mut lines = vec![Line::from(vec![
                Span::styled("? ", theme.warning()),
                Span::styled(
                    truncate_width(&sanitize_inline(&title), usize::from(width.max(1))),
                    theme.warning(),
                ),
            ])];
            lines.extend(body_lines(&detail, "  ", width, theme.base()));
            append_terminal_status(&mut lines, entry, theme);
            lines
        }
        transcript::Region::Compaction { message } => {
            let mut lines = vec![Line::styled(
                truncate_width("Compacting conversation", usize::from(width.max(1))),
                theme.muted(),
            )];
            lines.extend(body_lines(&message, "  ", width, theme.muted()));
            lines
        }
        transcript::Region::Checkpoint { message } => {
            prefixed_lines(&message, "  ⎔ ", width, theme.muted(), theme, entry)
        }
        transcript::Region::Hook { icon, line } => {
            let content_width = usize::from(width.saturating_sub(4).max(1));
            wrapped_terminal_lines(&line, content_width)
                .into_iter()
                .enumerate()
                .map(|(index, text)| {
                    Line::from(vec![
                        Span::styled(
                            if index == 0 {
                                format!("{icon} ")
                            } else {
                                "  ".to_owned()
                            },
                            theme.warning(),
                        ),
                        Span::styled(text, theme.muted()),
                    ])
                })
                .collect()
        }
        transcript::Region::Command { message } => {
            prefixed_lines(&message, "  ▏ ", width, theme.secondary(), theme, entry)
        }
        transcript::Region::Document { message } => document_lines(&message, width, theme),
        transcript::Region::Notice { level, .. } => {
            notice_message_lines(entry, width, theme, level)
        }
        transcript::Region::Plan => {
            prefixed_lines(&entry.text, "  ", width, theme.assistant(), theme, entry)
        }
    }
}

fn prefixed_lines(
    text: &str,
    prefix: &str,
    width: u16,
    style: Style,
    theme: ResolvedTheme,
    entry: &TranscriptEntry,
) -> Vec<Line<'static>> {
    let mut lines = body_lines(text, prefix, width, style);
    append_terminal_status(&mut lines, entry, theme);
    lines
}

fn body_lines(text: &str, prefix: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let content_width = usize::from(width).saturating_sub(prefix.width()).max(1);
    let prefix = prefix.to_owned();
    wrapped_terminal_lines(text, content_width)
        .into_iter()
        .take(MAX_RENDER_LINES)
        .map(|line| Line::from(vec![Span::raw(prefix.clone()), Span::styled(line, style)]))
        .collect()
}

/// Reference tool call and result presentation: one status header carrying the
/// authoritative indicator, the running stream, and the settled body.
fn effect_lines(
    effect: &transcript::EffectRegion,
    width: u16,
    theme: ResolvedTheme,
    tools_collapsed: bool,
) -> Vec<Line<'static>> {
    let max_width = usize::from(width.max(1));
    let indicator_style = match effect.indicator {
        transcript::Indicator::Running => theme.orange(),
        transcript::Indicator::Success => theme.success(),
        transcript::Indicator::Error => theme.error(),
        transcript::Indicator::Muted => theme.muted(),
    };
    let mut header = vec![Span::styled(
        format!("{} ", effect.indicator.glyph()),
        indicator_style,
    )];
    if !effect.verb.is_empty() {
        header.push(Span::styled(
            format!("{} ", sanitize_inline(&effect.verb)),
            theme.effect(),
        ));
    }
    header.push(Span::styled(
        truncate_width(
            &sanitize_inline(&effect.message),
            max_width.saturating_sub(12),
        ),
        theme.base(),
    ));
    if !effect.suffix.is_empty() {
        header.push(Span::styled(
            format!(" {}", sanitize_inline(&effect.suffix)),
            theme.muted(),
        ));
    }
    // The reference collapses collapsible results into their header; diff and
    // question results always render in full.
    let collapsed = effect.collapsed_by_default && tools_collapsed;
    if collapsed && !effect.body.is_empty() {
        header.push(Span::styled(
            format!(" · {} lines (Ctrl+O)", effect.body.len()),
            theme.muted(),
        ));
    }
    let mut lines = vec![Line::from(header)];
    if let Some(stream) = &effect.stream {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                truncate_width(&sanitize_inline(stream), max_width.saturating_sub(2)),
                theme.muted(),
            ),
        ]));
    }
    if collapsed {
        return lines;
    }
    for body in effect
        .body
        .iter()
        .take(MAX_RENDER_LINES.saturating_sub(lines.len()))
    {
        let style = match body.style {
            transcript::BodyStyle::Plain => theme.base(),
            transcript::BodyStyle::Added => theme.success(),
            transcript::BodyStyle::Removed => theme.error(),
            transcript::BodyStyle::Warning => theme.warning(),
            transcript::BodyStyle::Error => theme.error(),
            transcript::BodyStyle::Muted => theme.muted(),
        };
        lines.push(Line::from(vec![
            Span::styled("  │ ", theme.muted()),
            Span::styled(
                truncate_width(&sanitize_inline(&body.text), max_width.saturating_sub(4)),
                style,
            ),
        ]));
    }
    lines
}

fn notice_message_lines(
    entry: &TranscriptEntry,
    width: u16,
    theme: ResolvedTheme,
    level: transcript::NoticeLevel,
) -> Vec<Line<'static>> {
    let content_style = match level {
        transcript::NoticeLevel::Info => theme.base(),
        transcript::NoticeLevel::Warning => theme.warning(),
        transcript::NoticeLevel::Error => theme.error(),
    };
    let content_width = usize::from(width.saturating_sub(4).max(1));
    let wrapped = wrapped_terminal_lines(&entry.text, content_width);
    let last = wrapped.len().saturating_sub(1);
    let mut lines = wrapped
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            Line::from(vec![
                Span::styled(if index == last { "  ⎣ " } else { "  ⎢ " }, theme.muted()),
                Span::styled(line, content_style),
            ])
        })
        .collect::<Vec<_>>();
    append_terminal_status(&mut lines, entry, theme);
    lines
}

fn user_message_lines(
    entry: &TranscriptEntry,
    width: u16,
    theme: ResolvedTheme,
) -> Vec<Line<'static>> {
    let (prompt, content) = match entry.text.chars().next() {
        Some('/') => ('/', &entry.text[1..]),
        _ => ('>', entry.text.as_str()),
    };
    let pending = entry.status == EntryStatus::Streaming;
    let prompt_style = theme
        .orange()
        .add_modifier(Modifier::BOLD)
        .add_modifier(if pending {
            Modifier::ITALIC
        } else {
            Modifier::empty()
        });
    let content_style = if prompt == '/' {
        theme.orange().add_modifier(Modifier::BOLD)
    } else {
        theme.base().add_modifier(Modifier::BOLD)
    }
    .add_modifier(if pending {
        Modifier::ITALIC
    } else {
        Modifier::empty()
    });
    let max_width = usize::from(width.saturating_sub(PROMPT_WIDTH).max(1));
    let mut lines = Vec::new();
    for (index, wrapped_line) in wrapped_terminal_lines(content, max_width)
        .into_iter()
        .take(MAX_RENDER_LINES.saturating_sub(lines.len()))
        .enumerate()
    {
        let prefix = if index == 0 {
            Span::styled(format!("{prompt} "), prompt_style)
        } else {
            Span::raw("  ")
        };
        lines.push(Line::from(vec![
            prefix,
            Span::styled(wrapped_line, content_style),
        ]));
    }
    append_terminal_status(&mut lines, entry, theme);
    if !pending && prompt != '/' {
        lines.push(Line::styled("─".repeat(usize::from(width)), theme.muted()));
    }
    lines
}

/// A command's own Markdown, rendered the way an assistant message is and
/// carried by the left border reference `UserCommandMessage` draws around it,
/// so a scrolled-back transcript still says the document came from a command.
fn document_lines(message: &str, width: u16, theme: ResolvedTheme) -> Vec<Line<'static>> {
    markdown_lines(message, usize::from(width.saturating_sub(1)), theme)
        .into_iter()
        .take(MAX_RENDER_LINES)
        .map(|line| {
            let mut spans = vec![Span::styled("\u{258f}", theme.secondary())];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

fn assistant_message_lines(
    entry: &TranscriptEntry,
    width: u16,
    theme: ResolvedTheme,
) -> Vec<Line<'static>> {
    let mut lines = markdown_lines(&entry.text, usize::from(width), theme)
        .into_iter()
        .take(MAX_RENDER_LINES)
        .collect::<Vec<_>>();
    append_terminal_status(&mut lines, entry, theme);
    lines
}

fn append_terminal_status(
    lines: &mut Vec<Line<'static>>,
    entry: &TranscriptEntry,
    theme: ResolvedTheme,
) {
    // An effect carries its failure inside its own region; every other entry
    // settles under a bare label, which is all the canonical entry publishes.
    let status = match entry.status {
        EntryStatus::Failed | EntryStatus::Cancelled | EntryStatus::Skipped => entry.status.label(),
        EntryStatus::Pending
        | EntryStatus::Streaming
        | EntryStatus::Blocked
        | EntryStatus::Completed => return,
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("({})", sanitize_inline(status)), theme.error()),
    ]));
}

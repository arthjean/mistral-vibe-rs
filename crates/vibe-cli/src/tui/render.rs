use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::input::PromptEditor;
use super::setup::{ResolvedTheme, Theme};
use super::state::{EntryStatus, TranscriptEntry, TranscriptKind, TuiState};

pub const MAX_RENDER_CHARS: usize = 64 * 1024;
pub const MAX_RENDER_LINE_CHARS: usize = 4 * 1024;
const MAX_RENDER_LINES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderLimits {
    pub max_chars: usize,
    pub max_line_chars: usize,
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            max_chars: MAX_RENDER_CHARS,
            max_line_chars: MAX_RENDER_LINE_CHARS,
        }
    }
}

pub fn draw(
    frame: &mut Frame<'_>,
    state: &mut TuiState,
    editor: &PromptEditor,
    theme: ResolvedTheme,
    secret_input: bool,
) {
    let [transcript_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(5),
    ])
    .areas(frame.area());
    draw_transcript(frame, transcript_area, state, theme);
    draw_status(frame, status_area, state, theme);
    draw_input(frame, input_area, editor, theme, secret_input);
}

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, state: &mut TuiState, theme: ResolvedTheme) {
    let visible_height = usize::from(area.height.saturating_sub(2)).max(1);
    let mut newest_first = Vec::new();
    let mut rendered_chars = 0usize;
    'entries: for entry in state.entries.iter().rev() {
        for line in semantic_lines(entry, area.width.saturating_sub(2), theme)
            .into_iter()
            .rev()
        {
            let line_chars = line.to_string().chars().count();
            if newest_first.len() >= MAX_RENDER_LINES
                || rendered_chars.saturating_add(line_chars) > MAX_RENDER_CHARS
            {
                break 'entries;
            }
            rendered_chars = rendered_chars.saturating_add(line_chars);
            newest_first.push(line);
        }
    }
    state.set_scroll_line_limit(newest_first.len().saturating_sub(visible_height));
    let lines = newest_first
        .into_iter()
        .skip(state.scroll_offset)
        .take(visible_height)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    let transcript = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title("Transcript"));
    frame.render_widget(transcript, area);
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, state: &TuiState, theme: ResolvedTheme) {
    let connection = if state.connected {
        "connected"
    } else {
        "disconnected"
    };
    let activity = if state.resync_required {
        "resync required"
    } else if state.waiting {
        "waiting"
    } else if state.ready {
        "ready"
    } else {
        "starting"
    };
    let mut text = format!("{connection} | {activity} | event {}", state.watermark);
    if let Some(diagnostic) = state.diagnostics().last() {
        text.push_str(" | ! ");
        text.push_str(&sanitize_inline(diagnostic));
    }
    let text = truncate_width(&text, usize::from(area.width));
    frame.render_widget(
        Paragraph::new(text).style(status_style(theme, state.resync_required)),
        area,
    );
}

fn draw_input(
    frame: &mut Frame<'_>,
    area: Rect,
    editor: &PromptEditor,
    theme: ResolvedTheme,
    secret_input: bool,
) {
    let text = editor_text(editor, secret_input);
    let title = if secret_input {
        "API key (stored in native keyring)"
    } else {
        "Prompt"
    };
    let inner_width = area.width.saturating_sub(2).max(1);
    let inner_height = area.height.saturating_sub(2).max(1);
    let (cursor_row, cursor_column) = editor_cursor(editor, secret_input, inner_width);
    let scroll = cursor_row.saturating_sub(inner_height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(title))
            .style(base_style(theme))
            .scroll((scroll, 0)),
        area,
    );
    frame.set_cursor_position((
        area.x
            .saturating_add(1)
            .saturating_add(cursor_column.min(inner_width.saturating_sub(1))),
        area.y
            .saturating_add(1)
            .saturating_add(cursor_row.saturating_sub(scroll)),
    ));
}

fn editor_text(editor: &PromptEditor, secret_input: bool) -> Text<'static> {
    let selection = editor.selection();
    let mut lines = vec![Vec::<Span<'static>>::new()];
    let mut rendered = 0usize;
    for (index, grapheme) in editor.text().graphemes(true).enumerate() {
        if rendered >= MAX_RENDER_CHARS {
            if let Some(line) = lines.last_mut() {
                line.push(Span::raw("…"));
            }
            break;
        }
        if grapheme == "\n" {
            lines.push(Vec::new());
            rendered = rendered.saturating_add(1);
            continue;
        }
        let visible = if secret_input {
            "•".to_owned()
        } else {
            sanitize_inline(grapheme)
        };
        rendered = rendered.saturating_add(visible.chars().count());
        let selected = selection
            .as_ref()
            .is_some_and(|range| range.contains(&index));
        let span = if selected {
            Span::styled(visible, Style::default().add_modifier(Modifier::REVERSED))
        } else {
            Span::raw(visible)
        };
        if let Some(line) = lines.last_mut() {
            line.push(span);
        }
    }
    Text::from(lines.into_iter().map(Line::from).collect::<Vec<_>>())
}

fn editor_cursor(editor: &PromptEditor, secret_input: bool, width: u16) -> (u16, u16) {
    let width = usize::from(width.max(1));
    let mut row = 0usize;
    let mut column = 0usize;
    for grapheme in editor.text().graphemes(true).take(editor.cursor()) {
        if grapheme == "\n" {
            row = row.saturating_add(1);
            column = 0;
            continue;
        }
        let visible = if secret_input {
            "•".to_owned()
        } else {
            sanitize_inline(grapheme)
        };
        let grapheme_width = UnicodeWidthStr::width(visible.as_str()).max(1);
        if column.saturating_add(grapheme_width) > width {
            row = row.saturating_add(1);
            column = 0;
        }
        column = column.saturating_add(grapheme_width);
        if column >= width {
            row = row.saturating_add(column / width);
            column %= width;
        }
    }
    (
        u16::try_from(row).unwrap_or(u16::MAX),
        u16::try_from(column).unwrap_or(u16::MAX),
    )
}

fn semantic_lines(entry: &TranscriptEntry, width: u16, theme: ResolvedTheme) -> Vec<Line<'static>> {
    let (kind, style) = match entry.kind {
        TranscriptKind::UserMessage => ("USER", user_style(theme)),
        TranscriptKind::AssistantMessage => ("ASSISTANT", assistant_style(theme)),
        TranscriptKind::Reasoning => ("REASONING", muted_style(theme)),
        TranscriptKind::Effect => ("TOOL", effect_style(theme)),
        TranscriptKind::Callback => ("ACTION REQUIRED", warning_style(theme)),
        TranscriptKind::Checkpoint => ("CHECKPOINT", muted_style(theme)),
        TranscriptKind::Notice => ("NOTICE", notice_style(theme)),
        TranscriptKind::Plan => ("PLAN", assistant_style(theme)),
    };
    let status = match entry.status {
        EntryStatus::Streaming => "streaming",
        EntryStatus::Completed => "complete",
        EntryStatus::Failed => "failed",
        EntryStatus::Cancelled => "cancelled",
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("[{kind}]"), style),
        Span::raw(format!(" ({status})")),
    ])];
    let sanitized = sanitize_terminal(&entry.text, RenderLimits::default());
    let max_width = usize::from(width.max(1));
    for raw_line in sanitized
        .lines()
        .take(MAX_RENDER_LINES.saturating_sub(lines.len()))
    {
        lines.push(Line::raw(truncate_width(raw_line, max_width)));
    }
    if lines.len() < MAX_RENDER_LINES
        && let Some(duration) = entry
            .details
            .get("durationMs")
            .and_then(|value| value.as_u64())
    {
        lines.push(Line::raw(format!("duration: {duration} ms")));
    }
    if lines.len() < MAX_RENDER_LINES
        && let Some(kind) = entry
            .details
            .get("presentationKind")
            .and_then(|value| value.as_str())
    {
        lines.push(Line::raw(truncate_width(
            &format!("presentation: {}", sanitize_inline(kind)),
            max_width,
        )));
    }
    if lines.len() < MAX_RENDER_LINES
        && let Some(error) = entry.details.get("error").and_then(|value| value.as_str())
    {
        lines.push(Line::styled(
            truncate_width(&format!("error: {}", sanitize_inline(error)), max_width),
            error_style(theme),
        ));
    }
    if let Some(diff) = entry.details.get("diff").and_then(|value| value.as_array()) {
        for line in diff
            .iter()
            .filter_map(|value| value.as_str())
            .take(MAX_RENDER_LINES.saturating_sub(lines.len()))
        {
            let sanitized = truncate_width(&sanitize_inline(line), max_width);
            let style = if sanitized.starts_with('+') {
                success_style(theme)
            } else if sanitized.starts_with('-') {
                error_style(theme)
            } else {
                muted_style(theme)
            };
            lines.push(Line::styled(sanitized, style));
        }
    }
    lines
}

#[must_use]
pub fn sanitize_terminal(input: &str, limits: RenderLimits) -> String {
    let mut output = String::new();
    let mut total = 0usize;
    let mut line = 0usize;
    let mut truncated = false;
    for character in input.chars() {
        if total >= limits.max_chars {
            truncated = true;
            break;
        }
        if character == '\n' {
            output.push('\n');
            total += 1;
            line = 0;
            continue;
        }
        if line >= limits.max_line_chars {
            if !output.ends_with('…') {
                output.push('…');
                total += 1;
            }
            continue;
        }
        match character {
            '\t' => {
                output.push_str("    ");
                total = total.saturating_add(4);
                line = line.saturating_add(4);
            }
            '\u{1b}' => {
                output.push('␛');
                total += 1;
                line += 1;
            }
            character if character.is_control() => {
                output.push('�');
                total += 1;
                line += 1;
            }
            character => {
                output.push(character);
                total += 1;
                line += 1;
            }
        }
    }
    if truncated {
        output.push_str("\n… content truncated …");
    }
    output
}

fn sanitize_inline(input: &str) -> String {
    sanitize_terminal(
        input,
        RenderLimits {
            max_chars: MAX_RENDER_LINE_CHARS,
            max_line_chars: MAX_RENDER_LINE_CHARS,
        },
    )
    .replace('\n', " ")
}

fn truncate_width(input: &str, width: usize) -> String {
    if UnicodeWidthStr::width(input) <= width {
        return input.to_owned();
    }
    let mut output = String::new();
    for grapheme in input.graphemes(true) {
        let next = UnicodeWidthStr::width(output.as_str())
            .saturating_add(UnicodeWidthStr::width(grapheme));
        if next.saturating_add(1) > width {
            break;
        }
        output.push_str(grapheme);
    }
    if width > 0 {
        output.push('…');
    }
    output
}

fn base_style(theme: ResolvedTheme) -> Style {
    if !theme.colors_enabled {
        return Style::default();
    }
    match theme.theme {
        Theme::Light => Style::default().fg(Color::Black).bg(Color::White),
        Theme::Dark | Theme::System => Style::default().fg(Color::White).bg(Color::Black),
    }
}

fn user_style(theme: ResolvedTheme) -> Style {
    colored(theme, Color::Cyan).add_modifier(Modifier::BOLD)
}

fn assistant_style(theme: ResolvedTheme) -> Style {
    colored(theme, Color::Green).add_modifier(Modifier::BOLD)
}

fn effect_style(theme: ResolvedTheme) -> Style {
    colored(theme, Color::Magenta).add_modifier(Modifier::BOLD)
}

fn warning_style(theme: ResolvedTheme) -> Style {
    colored(theme, Color::Yellow).add_modifier(Modifier::BOLD)
}

fn notice_style(theme: ResolvedTheme) -> Style {
    colored(theme, Color::Blue).add_modifier(Modifier::BOLD)
}

fn success_style(theme: ResolvedTheme) -> Style {
    colored(theme, Color::Green)
}

fn error_style(theme: ResolvedTheme) -> Style {
    colored(theme, Color::Red)
}

fn muted_style(theme: ResolvedTheme) -> Style {
    colored(theme, Color::DarkGray)
}

fn status_style(theme: ResolvedTheme, warning: bool) -> Style {
    if warning {
        warning_style(theme)
    } else {
        muted_style(theme)
    }
}

fn colored(theme: ResolvedTheme, color: Color) -> Style {
    if theme.colors_enabled {
        Style::default().fg(color)
    } else {
        Style::default()
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    use super::*;
    use crate::tui::state::{EntryStatus, TranscriptEntry, TranscriptKind};

    fn theme(colors_enabled: bool) -> ResolvedTheme {
        ResolvedTheme {
            theme: Theme::Dark,
            colors_enabled,
        }
    }

    #[test]
    fn hostile_content_never_reaches_the_terminal_as_control_sequences() {
        let rendered = sanitize_terminal(
            "\u{1b}]52;c;secret\u{7}\nline\u{0}",
            RenderLimits::default(),
        );
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
        assert!(!rendered.contains('\u{0}'));
        assert!(rendered.contains('␛'));
    }

    #[test]
    fn huge_and_narrow_content_is_bounded_without_splitting_graphemes() {
        let input = format!("{}e\u{301}", "x".repeat(MAX_RENDER_CHARS + 100));
        let rendered = sanitize_terminal(&input, RenderLimits::default());
        assert!(rendered.chars().count() < MAX_RENDER_CHARS + 100);
        assert_eq!(truncate_width("abc e\u{301}z", 5), "abc …");
    }

    #[test]
    fn test_backend_snapshot_distinguishes_status_diff_duration_and_errors() {
        let backend = TestBackend::new(52, 16);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut state = TuiState::new("session");
        state.ready = true;
        state.entries.push(TranscriptEntry {
            id: "effect".to_owned(),
            revision: 1,
            kind: TranscriptKind::Effect,
            text: "Updated src/lib.rs".to_owned(),
            status: EntryStatus::Failed,
            details: json!({
                "presentationKind": "diff",
                "durationMs": 17,
                "error": "permission denied",
                "diff": [" context", "-old", "+new"],
            }),
        });
        let editor = PromptEditor::default();
        terminal
            .draw(|frame| draw(frame, &mut state, &editor, theme(false), false))
            .expect("snapshot renders");
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("[TOOL] (failed)"));
        assert!(text.contains("duration: 17 ms"));
        assert!(text.contains("error: permission denied"));
        assert!(text.contains("-old"));
        assert!(text.contains("+new"));
    }

    #[test]
    fn no_color_snapshot_still_exposes_semantic_labels() {
        let entry = TranscriptEntry {
            id: "notice".to_owned(),
            revision: 1,
            kind: TranscriptKind::Notice,
            text: "scheduled loop fired".to_owned(),
            status: EntryStatus::Completed,
            details: serde_json::Value::Null,
        };
        let lines = semantic_lines(&entry, 40, theme(false));
        assert_eq!(lines[0].to_string(), "[NOTICE] (complete)");
        assert_eq!(lines[1].to_string(), "scheduled loop fired");
    }

    #[test]
    fn latest_diagnostic_is_visible_without_leaving_the_prompt() {
        let backend = TestBackend::new(80, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut state = TuiState::new("session");
        state.push_diagnostic("Keyring unavailable; run /setup after repairing access");
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &PromptEditor::default(),
                    theme(false),
                    false,
                );
            })
            .expect("diagnostic renders");
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Keyring unavailable"));
    }

    #[test]
    fn prompt_snapshot_keeps_selection_and_cursor_visible() {
        let backend = TestBackend::new(30, 9);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut state = TuiState::new("session");
        let mut editor = PromptEditor::default();
        editor.set_text("aβc");
        editor.select(1..2);

        terminal
            .draw(|frame| draw(frame, &mut state, &editor, theme(false), false))
            .expect("prompt renders");

        let input_y = 5;
        let selected = terminal
            .backend()
            .buffer()
            .cell((2, input_y))
            .expect("selected cell");
        assert_eq!(selected.symbol(), "β");
        assert!(selected.modifier.contains(Modifier::REVERSED));
        assert_eq!(
            terminal
                .get_cursor_position()
                .expect("rendered cursor position"),
            ratatui::layout::Position::new(3, input_y)
        );
    }

    #[test]
    fn tall_entry_scrolls_by_semantic_lines() {
        let backend = TestBackend::new(32, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut state = TuiState::new("session");
        state.entries.push(TranscriptEntry {
            id: "tall".to_owned(),
            revision: 1,
            kind: TranscriptKind::AssistantMessage,
            text: [
                "line-one",
                "line-two",
                "line-three",
                "line-four",
                "line-five",
                "line-six",
            ]
            .join("\n"),
            status: EntryStatus::Completed,
            details: serde_json::Value::Null,
        });

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &PromptEditor::default(),
                    theme(false),
                    false,
                );
            })
            .expect("latest semantic lines render");
        let latest = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!latest.contains("line-one"));
        assert!(latest.contains("line-six"));

        assert!(state.scroll_up(2));
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &PromptEditor::default(),
                    theme(false),
                    false,
                );
            })
            .expect("older semantic lines render");
        let scrolled = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(scrolled.contains("line-one"));
        assert!(!scrolled.contains("line-six"));
    }
}

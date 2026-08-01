use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

mod markdown;

use super::chat_input::InputMode;
use super::input::{
    CompletionEngine, PromptEditor, completion_description, composer_content_width,
    visual_cursor_cell, visual_lines,
};
use super::setup::{ResolvedTheme, Theme};
use super::state::{EntryStatus, TranscriptEntry, TranscriptKind, TuiState};
use markdown::markdown_lines;

pub const MAX_RENDER_CHARS: usize = 64 * 1024;
pub const MAX_RENDER_LINE_CHARS: usize = 4 * 1024;
const MAX_RENDER_LINES: usize = 4 * 1024;
const COMPOSER_MIN_BODY_HEIGHT: u16 = 3;
const COMPOSER_CHROME_HEIGHT: u16 = 2;
const COMPOSER_PROMPT_WIDTH: u16 = 2;
const COMPLETION_MAX_HEIGHT: u16 = 12;
const MISTRAL_ORANGE: Color = Color::Rgb(255, 130, 5);
const PETIT_CHAT: [&str; 3] = ["  ⡠⣒⠄  ⡔⢄⠔⡄", " ⢸⠸⣀⡔⢉⠱⣃⡢⣂⡣", "  ⠉⠒⠣⠤⠵⠤⠬⠮⠆"];

#[derive(Debug, Clone, Copy)]
pub struct BannerContext<'a> {
    pub version: &'a str,
    pub model: &'a str,
    pub thinking: &'a str,
    pub models_count: usize,
    pub skills_count: usize,
    pub mcp_servers_enabled: usize,
    pub mcp_servers_total: usize,
    pub connectors_connected: usize,
    pub connectors_total: usize,
    pub hooks_count: usize,
    pub plan: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenState {
    pub max_tokens: u64,
    pub current_tokens: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct UiContext<'a> {
    pub cwd: &'a Path,
    pub agent_name: &'a str,
    pub secret_input: bool,
    pub banner: BannerContext<'a>,
    pub tokens: TokenState,
}

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
    completion: &CompletionEngine,
    input_mode: InputMode,
    theme: ResolvedTheme,
    context: UiContext<'_>,
) {
    let requested_completion_height = completion.view().map_or(0, |view| {
        u16::try_from(view.candidates.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(COMPLETION_MAX_HEIGHT)
    });
    let activity_height = u16::from(activity_text(state).is_some());
    let input_height = composer_height(editor, frame.area(), context.secret_input, input_mode);
    let completion_height = requested_completion_height.min(
        frame
            .area()
            .height
            .saturating_sub(activity_height)
            .saturating_sub(input_height)
            .saturating_sub(2),
    );
    let [
        transcript_area,
        activity_area,
        completion_area,
        input_area,
        footer_area,
    ] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(activity_height),
        Constraint::Length(completion_height),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_transcript(frame, transcript_area, state, theme, context.banner);
    draw_activity(frame, activity_area, state, theme);
    draw_completion(frame, completion_area, completion, theme);
    draw_input(frame, input_area, editor, input_mode, theme, context);
    draw_footer(frame, footer_area, context.cwd, context.tokens, theme);
    if let Some(overlay) = &state.overlay {
        draw_overlay(frame, overlay, theme);
    }
}

fn draw_overlay(
    frame: &mut Frame<'_>,
    overlay: &super::interaction::Overlay,
    theme: ResolvedTheme,
) {
    let outer = frame.area();
    let width = outer.width.saturating_sub(4).clamp(1, 86);
    let visible = overlay.visible_items();
    let content_height = u16::try_from(visible.len())
        .unwrap_or(u16::MAX)
        .saturating_add(5);
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
            Span::styled("Filter: ", muted_style(theme)),
            Span::raw(truncate_width(
                &overlay.query,
                inner_width.saturating_sub(8),
            )),
        ]));
        lines.push(Line::default());
    }
    for (selected, item) in visible {
        let marker = if selected { "▸ " } else { "  " };
        let style = if item.disabled {
            muted_style(theme)
        } else if selected {
            secondary_style(theme).add_modifier(Modifier::BOLD)
        } else {
            base_style(theme)
        };
        let description_width = inner_width.saturating_sub(item.label.width().saturating_add(5));
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(truncate_width(&item.label, inner_width), style),
            Span::raw("  "),
            Span::styled(
                truncate_width(&item.description, description_width),
                muted_style(theme),
            ),
        ]));
    }
    if let Some(notice) = &overlay.notice {
        lines.push(Line::default());
        lines.push(Line::styled(
            truncate_width(notice, inner_width),
            warning_style(theme),
        ));
    }
    lines.push(Line::default());
    let help = match overlay.kind {
        super::interaction::OverlayKind::Config => {
            "↑↓/jk Navigate  Enter Edit  Ctrl+R Reset  Esc Close"
        }
        super::interaction::OverlayKind::Sessions => {
            "↑↓/jk Navigate  Enter Resume  Delete Remove  Esc Close"
        }
        super::interaction::OverlayKind::Mcp | super::interaction::OverlayKind::Connectors => {
            "↑↓/jk Navigate  Enter Toggle  Ctrl+R Refresh  Esc Close"
        }
        _ => "↑↓/jk Navigate  Enter Select  Esc Close",
    };
    lines.push(Line::styled(help, muted_style(theme)));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", overlay.title))
                    .padding(Padding::horizontal(1))
                    .style(base_style(theme)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn composer_height(
    editor: &PromptEditor,
    area: Rect,
    secret_input: bool,
    input_mode: InputMode,
) -> u16 {
    let input_width = composer_content_width(area.width);
    let body_height = editor_visual_height(editor, secret_input, input_mode, input_width)
        .max(COMPOSER_MIN_BODY_HEIGHT)
        .min(area.height.saturating_div(2).max(COMPOSER_MIN_BODY_HEIGHT));
    body_height.saturating_add(COMPOSER_CHROME_HEIGHT)
}

fn editor_visual_height(
    editor: &PromptEditor,
    _secret_input: bool,
    input_mode: InputMode,
    width: usize,
) -> u16 {
    let rows = visual_lines(editor.text(), width.max(1), input_mode.prefix_len()).len();
    u16::try_from(rows).unwrap_or(u16::MAX)
}

fn draw_transcript(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut TuiState,
    theme: ResolvedTheme,
    banner: BannerContext<'_>,
) {
    let visible_height = usize::from(area.height).max(1);
    let mut newest_first = Vec::new();
    let mut rendered_chars = 0usize;
    let mut history_truncated = false;
    'entries: for entry in state.entries.iter().rev() {
        if entry.kind == TranscriptKind::Reasoning && !state.show_reasoning {
            continue;
        }
        let entry_lines = if entry.kind == TranscriptKind::Effect && state.tools_collapsed {
            collapsed_effect_lines(entry, area.width, theme)
        } else {
            semantic_lines(entry, area.width, theme)
        };
        for line in entry_lines.into_iter().rev() {
            let line_chars = line.to_string().chars().count();
            if newest_first.len() >= MAX_RENDER_LINES
                || rendered_chars.saturating_add(line_chars) > MAX_RENDER_CHARS
            {
                history_truncated = true;
                break 'entries;
            }
            rendered_chars = rendered_chars.saturating_add(line_chars);
            newest_first.push(line);
        }
    }
    if !history_truncated {
        for line in banner_lines(banner, theme).into_iter().rev() {
            let line_chars = line.to_string().chars().count();
            if newest_first.len() >= MAX_RENDER_LINES
                || rendered_chars.saturating_add(line_chars) > MAX_RENDER_CHARS
            {
                break;
            }
            rendered_chars = rendered_chars.saturating_add(line_chars);
            newest_first.push(line);
        }
    }
    state.set_scroll_line_limit(newest_first.len().saturating_sub(visible_height));
    let mut lines = newest_first
        .into_iter()
        .skip(state.scroll_offset)
        .take(visible_height)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    if lines.len() < visible_height {
        lines.splice(
            0..0,
            std::iter::repeat_with(Line::default).take(visible_height - lines.len()),
        );
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn banner_lines(banner: BannerContext<'_>, theme: ResolvedTheme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    lines.extend(
        PETIT_CHAT
            .into_iter()
            .map(|line| Line::styled(line, base_style(theme))),
    );
    lines.push(Line::default());
    let mut identity = vec![
        Span::styled(
            "Mistral Vibe",
            orange_style(theme).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("v{} · ", sanitize_inline(banner.version)),
            base_style(theme),
        ),
        Span::styled(
            format!(
                "{}[{}]",
                sanitize_inline(banner.model),
                sanitize_inline(banner.thinking)
            ),
            secondary_style(theme),
        ),
    ];
    if let Some(plan) = banner.plan {
        identity.push(Span::styled(
            format!(" · {}", sanitize_inline(plan)),
            base_style(theme),
        ));
    }
    lines.push(Line::from(identity));
    lines.push(Line::styled(banner_counts(banner), base_style(theme)));
    lines.push(Line::from(vec![
        Span::styled("Type ", base_style(theme)),
        Span::styled("/help", secondary_style(theme)),
        Span::styled(" for more information", base_style(theme)),
    ]));
    lines.push(Line::default());
    lines
}

fn banner_counts(banner: BannerContext<'_>) -> String {
    let mut parts = vec![pluralized(banner.models_count, "model")];
    if banner.connectors_total > 0 {
        parts.push(if banner.connectors_connected == banner.connectors_total {
            pluralized(banner.connectors_connected, "connector")
        } else {
            format!(
                "{}/{} connector{}",
                banner.connectors_connected,
                banner.connectors_total,
                if banner.connectors_total == 1 {
                    ""
                } else {
                    "s"
                }
            )
        });
    }
    parts.push(if banner.mcp_servers_enabled == banner.mcp_servers_total {
        pluralized(banner.mcp_servers_enabled, "MCP server")
    } else {
        format!(
            "{}/{} MCP server{}",
            banner.mcp_servers_enabled,
            banner.mcp_servers_total,
            if banner.mcp_servers_total == 1 {
                ""
            } else {
                "s"
            }
        )
    });
    parts.push(pluralized(banner.skills_count, "skill"));
    if banner.hooks_count > 0 {
        parts.push(pluralized(banner.hooks_count, "hook"));
    }
    parts.join(" · ")
}

fn pluralized(count: usize, singular: &str) -> String {
    format!("{count} {singular}{}", if count == 1 { "" } else { "s" })
}

fn activity_text(state: &TuiState) -> Option<(String, bool)> {
    if let Some(diagnostic) = state.diagnostics().last() {
        return Some((format!("! {}", sanitize_inline(diagnostic)), true));
    }
    if !state.connected || state.resync_required {
        return Some(("! Connection lost".to_owned(), true));
    }
    state.waiting.then(|| {
        let queued = state.prompt_queue.len();
        (
            if queued == 0 {
                "⠋ Working…".to_owned()
            } else {
                format!("⠋ Working… · {queued} queued")
            },
            false,
        )
    })
}

fn draw_activity(frame: &mut Frame<'_>, area: Rect, state: &TuiState, theme: ResolvedTheme) {
    let Some((text, warning)) = activity_text(state) else {
        return;
    };
    frame.render_widget(
        Paragraph::new(truncate_width(&text, usize::from(area.width))).style(if warning {
            warning_style(theme)
        } else {
            orange_style(theme)
        }),
        area,
    );
}

fn draw_completion(
    frame: &mut Frame<'_>,
    area: Rect,
    completion: &CompletionEngine,
    theme: ResolvedTheme,
) {
    let Some(view) = completion.view() else {
        return;
    };
    let label_width = view
        .candidates
        .iter()
        .map(|candidate| UnicodeWidthStr::width(candidate.label.trim_start_matches('@')))
        .max()
        .unwrap_or_default()
        .min(usize::from(area.width.saturating_mul(3) / 10));
    let visible_rows = usize::from(area.height.saturating_sub(2)).max(1);
    let first_visible = view.selected.saturating_add(1).saturating_sub(visible_rows);
    let lines = view
        .candidates
        .iter()
        .enumerate()
        .skip(first_visible)
        .take(visible_rows)
        .map(|(index, candidate)| {
            let label = candidate.label.trim_start_matches('@');
            let description = completion_description(&candidate.label);
            let selected = index == view.selected;
            let command_style = if selected {
                base_style(theme)
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::REVERSED)
            } else {
                base_style(theme).add_modifier(Modifier::BOLD)
            };
            let description_style = if selected {
                base_style(theme).add_modifier(Modifier::ITALIC)
            } else {
                muted_style(theme)
            };
            if description.is_empty() {
                Line::from(Span::styled(label.to_owned(), command_style))
            } else {
                Line::from(vec![
                    Span::styled(format!("{label:label_width$}"), command_style),
                    Span::raw("  "),
                    Span::styled(description, description_style),
                ])
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .padding(Padding::horizontal(1))
                .border_style(muted_style(theme)),
        ),
        area,
    );
}

fn draw_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    cwd: &Path,
    tokens: TokenState,
    theme: ResolvedTheme,
) {
    let path = display_path(cwd);
    let context = format_context_progress(tokens);
    let context_width = u16::try_from(UnicodeWidthStr::width(context.as_str()))
        .unwrap_or(u16::MAX)
        .min(area.width);
    let [path_area, context_area] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(context_width)]).areas(area);
    frame.render_widget(
        Paragraph::new(truncate_width(&path, usize::from(path_area.width)))
            .style(muted_style(theme)),
        path_area,
    );
    frame.render_widget(
        Paragraph::new(context)
            .alignment(Alignment::Right)
            .style(muted_style(theme)),
        context_area,
    );
}

fn format_context_progress(tokens: TokenState) -> String {
    if tokens.max_tokens == 0 {
        return String::new();
    }
    let ratio = (tokens.current_tokens as f64 / tokens.max_tokens as f64).min(1.0);
    format!(
        "{}/{} tokens ({:.0}%)",
        format_token_count(tokens.current_tokens),
        format_token_count(tokens.max_tokens),
        ratio * 100.0
    )
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        return format!("{:.1}M", tokens as f64 / 1_000_000.0);
    }
    if tokens >= 1_000 {
        return format!("{}k", tokens / 1_000);
    }
    tokens.to_string()
}

fn display_path(path: &Path) -> String {
    let Some(home) = user_home_directory() else {
        return path.to_string_lossy().into_owned();
    };
    match path.strip_prefix(&home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".to_owned(),
        Ok(relative) => Path::new("~").join(relative).to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
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

fn draw_input(
    frame: &mut Frame<'_>,
    area: Rect,
    editor: &PromptEditor,
    input_mode: InputMode,
    theme: ResolvedTheme,
    context: UiContext<'_>,
) {
    let input_width = composer_content_width(area.width);
    let text = editor_text(editor, context.secret_input, input_mode, input_width);
    let title = if context.secret_input {
        " API key (stored in native keyring) "
    } else if context.agent_name.is_empty() {
        ""
    } else {
        context.agent_name
    };
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(muted_style(theme))
        .title_style(muted_style(theme))
        .title_alignment(Alignment::Right)
        .title(title);
    frame.render_widget(block, area);
    let body = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(COMPOSER_CHROME_HEIGHT),
    );
    let [prompt_area, input_area, _trailing_area] = Layout::horizontal([
        Constraint::Length(COMPOSER_PROMPT_WIDTH),
        Constraint::Length(u16::try_from(input_width).unwrap_or(u16::MAX)),
        Constraint::Min(0),
    ])
    .areas(body);
    let (cursor_row, cursor_column) = editor_cursor(editor, input_mode, input_width);
    let scroll = cursor_row.saturating_sub(input_area.height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(input_mode.symbol().to_string()).style(orange_style(theme)),
        prompt_area,
    );
    frame.render_widget(
        Paragraph::new(text)
            .style(base_style(theme))
            .scroll((scroll, 0)),
        input_area,
    );
    frame.set_cursor_position((
        input_area
            .x
            .saturating_add(cursor_column.min(input_area.width.saturating_sub(1))),
        input_area
            .y
            .saturating_add(cursor_row.saturating_sub(scroll)),
    ));
}

/// Maps an absolute terminal coordinate into the scrolled editor body.
/// Coordinates outside the visible composer return `None` without inventing
/// a nearest selection target.
pub(crate) fn editor_mouse_cell(
    editor: &PromptEditor,
    screen: Rect,
    secret_input: bool,
    input_mode: InputMode,
    x: u16,
    y: u16,
) -> Option<(usize, usize, usize)> {
    let input_height = composer_height(editor, screen, secret_input, input_mode);
    let input_y = screen
        .y
        .saturating_add(screen.height)
        .saturating_sub(1)
        .saturating_sub(input_height);
    let body_y = input_y.saturating_add(1);
    let body_height = input_height.saturating_sub(COMPOSER_CHROME_HEIGHT);
    let input_x = screen.x.saturating_add(COMPOSER_PROMPT_WIDTH);
    let input_width = u16::try_from(composer_content_width(screen.width)).unwrap_or(u16::MAX);
    if x < input_x
        || x >= input_x.saturating_add(input_width)
        || y < body_y
        || y >= body_y.saturating_add(body_height)
    {
        return None;
    }
    let (cursor_row, _) = editor_cursor(editor, input_mode, usize::from(input_width));
    let scroll = cursor_row.saturating_sub(body_height.saturating_sub(1));
    Some((
        usize::from(y.saturating_sub(body_y).saturating_add(scroll)),
        usize::from(x.saturating_sub(input_x)),
        usize::from(input_width),
    ))
}

fn editor_text(
    editor: &PromptEditor,
    secret_input: bool,
    input_mode: InputMode,
    width: usize,
) -> Text<'static> {
    let prefix = input_mode.prefix_len();
    let selection = editor.selection();
    let graphemes = editor.text().graphemes(true).collect::<Vec<_>>();
    let ranges = visual_lines(editor.text(), width.max(1), prefix);
    let mut lines = Vec::with_capacity(ranges.len());
    let mut rendered = 0usize;
    for range in ranges {
        let mut line = Vec::new();
        for index in range {
            if rendered >= MAX_RENDER_CHARS {
                line.push(Span::raw("…"));
                break;
            }
            let grapheme = graphemes[index];
            let visible = if secret_input {
                "•".to_owned()
            } else {
                sanitize_inline(grapheme)
            };
            rendered = rendered.saturating_add(visible.chars().count());
            let selected = selection
                .as_ref()
                .is_some_and(|selection| selection.contains(&index));
            line.push(if selected {
                Span::styled(visible, Style::default().add_modifier(Modifier::REVERSED))
            } else {
                Span::raw(visible)
            });
        }
        lines.push(line);
        if rendered >= MAX_RENDER_CHARS {
            break;
        }
    }
    Text::from(lines.into_iter().map(Line::from).collect::<Vec<_>>())
}

fn editor_cursor(editor: &PromptEditor, input_mode: InputMode, width: usize) -> (u16, u16) {
    let (row, column) = visual_cursor_cell(
        editor.text(),
        editor.cursor(),
        width,
        input_mode.prefix_len(),
    );
    (
        u16::try_from(row).unwrap_or(u16::MAX),
        u16::try_from(column).unwrap_or(u16::MAX),
    )
}

fn semantic_lines(entry: &TranscriptEntry, width: u16, theme: ResolvedTheme) -> Vec<Line<'static>> {
    if entry.kind == TranscriptKind::UserMessage {
        return user_message_lines(entry, width, theme);
    }
    if entry.kind == TranscriptKind::AssistantMessage {
        return assistant_message_lines(entry, width, theme);
    }
    if entry.kind == TranscriptKind::Notice {
        return notice_message_lines(entry, width, theme);
    }
    let (kind, style) = match entry.kind {
        TranscriptKind::UserMessage | TranscriptKind::AssistantMessage => {
            ("MESSAGE", base_style(theme))
        }
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

fn notice_message_lines(
    entry: &TranscriptEntry,
    width: u16,
    theme: ResolvedTheme,
) -> Vec<Line<'static>> {
    let content_width = usize::from(width.saturating_sub(4).max(1));
    let wrapped = wrapped_terminal_lines(&entry.text, content_width);
    let last = wrapped.len().saturating_sub(1);
    let mut lines = wrapped
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            Line::from(vec![
                Span::styled(
                    if index == last { "  ⎣ " } else { "  ⎢ " },
                    muted_style(theme),
                ),
                Span::styled(line, base_style(theme)),
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
    let prompt_style = orange_style(theme)
        .add_modifier(Modifier::BOLD)
        .add_modifier(if pending {
            Modifier::ITALIC
        } else {
            Modifier::empty()
        });
    let content_style = if prompt == '/' {
        orange_style(theme).add_modifier(Modifier::BOLD)
    } else {
        base_style(theme).add_modifier(Modifier::BOLD)
    }
    .add_modifier(if pending {
        Modifier::ITALIC
    } else {
        Modifier::empty()
    });
    let max_width = usize::from(width.saturating_sub(COMPOSER_PROMPT_WIDTH).max(1));
    let mut lines = vec![Line::default()];
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
        lines.push(Line::styled(
            "─".repeat(usize::from(width)),
            muted_style(theme),
        ));
    }
    lines
}

fn assistant_message_lines(
    entry: &TranscriptEntry,
    width: u16,
    theme: ResolvedTheme,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    lines.extend(
        markdown_lines(&entry.text, usize::from(width), theme)
            .into_iter()
            .take(MAX_RENDER_LINES.saturating_sub(lines.len())),
    );
    append_terminal_status(&mut lines, entry, theme);
    lines
}

fn collapsed_effect_lines(
    entry: &TranscriptEntry,
    width: u16,
    theme: ResolvedTheme,
) -> Vec<Line<'static>> {
    let status = match entry.status {
        EntryStatus::Streaming => "streaming",
        EntryStatus::Completed => "complete",
        EntryStatus::Failed => "failed",
        EntryStatus::Cancelled => "cancelled",
    };
    let title = entry.text.lines().next().unwrap_or("Tool call");
    vec![Line::from(vec![
        Span::styled("[TOOL]", effect_style(theme)),
        Span::raw(format!(" ({status}) ")),
        Span::styled(
            truncate_width(title, usize::from(width.saturating_sub(30).max(1))),
            base_style(theme),
        ),
        Span::styled(" · collapsed (Ctrl+O)", muted_style(theme)),
    ])]
}

fn append_terminal_status(
    lines: &mut Vec<Line<'static>>,
    entry: &TranscriptEntry,
    theme: ResolvedTheme,
) {
    let status = match entry.status {
        EntryStatus::Failed => entry
            .details
            .get("error")
            .and_then(|value| value.as_str())
            .map_or_else(|| "failed".to_owned(), |error| format!("failed: {error}")),
        EntryStatus::Cancelled => "cancelled".to_owned(),
        EntryStatus::Streaming | EntryStatus::Completed => return,
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("({})", sanitize_inline(&status)),
            error_style(theme),
        ),
    ]));
}

fn wrapped_terminal_lines(input: &str, width: usize) -> Vec<String> {
    let sanitized = sanitize_terminal(input, RenderLimits::default());
    let mut wrapped = Vec::new();
    for logical_line in sanitized.split('\n') {
        wrap_visual_line(logical_line, width.max(1), &mut wrapped);
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

fn wrap_visual_line(line: &str, width: usize, output: &mut Vec<String>) {
    if line.is_empty() {
        output.push(String::new());
        return;
    }
    let mut current = String::new();
    let mut current_width = 0usize;
    for grapheme in line.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if current_width > 0 && current_width.saturating_add(grapheme_width) > width {
            output.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push_str(grapheme);
        current_width = current_width.saturating_add(grapheme_width);
    }
    if !current.is_empty() {
        output.push(current);
    }
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
        Theme::Light => Style::default().fg(Color::Black),
        Theme::Dark | Theme::System => Style::default().fg(Color::White),
    }
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

fn orange_style(theme: ResolvedTheme) -> Style {
    colored(theme, MISTRAL_ORANGE).add_modifier(Modifier::BOLD)
}

fn secondary_style(theme: ResolvedTheme) -> Style {
    colored(theme, Color::Cyan)
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

    fn test_context(secret_input: bool) -> UiContext<'static> {
        UiContext {
            cwd: Path::new("/workspace"),
            agent_name: " default ",
            secret_input,
            banner: BannerContext {
                version: env!("CARGO_PKG_VERSION"),
                model: "mistral-medium-3.5",
                thinking: "high",
                models_count: 3,
                skills_count: 53,
                mcp_servers_enabled: 0,
                mcp_servers_total: 0,
                connectors_connected: 0,
                connectors_total: 17,
                hooks_count: 0,
                plan: Some("Free"),
            },
            tokens: TokenState {
                max_tokens: 200_000,
                current_tokens: 0,
            },
        }
    }

    fn draw_test(
        frame: &mut Frame<'_>,
        state: &mut TuiState,
        editor: &PromptEditor,
        secret_input: bool,
    ) {
        draw(
            frame,
            state,
            editor,
            &CompletionEngine::default(),
            InputMode::Prompt,
            theme(false),
            test_context(secret_input),
        );
    }

    fn draw_test_mode(
        frame: &mut Frame<'_>,
        state: &mut TuiState,
        editor: &PromptEditor,
        mode: InputMode,
    ) {
        draw(
            frame,
            state,
            editor,
            &CompletionEngine::default(),
            mode,
            theme(false),
            test_context(false),
        );
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
            .draw(|frame| draw_test(frame, &mut state, &editor, false))
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
    fn notices_follow_the_upstream_user_command_message_contract() {
        let entry = TranscriptEntry {
            id: "notice".to_owned(),
            revision: 1,
            kind: TranscriptKind::Notice,
            text: "scheduled loop fired".to_owned(),
            status: EntryStatus::Completed,
            details: serde_json::Value::Null,
        };
        let lines = semantic_lines(&entry, 40, theme(false));
        assert_eq!(lines[0].to_string(), "  ⎣ scheduled loop fired");
        assert!(!lines[0].to_string().contains("[NOTICE]"));
    }

    #[test]
    fn user_messages_follow_the_upstream_prompt_and_separator_contract() {
        let entry = TranscriptEntry {
            id: "user".to_owned(),
            revision: 1,
            kind: TranscriptKind::UserMessage,
            text: "/help".to_owned(),
            status: EntryStatus::Completed,
            details: serde_json::Value::Null,
        };
        let lines = semantic_lines(&entry, 24, theme(false));
        assert_eq!(lines[1].to_string(), "/ help");
        assert_eq!(lines.len(), 2);
        assert!(!lines.iter().any(|line| line.to_string().contains("[USER]")));

        let mut prompt = entry;
        prompt.text = "hello".to_owned();
        let lines = semantic_lines(&prompt, 24, theme(false));
        assert_eq!(lines[1].to_string(), "> hello");
        assert_eq!(lines[2].to_string(), "─".repeat(24));
    }

    #[test]
    fn messages_wrap_without_losing_status_or_content() {
        let entry = TranscriptEntry {
            id: "assistant".to_owned(),
            revision: 1,
            kind: TranscriptKind::AssistantMessage,
            text: "abcdefghijkl".to_owned(),
            status: EntryStatus::Failed,
            details: json!({"error": "network"}),
        };
        let lines = semantic_lines(&entry, 7, theme(false));
        assert_eq!(lines[1].to_string(), "  abcde");
        assert_eq!(lines[2].to_string(), "  fghij");
        assert_eq!(lines[3].to_string(), "  kl");
        assert_eq!(lines[4].to_string(), "  (failed: network)");
    }

    #[test]
    fn composer_uses_upstream_chrome_modes_completion_and_footer() {
        let backend = TestBackend::new(84, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut state = TuiState::new("session");
        let mut editor = PromptEditor::default();
        editor.set_text("/he");
        let mut completion = CompletionEngine::default();
        completion
            .refresh(&editor, Path::new("/workspace"))
            .expect("slash completion");

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &editor,
                    &completion,
                    InputMode::Command,
                    theme(true),
                    test_context(false),
                );
            })
            .expect("composer renders");

        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains(" default "));
        assert!(text.contains("Show help message"));
        assert!(text.contains("/workspace"));
        assert!(text.contains(&format!(
            "Mistral Vibe v{} · mistral-medium-3.5[high] · Free",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(text.contains("3 models · 0/17 connectors · 0 MCP servers · 53 skills"));
        assert!(text.contains("Type /help for more information"));
        assert!(text.contains("0/200k tokens (0%)"));
        assert!(!text.contains("Transcript"));
        assert!(!text.contains("Prompt"));
        assert!(!text.contains("connected |"));
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((2, 25))
                .expect("composer body cell")
                .bg,
            Color::Reset
        );
    }

    #[test]
    fn context_progress_uses_the_official_compact_format() {
        assert_eq!(
            format_context_progress(TokenState {
                max_tokens: 200_000,
                current_tokens: 12_345,
            }),
            "12k/200k tokens (6%)"
        );
        assert_eq!(
            format_context_progress(TokenState {
                max_tokens: 2_000_000,
                current_tokens: 2_500_000,
            }),
            "2.5M/2.0M tokens (100%)"
        );
    }

    #[test]
    fn composer_renders_all_supported_input_modes_as_prefix_chrome() {
        assert_eq!(InputMode::Prompt.symbol(), '>');
        assert_eq!(InputMode::Shell.symbol(), '!');
        assert_eq!(InputMode::Command.symbol(), '/');
        assert_eq!(InputMode::Teleport.symbol(), '&');
        assert_eq!(InputMode::Prompt.prefix_len(), 0);
        assert_eq!(InputMode::Teleport.prefix_len(), 1);
    }

    #[test]
    fn mode_prefix_and_unicode_cursor_render_at_fixed_reference_widths() {
        for width in [40, 80, 120] {
            let backend = TestBackend::new(width, 12);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let mut state = TuiState::new("session");
            let mut editor = PromptEditor::default();
            editor.set_text("/界");

            terminal
                .draw(|frame| {
                    draw_test_mode(frame, &mut state, &editor, InputMode::Command);
                })
                .expect("mode composer renders");

            let input_height = composer_height(
                &editor,
                Rect::new(0, 0, width, 12),
                false,
                InputMode::Command,
            );
            let body_y = 12_u16.saturating_sub(input_height);
            let buffer = terminal.backend().buffer();
            assert_eq!(
                buffer.cell((0, body_y)).map(|cell| cell.symbol()),
                Some("/")
            );
            assert_eq!(
                buffer.cell((2, body_y)).map(|cell| cell.symbol()),
                Some("界")
            );
            assert_eq!(
                terminal
                    .get_cursor_position()
                    .expect("rendered cursor position"),
                ratatui::layout::Position::new(4, body_y)
            );
        }
    }

    #[test]
    fn composer_mouse_coordinates_reject_chrome_and_map_visible_cells() {
        let mut editor = PromptEditor::default();
        editor.set_text("alpha\nbeta");
        let screen = Rect::new(0, 0, 40, 10);
        let body_y = 5;
        assert_eq!(
            editor_mouse_cell(&editor, screen, false, InputMode::Prompt, 4, body_y),
            Some((0, 2, 31))
        );
        assert_eq!(
            editor_mouse_cell(&editor, screen, false, InputMode::Prompt, 1, body_y),
            None
        );
        assert_eq!(
            editor_mouse_cell(&editor, screen, false, InputMode::Prompt, 4, 0),
            None
        );
    }

    #[test]
    fn latest_diagnostic_is_visible_without_leaving_the_prompt() {
        let backend = TestBackend::new(80, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut state = TuiState::new("session");
        state.push_diagnostic("Keyring unavailable; run /setup after repairing access");
        terminal
            .draw(|frame| draw_test(frame, &mut state, &PromptEditor::default(), false))
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
            .draw(|frame| draw_test(frame, &mut state, &editor, false))
            .expect("prompt renders");

        let input_y = 4;
        let selected = terminal
            .backend()
            .buffer()
            .cell((3, input_y))
            .expect("selected cell");
        assert_eq!(selected.symbol(), "β");
        assert!(selected.modifier.contains(Modifier::REVERSED));
        assert_eq!(
            terminal
                .get_cursor_position()
                .expect("rendered cursor position"),
            ratatui::layout::Position::new(4, input_y)
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
                "line-seven",
            ]
            .join("\n"),
            status: EntryStatus::Completed,
            details: serde_json::Value::Null,
        });

        terminal
            .draw(|frame| draw_test(frame, &mut state, &PromptEditor::default(), false))
            .expect("latest semantic lines render");
        let latest = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!latest.contains("line-one"));
        assert!(latest.contains("line-seven"));

        assert!(state.scroll_up(2));
        terminal
            .draw(|frame| draw_test(frame, &mut state, &PromptEditor::default(), false))
            .expect("older semantic lines render");
        let scrolled = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(scrolled.contains("line-one"));
        assert!(!scrolled.contains("line-seven"));
    }
}

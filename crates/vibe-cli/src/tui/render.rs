use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

mod completion_popup;
mod entries;
mod markdown;
mod overlays;
mod palette;
mod rewind;
mod text;

use super::chat_input::{InputMode, Safety, VoicePhase};
use super::completion::CompletionEngine;
use super::composer_layout::{CHROME_HEIGHT, ComposerLayout, PROMPT_WIDTH};
use super::input::{PromptEditor, VisualLayout};
use super::rewind::{RewindAction, RewindState};
use super::setup::ResolvedTheme;
use super::state::{TranscriptKind, TuiState};
use super::transcript;
use entries::semantic_lines;
use overlays::{draw_callback_overlay, draw_overlay};
use rewind::draw_rewind;
use text::{MAX_RENDER_LINES, sanitize_inline, truncate_width, wrapped_terminal_lines};

pub use text::{MAX_RENDER_CHARS, RenderLimits, sanitize_terminal};

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
    pub safety: Safety,
    pub switching: bool,
    pub feedback_active: bool,
    pub voice_phase: VoicePhase,
    pub voice_indicator: u8,
    pub banner: BannerContext<'a>,
    pub tokens: TokenState,
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
    let requested_completion_height = completion_popup::requested_height(completion);
    let activity_height = u16::from(activity_text(state).is_some());
    let queue_lines = state.prompt_queue.presentation_lines();
    let queue_height = u16::try_from(queue_lines.len()).unwrap_or(u16::MAX).min(6);
    let composer = ComposerLayout::for_viewport(
        editor,
        frame.area().width,
        frame.area().height,
        input_mode.prefix_len(),
    );
    let input_height = composer.input_height();
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
        queue_area,
        activity_area,
        completion_area,
        input_area,
        footer_area,
    ] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(queue_height),
        Constraint::Length(activity_height),
        Constraint::Length(completion_height),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_transcript(frame, transcript_area, state, theme, context.banner);
    draw_queue(
        frame,
        queue_area,
        &queue_lines,
        state.prompt_queue.scroll_offset(),
        theme,
    );
    draw_activity(frame, activity_area, state, theme);
    completion_popup::draw(frame, completion_area, completion, theme);
    draw_input(
        frame, input_area, editor, input_mode, theme, context, &composer,
    );
    // Reference `QuitManager.request_confirmation` writes into the path display.
    let quit_prompt = state
        .quit_confirmation
        .pending_key()
        .map(|key| crate::tui::exit::quit_prompt(key, state.prompt_queue.len()));
    draw_footer(
        frame,
        footer_area,
        context.cwd,
        context.tokens,
        theme,
        quit_prompt.as_deref(),
    );
    if let Some(overlay) = &state.overlay {
        draw_overlay(frame, overlay, state.session_delete.as_ref(), theme);
    }
    if let Some(rewind) = &state.rewind {
        draw_rewind(frame, rewind, theme);
    }
    if let Some(callback) = &state.callback {
        draw_callback_overlay(frame, callback, state.callback_scroll_offset, theme);
    }
}

fn draw_queue(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: &[String],
    scroll_offset: usize,
    theme: ResolvedTheme,
) {
    if area.height == 0 || lines.is_empty() {
        return;
    }
    let width = usize::from(area.width.max(1));
    let visible_items = usize::from(area.height).saturating_sub(1);
    let item_count = lines.len().saturating_sub(1);
    let start = scroll_offset.min(item_count.saturating_sub(visible_items));
    let mut visible = Vec::with_capacity(usize::from(area.height));
    let mut header = lines[0].clone();
    if item_count > visible_items {
        header.push_str("  Alt+PgUp/PgDn");
    }
    visible.push(header);
    visible.extend(lines.iter().skip(1 + start).take(visible_items).cloned());
    let rendered = visible
        .iter()
        .enumerate()
        .map(|(index, line)| {
            Line::styled(
                truncate_width(&sanitize_inline(line), width),
                if index == 0 {
                    theme.warning().add_modifier(Modifier::BOLD)
                } else {
                    theme.muted()
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(rendered), area);
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
    let visible = state
        .entries
        .iter()
        .filter(|entry| entry.kind != TranscriptKind::Reasoning || state.show_reasoning)
        .collect::<Vec<_>>();
    'entries: for (index, entry) in visible.iter().enumerate().rev() {
        // Consecutive tool-group members are packed; anything else opens with a
        // blank line, exactly as the reference spaces a group from the
        // surrounding conversation.
        let packed = transcript::keeps_tool_group(entry)
            && index
                .checked_sub(1)
                .and_then(|previous| visible.get(previous))
                .is_some_and(|previous| transcript::keeps_tool_group(previous));
        let mut entry_lines = semantic_lines(entry, area.width, theme, state.tools_collapsed);
        if !packed {
            entry_lines.insert(0, Line::default());
        }
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
    // Interaction resolves against what was painted, so the frame publishes its
    // visible text before handing the lines to the widget.
    let painted = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
    state.transcript_view.publish(area.y, painted.clone());
    for (index, from, to) in state.transcript_view.selection_ranges() {
        let (Some(line), Some(text)) = (lines.get_mut(index), painted.get(index)) else {
            continue;
        };
        *line = selected_line(text, from, to);
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Repaints one painted line with its selected range reversed, the way the
/// reference marks a transcript selection.
fn selected_line(text: &str, from: usize, to: usize) -> Line<'static> {
    let characters = text.chars().collect::<Vec<_>>();
    let slice = |range: std::ops::Range<usize>| -> String {
        characters.get(range).unwrap_or_default().iter().collect()
    };
    Line::from(vec![
        Span::raw(slice(0..from)),
        Span::styled(
            slice(from..to),
            Style::default().add_modifier(Modifier::REVERSED),
        ),
        Span::raw(slice(to..characters.len())),
    ])
}

fn banner_lines(banner: BannerContext<'_>, theme: ResolvedTheme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    lines.extend(
        PETIT_CHAT
            .into_iter()
            .map(|line| Line::styled(line, theme.base())),
    );
    lines.push(Line::default());
    let mut identity = vec![
        Span::styled("Mistral Vibe", theme.orange().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(
            format!("v{} · ", sanitize_inline(banner.version)),
            theme.base(),
        ),
        Span::styled(
            format!(
                "{}[{}]",
                sanitize_inline(banner.model),
                sanitize_inline(banner.thinking)
            ),
            theme.secondary(),
        ),
    ];
    if let Some(plan) = banner.plan {
        identity.push(Span::styled(
            format!(" · {}", sanitize_inline(plan)),
            theme.base(),
        ));
    }
    lines.push(Line::from(identity));
    lines.push(Line::styled(banner_counts(banner), theme.base()));
    lines.push(Line::from(vec![
        Span::styled("Type ", theme.base()),
        Span::styled("/help", theme.secondary()),
        Span::styled(" for more information", theme.base()),
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
    if let Some(activity) = state.activity.as_ref() {
        return Some((activity.line(), false));
    }
    // Reference `NarratorStatus`: narration keeps its own visible state once the
    // loading indicator is gone.
    state
        .narrator
        .status_line()
        .map(|narration| (narration, false))
}

fn draw_activity(frame: &mut Frame<'_>, area: Rect, state: &TuiState, theme: ResolvedTheme) {
    let Some((text, warning)) = activity_text(state) else {
        return;
    };
    frame.render_widget(
        Paragraph::new(truncate_width(&text, usize::from(area.width))).style(if warning {
            theme.warning()
        } else {
            theme.orange()
        }),
        area,
    );
}

fn draw_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    cwd: &Path,
    tokens: TokenState,
    theme: ResolvedTheme,
    quit_prompt: Option<&str>,
) {
    let path = quit_prompt.map_or_else(|| display_path(cwd), ToOwned::to_owned);
    let context = format_context_progress(tokens);
    let context_width = u16::try_from(UnicodeWidthStr::width(context.as_str()))
        .unwrap_or(u16::MAX)
        .min(area.width);
    let [path_area, context_area] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(context_width)]).areas(area);
    frame.render_widget(
        Paragraph::new(truncate_width(&path, usize::from(path_area.width))).style(
            if quit_prompt.is_some() {
                theme.warning()
            } else {
                theme.muted()
            },
        ),
        path_area,
    );
    frame.render_widget(
        Paragraph::new(context)
            .alignment(Alignment::Right)
            .style(theme.muted()),
        context_area,
    );
}

/// Reference `ContextProgress.watch_tokens`.
#[must_use]
pub fn format_context_progress(tokens: TokenState) -> String {
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
    composer: &ComposerLayout,
) {
    let input_width = composer.width();
    let title = if context.secret_input {
        " API key (stored in native keyring) "
    } else if context.switching {
        " Switching "
    } else if context.feedback_active {
        " Rate response: 1-3, 0 later, Esc dismisses "
    } else if context.agent_name.is_empty() {
        ""
    } else {
        context.agent_name
    };
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(composer_border_style(
            theme,
            context.safety,
            context.voice_phase,
        ))
        .title_style(theme.muted())
        .title_alignment(Alignment::Right)
        .title(title);
    frame.render_widget(block, area);
    let body = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(CHROME_HEIGHT),
    );
    let [prompt_area, input_area, _trailing_area] = Layout::horizontal([
        Constraint::Length(PROMPT_WIDTH),
        Constraint::Length(u16::try_from(input_width).unwrap_or(u16::MAX)),
        Constraint::Min(0),
    ])
    .areas(body);
    let text = editor_text(
        editor,
        context.secret_input,
        composer.visual(),
        composer.scroll(),
        usize::from(input_area.height),
    );
    const PEAK_BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    const FILL_BLOCKS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let indicator = usize::from(context.voice_indicator.min(7));
    let prompt = if context.voice_phase == VoicePhase::Transcribing {
        FILL_BLOCKS[indicator]
    } else if context.voice_phase.is_active() {
        PEAK_BLOCKS[indicator]
    } else if context.switching {
        '⠋'
    } else {
        input_mode.symbol()
    };
    frame.render_widget(
        Paragraph::new(prompt.to_string()).style(theme.orange()),
        prompt_area,
    );
    let input_style = if context.voice_phase.is_active() {
        theme.muted()
    } else {
        theme.base()
    };
    frame.render_widget(Paragraph::new(text).style(input_style), input_area);
    if input_area.width > 0 && input_area.height > 0 {
        frame.set_cursor_position((
            input_area.x.saturating_add(
                u16::try_from(composer.cursor_column())
                    .unwrap_or(u16::MAX)
                    .min(input_area.width.saturating_sub(1)),
            ),
            input_area.y.saturating_add(
                u16::try_from(composer.cursor_row().saturating_sub(composer.scroll()))
                    .unwrap_or(u16::MAX)
                    .min(input_area.height.saturating_sub(1)),
            ),
        ));
    }
}

fn composer_border_style(theme: ResolvedTheme, safety: Safety, voice: VoicePhase) -> Style {
    if voice.is_active() {
        return theme.orange();
    }
    match safety {
        Safety::Neutral => theme.muted(),
        Safety::Safe => theme.success(),
        Safety::Destructive => theme.warning(),
        Safety::Yolo => theme.error(),
    }
}

/// Maps an absolute terminal coordinate into the scrolled editor body.
/// Coordinates outside the visible composer return `None` without inventing
/// a nearest selection target.
pub(crate) fn editor_mouse_cell(
    editor: &PromptEditor,
    screen: Rect,
    _secret_input: bool,
    input_mode: InputMode,
    x: u16,
    y: u16,
) -> Option<(usize, usize)> {
    let composer =
        ComposerLayout::for_viewport(editor, screen.width, screen.height, input_mode.prefix_len());
    let input_height = composer.input_height();
    let input_y = screen
        .y
        .saturating_add(screen.height)
        .saturating_sub(1)
        .saturating_sub(input_height);
    let body_y = input_y.saturating_add(1);
    let body_height = composer.body_height();
    let input_x = screen.x.saturating_add(PROMPT_WIDTH);
    let input_width = u16::try_from(composer.width()).unwrap_or(u16::MAX);
    if x < input_x
        || x >= input_x.saturating_add(input_width)
        || y < body_y
        || y >= body_y.saturating_add(body_height)
    {
        return None;
    }
    Some((
        usize::from(y.saturating_sub(body_y)).saturating_add(composer.scroll()),
        usize::from(x.saturating_sub(input_x)),
    ))
}

fn editor_text(
    editor: &PromptEditor,
    secret_input: bool,
    layout: &VisualLayout,
    scroll: usize,
    height: usize,
) -> Text<'static> {
    let selection = editor.selection();
    let mut lines = Vec::with_capacity(height);
    for range in layout.lines().iter().skip(scroll).take(height) {
        let mut line = Vec::new();
        let mut column = 0usize;
        for index in range.clone() {
            let grapheme = layout.grapheme(editor.text(), index);
            let visible = if secret_input {
                "•".to_owned()
            } else if grapheme == "\t" {
                " ".repeat(4 - (column % 4))
            } else if grapheme.chars().all(char::is_control) {
                String::new()
            } else {
                sanitize_inline(grapheme)
            };
            column = column.saturating_add(UnicodeWidthStr::width(visible.as_str()));
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
    }
    Text::from(lines.into_iter().map(Line::from).collect::<Vec<_>>())
}

#[cfg(test)]
#[path = "render/render_tests.rs"]
mod render_tests;

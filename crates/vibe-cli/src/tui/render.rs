use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

mod completion_popup;
mod composer;
mod entries;
mod markdown;
mod overlays;
mod palette;
mod rewind;
mod subagents;
mod text;

use super::chat_input::{InputMode, Safety, VoicePhase};
use super::completion::CompletionEngine;
use super::composer_layout::{CHROME_HEIGHT, ComposerLayout, PROMPT_WIDTH};
use super::diagnostics::Activity;
use super::input::{PromptEditor, VisualLayout};
use super::loading::LoadingAnimation;
use super::rewind::{RewindChoice, RewindState};
use super::setup::ResolvedTheme;
use super::state::{TranscriptEntry, TranscriptKind, TuiState};
use super::transcript;
use composer::draw_input;
pub(crate) use composer::editor_mouse_cell;
pub(in crate::tui) use entries::follows_tool_toggle;
#[cfg(test)]
pub(in crate::tui) use entries::group_label;
use entries::{Folding, Painted, effect_block, fold_keys, semantic_lines, tool_group};
use overlays::{draw_callback_overlay, draw_overlay};
use rewind::draw_rewind;
use text::{MAX_RENDER_LINES, sanitize_inline, truncate_width, wrapped_terminal_lines};

pub(in crate::tui) use text::clean_output;
pub use text::{MAX_RENDER_CHARS, RenderLimits, sanitize_terminal};

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
    /// The spinner frame shown in place of the model while a new session
    /// waits on the rollout that may route it onto another one. Reference
    /// `SpinnerText`, driven by `ConfigView.awaitingExperimentModel`.
    pub model_spinner: Option<&'static str>,
}

/// The frames reference `BrailleSpinner` cycles through.
const MODEL_SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long one spinner frame shows, reference `SpinnerText._SPINNER_INTERVAL`.
const MODEL_SPINNER_INTERVAL_MS: u64 = 100;

/// The spinner frame the model slot shows at `now_ms`.
#[must_use]
pub fn model_spinner_frame(now_ms: u64) -> &'static str {
    let index = usize::try_from((now_ms / MODEL_SPINNER_INTERVAL_MS) % 10).unwrap_or(0);
    MODEL_SPINNER_FRAMES.get(index).copied().unwrap_or("⠋")
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
    // Reference `_show_subagent_chat`: a viewed child hides the composer and
    // the conversation's loading line, and the footer gauges its context.
    let viewing = state.subagents.is_viewing();
    let mut context = context;
    if let Some(child) = state.subagents.viewed() {
        context.tokens.current_tokens = child.context_tokens;
    }
    state.scroll_offset = state.subagents.settle_scroll(state.scroll_offset);
    let requested_completion_height = if viewing {
        0
    } else {
        completion_popup::requested_height(completion)
    };
    let activity_height =
        u16::from(activity_text(state).is_some() || state.inline_notice.is_some());
    let queue_lines = state.prompt_queue.presentation_lines();
    let queue_height = u16::try_from(queue_lines.len()).unwrap_or(u16::MAX).min(6);
    let composer = ComposerLayout::for_viewport(
        editor,
        frame.area().width,
        frame.area().height,
        input_mode.prefix_len(),
    );
    let input_height = if viewing { 0 } else { composer.input_height() };
    let list_height = subagents::list_height(&state.subagents);
    let todo_summary = state
        .todos
        .as_ref()
        .and_then(crate::tui::todos::TodoTracker::summary);
    let todo_height = u16::from(todo_summary.is_some());
    let completion_height = requested_completion_height.min(
        frame
            .area()
            .height
            .saturating_sub(activity_height)
            .saturating_sub(input_height)
            .saturating_sub(list_height)
            .saturating_sub(todo_height)
            .saturating_sub(2),
    );
    let [
        transcript_area,
        queue_area,
        activity_area,
        completion_area,
        input_area,
        list_area,
        todo_area,
        footer_area,
    ] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(queue_height),
        Constraint::Length(activity_height),
        Constraint::Length(completion_height),
        Constraint::Length(input_height),
        Constraint::Length(list_height),
        Constraint::Length(todo_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let source = match state.subagents.view_entries() {
        Some(entries) => TranscriptSource::Child {
            entries,
            running: state
                .subagents
                .viewed()
                .is_some_and(|child| child.status.loading_status().is_some()),
        },
        None => TranscriptSource::Conversation(context.banner),
    };
    draw_transcript(frame, transcript_area, state, theme, &source);
    let selected = state
        .queue_selection
        .as_ref()
        .and_then(|selection| state.prompt_queue.index_of(&selection.selected));
    draw_queue(
        frame,
        queue_area,
        &queue_lines,
        (state.prompt_queue.scroll_offset(), selected),
        theme,
    );
    match state.subagents.viewed() {
        Some(child) => {
            if let Some(status) = child.status.loading_status() {
                frame.render_widget(
                    Paragraph::new(truncate_width(
                        &format!(
                            "{} {status}",
                            model_spinner_frame(state.animation_frame * 100)
                        ),
                        usize::from(activity_area.width),
                    ))
                    .style(theme.orange()),
                    activity_area,
                );
            }
        }
        None => draw_activity(frame, activity_area, state, theme),
    }
    state.subagent_rows = subagents::draw_list(frame, list_area, &state.subagents, theme);
    if viewing {
        draw_footer(
            frame,
            footer_area,
            context.cwd,
            context.tokens,
            theme,
            state
                .quit_confirmation
                .pending_key()
                .map(|key| crate::tui::exit::quit_prompt(key, state.prompt_queue.len()))
                .as_deref(),
        );
        draw_modals(frame, state, theme);
        return;
    }
    completion_popup::draw(frame, completion_area, completion, theme);
    // Reference `_lock_input_for_selection`: a selected queued prompt hides the
    // caret of the draft it locked.
    let caret = !state
        .queue_selection
        .as_ref()
        .is_some_and(|selection| !selection.editing);
    let ghost = completion.inline_skill_suffix();
    draw_input(
        frame,
        input_area,
        editor,
        input_mode,
        theme,
        context,
        (&composer, caret, ghost.as_deref()),
    );
    // Reference `TodoStatusRow`: one muted line between the composer and the
    // bottom bar, cut with an ellipsis rather than wrapped.
    if let Some(summary) = &todo_summary {
        frame.render_widget(
            Paragraph::new(truncate_width(
                &sanitize_inline(summary),
                usize::from(todo_area.width),
            ))
            .style(theme.muted()),
            todo_area,
        );
        state.todo_row = (todo_area.height > 0).then_some(todo_area);
    } else {
        state.todo_row = None;
    }
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
    draw_modals(frame, state, theme);
}

fn draw_modals(frame: &mut Frame<'_>, state: &TuiState, theme: ResolvedTheme) {
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

/// Whose history a transcript paint shows.
enum TranscriptSource<'a> {
    /// The conversation's own: windowed, topped by "Load more messages" and
    /// the banner, with the deltas its todo writes made.
    Conversation(BannerContext<'a>),
    /// Reference `SubagentTranscripts`: the viewed child's history, painted
    /// whole, its newest tool group running while the child does.
    Child {
        entries: Vec<TranscriptEntry>,
        running: bool,
    },
}

fn draw_queue(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: &[String],
    (scroll_offset, selected): (usize, Option<usize>),
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
                } else if selected == Some(start + index - 1) {
                    // Reference `.user-message.queue-selected`.
                    theme.muted().add_modifier(Modifier::REVERSED)
                } else {
                    theme.muted()
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(rendered), area);
}

/// Reference `HistoryLoadMoreMessage._label_text`.
fn load_more_label(remaining: Option<usize>) -> String {
    remaining.map_or_else(
        || "Load more messages".to_owned(),
        |remaining| format!("Load more messages ({remaining})"),
    )
}

fn draw_transcript(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut TuiState,
    theme: ResolvedTheme,
    source: &TranscriptSource<'_>,
) {
    // Only the conversation's own transcript has a banner, and with it the
    // window, the "Load more messages" row and the todo deltas.
    let (entries, hidden, running, banner) = match source {
        TranscriptSource::Conversation(banner) => (
            state.entries.as_slice(),
            state.hidden_history(),
            state.waiting,
            Some(banner),
        ),
        TranscriptSource::Child { entries, running } => (entries.as_slice(), 0, *running, None),
    };
    let visible_height = usize::from(area.height).max(1);
    let mut newest_first = Vec::new();
    let mut action_rows = Vec::new();
    let mut rendered_chars = 0usize;
    let mut history_truncated = false;
    let visible = transcript::place_hook_lines(
        entries
            .iter()
            .skip(hidden)
            .filter(|entry| entry.kind != TranscriptKind::Reasoning || state.show_reasoning)
            .filter(|entry| transcript::is_drawn(entry))
            .collect::<Vec<_>>(),
    );
    // Reference `ToolGroup`: a run of tool calls, their reasoning and hook
    // notices folds under one summary; anything else stands alone.
    let mut blocks: Vec<Vec<&TranscriptEntry>> = Vec::new();
    for entry in visible {
        let grouped = transcript::keeps_tool_group(entry);
        match blocks.last_mut() {
            Some(block)
                if grouped
                    && block
                        .first()
                        .is_some_and(|first| transcript::keeps_tool_group(first)) =>
            {
                block.push(entry);
            }
            _ => blocks.push(vec![entry]),
        }
    }
    // A widget mounts once and keeps its own fold from then on.
    for block in &blocks {
        for (key, collapsed) in fold_keys(block, state.tools_collapsed) {
            state.folds.entry(key).or_insert(collapsed);
        }
    }
    let todo_deltas = (banner.is_some() && state.todos.is_some())
        .then(|| crate::tui::todos::todo_deltas(entries));
    let folding = Folding {
        folds: &state.folds,
        frame: state.animation_frame,
        streams: &state.effect_streams,
        todo_deltas: todo_deltas.as_ref(),
    };
    let block_count = blocks.len();
    'blocks: for (index, block) in blocks.iter().enumerate().rev() {
        let painted = match block.as_slice() {
            [entry] if !transcript::keeps_tool_group(entry) => match transcript::region(entry) {
                transcript::Region::Effect(effect) => {
                    effect_block(entry, &effect, true, folding, area.width, theme)
                }
                _ => Painted {
                    lines: semantic_lines(entry, area.width, theme, state.tools_collapsed),
                    actions: Vec::new(),
                },
            },
            // Reference `_finalize_tool_group`: the newest group keeps running
            // until the turn that opened it ends.
            members => tool_group(
                members,
                index + 1 == block_count && running,
                folding,
                area.width,
                theme,
            ),
        };
        // Every block opens with a blank line, as the reference spaces a group
        // or a message from what precedes it.
        let newest_line = newest_first.len() + painted.lines.len();
        action_rows.extend(
            painted
                .actions
                .into_iter()
                .map(|(line, action)| (newest_line - (line + 1), action)),
        );
        for line in painted
            .lines
            .into_iter()
            .rev()
            .chain(std::iter::once(Line::default()))
        {
            let line_chars = line.to_string().chars().count();
            if newest_first.len() >= MAX_RENDER_LINES
                || rendered_chars.saturating_add(line_chars) > MAX_RENDER_CHARS
            {
                history_truncated = true;
                break 'blocks;
            }
            rendered_chars = rendered_chars.saturating_add(line_chars);
            newest_first.push(line);
        }
    }
    // Reference `HistoryLoadMoreMessage`, mounted above the oldest entry shown.
    let mut load_more_index = None;
    if !history_truncated && banner.is_some() && state.has_older_history() {
        load_more_index = Some(newest_first.len());
        newest_first.push(Line::styled(
            load_more_label(state.older_history_remaining()),
            theme.secondary(),
        ));
        newest_first.push(Line::default());
    }
    if !history_truncated && let Some(banner) = banner {
        for line in banner_lines(*banner, state.banner_cat.frame(), theme)
            .into_iter()
            .rev()
        {
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
    let padding = visible_height.saturating_sub(lines.len());
    // Where a row counted from the newest line landed on screen, counted from
    // the top.
    let on_screen = |index: usize| {
        index
            .checked_sub(state.scroll_offset)
            .filter(|from_bottom| *from_bottom < lines.len())
            .map(|from_bottom| padding + lines.len() - 1 - from_bottom)
    };
    let load_more_line = load_more_index.and_then(on_screen);
    let actions = action_rows
        .into_iter()
        .filter_map(|(index, action)| Some((on_screen(index)?, action)))
        .collect::<Vec<_>>();
    if padding > 0 {
        lines.splice(0..0, std::iter::repeat_with(Line::default).take(padding));
    }
    // Interaction resolves against what was painted, so the frame publishes its
    // visible text before handing the lines to the widget.
    let painted = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
    state.transcript_view.publish(area.y, painted.clone());
    state.transcript_view.set_load_more_line(load_more_line);
    state.transcript_view.set_row_actions(actions);
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

fn banner_lines(
    banner: BannerContext<'_>,
    cat: [&'static str; 3],
    theme: ResolvedTheme,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    lines.extend(cat.into_iter().map(|line| Line::styled(line, theme.base())));
    lines.push(Line::default());
    let mut identity = vec![
        Span::styled("Mistral Vibe", theme.orange().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(
            format!("v{} · ", sanitize_inline(banner.version)),
            theme.base(),
        ),
        Span::styled(
            banner.model_spinner.map_or_else(
                || {
                    format!(
                        "{}[{}]",
                        sanitize_inline(banner.model),
                        sanitize_inline(banner.thinking)
                    )
                },
                ToOwned::to_owned,
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
        // Reference `ANIMATION_INTERVAL`: a frame every 150 ms, read off the
        // tenth-of-a-second animation tick.
        .status_line(state.animation_frame * 2 / 3)
        .map(|narration| (narration, false))
}

fn draw_activity(frame: &mut Frame<'_>, area: Rect, state: &TuiState, theme: ResolvedTheme) {
    // Reference `#inline-notice`: after the loading content, which takes the
    // rest of the row, in the muted color.
    let area = match state.inline_notice.as_ref() {
        Some(notice) => {
            let text = truncate_width(&sanitize_inline(&notice.text), usize::from(area.width));
            let width = u16::try_from(text.width()).unwrap_or(area.width);
            let [rest, notice_area] =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(area);
            frame.render_widget(Paragraph::new(text).style(theme.muted()), notice_area);
            rest
        }
        None => area,
    };
    let Some((text, warning)) = activity_text(state) else {
        return;
    };
    // Anything but a warning shown while a turn runs is the turn's own line.
    if let (Some(activity), false) = (state.activity.as_ref(), warning) {
        frame.render_widget(
            Paragraph::new(loading_line(
                activity,
                &state.loading,
                state.typing_hold,
                theme,
            )),
            area,
        );
        return;
    }
    frame.render_widget(
        Paragraph::new(truncate_width(&text, usize::from(area.width))).style(if warning {
            theme.warning()
        } else {
            theme.orange()
        }),
        area,
    );
}

/// Reference `LoadingWidget`: the snake and the status take the color sweep
/// cell by cell, and the hint follows in the muted color with its keys picked
/// out.
fn loading_line(
    activity: &Activity,
    loading: &LoadingAnimation,
    typing_hold: bool,
    theme: ResolvedTheme,
) -> Line<'static> {
    let swept = |position: usize| {
        let (red, green, blue) = loading.color_at(position);
        theme.colored(Color::Rgb(red, green, blue))
    };
    let mut spans = vec![Span::styled(loading.spinner(), swept(0)), Span::raw(" ")];
    let status = sanitize_inline(&activity.status);
    let length = status.chars().count();
    spans.extend(
        status
            .chars()
            .enumerate()
            .map(|(index, character)| Span::styled(character.to_string(), swept(1 + index))),
    );
    spans.push(Span::styled("… ", swept(1 + length)));
    spans.extend(
        activity.hint_parts().into_iter().map(|(text, key)| {
            Span::styled(text, if key { theme.orange() } else { theme.muted() })
        }),
    );
    // Reference `show_debounce_hint`.
    if typing_hold {
        spans.push(Span::styled(
            " typing detected, waiting…",
            theme.muted().add_modifier(Modifier::ITALIC),
        ));
    }
    Line::from(spans)
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

pub(super) fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        return format!("{:.1}M", tokens as f64 / 1_000_000.0);
    }
    if tokens >= 1_000 {
        return format!("{}k", tokens / 1_000);
    }
    tokens.to_string()
}

fn display_path(path: &Path) -> String {
    let Some(home) = vibe_core::platform::user_home() else {
        return path.to_string_lossy().into_owned();
    };
    match path.strip_prefix(&home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".to_owned(),
        Ok(relative) => Path::new("~").join(relative).to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
#[path = "render/render_tests.rs"]
mod render_tests;

//! One transcript entry, projected to the lines the frame paints.
//!
//! [`super::transcript`] decides what an entry *means*; this module decides how
//! that meaning is laid out. The split is what keeps the semantic projection
//! free of ratatui and testable without a terminal.

use std::collections::BTreeMap;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::markdown::markdown_lines;
use super::text::{MAX_RENDER_LINES, sanitize_inline, truncate_width, wrapped_terminal_lines};
use crate::tui::composer_layout::PROMPT_WIDTH;
use crate::tui::setup::ResolvedTheme;
use crate::tui::state::{EntryStatus, TranscriptEntry};
use crate::tui::transcript::{self, Indicator};
use crate::tui::transcript_view::RowAction;

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
        transcript::Region::Effect(effect) => {
            let (streams, mut folds) = (BTreeMap::new(), BTreeMap::new());
            folds.insert(entry.id.clone(), tools_collapsed);
            let folding = Folding {
                folds: &folds,
                frame: 0,
                streams: &streams,
            };
            effect_block(entry, &effect, true, folding, width, theme).lines
        }
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
        transcript::Region::SlashCommand { message } => {
            prompt_message_lines(entry, width, theme, '/', &message)
        }
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

/// How the tool presentation folds and animates for one frame.
#[derive(Debug, Clone, Copy)]
pub(super) struct Folding<'a> {
    /// Whether each section, group and reasoning block is folded. Reference
    /// widgets each hold their own state, so one missing here is still folded,
    /// as every reference section starts.
    pub(super) folds: &'a BTreeMap<String, bool>,
    /// The animation tick a running indicator reads its frame from.
    pub(super) frame: u64,
    /// What each running effect last appended to its output.
    pub(super) streams: &'a BTreeMap<String, String>,
}

impl Folding<'_> {
    fn collapsed(&self, key: &str) -> bool {
        self.folds.get(key).copied().unwrap_or(true)
    }
}

/// The fold keys a block paints, each with the state a new widget starts in.
/// Reference `ToolResultMessage` sections and `ToolGroup` start folded;
/// `ReasoningMessage` starts as `Ctrl+O` last left the tools.
pub(super) fn fold_keys(block: &[&TranscriptEntry], tools_collapsed: bool) -> Vec<(String, bool)> {
    let mut keys = Vec::new();
    if let [first, ..] = block
        && transcript::keeps_tool_group(first)
    {
        keys.push((group_key(first), true));
    }
    for entry in block {
        match entry.kind {
            crate::tui::state::TranscriptKind::Effect => {
                keys.push((entry.id.clone(), true));
                keys.push((error_key(entry), true));
            }
            crate::tui::state::TranscriptKind::Reasoning => {
                keys.push((reasoning_key(entry), tools_collapsed));
            }
            _ => {}
        }
    }
    keys
}

/// Whether `Ctrl+O` sets the fold `key` names. Reference `action_toggle_tool`
/// reaches every `CollapsibleSection` and `ToolGroup`, and no reasoning.
#[must_use]
pub(in crate::tui) fn follows_tool_toggle(key: &str) -> bool {
    !key.starts_with(REASONING_PREFIX)
}

const REASONING_PREFIX: &str = "reasoning:";

fn reasoning_key(entry: &TranscriptEntry) -> String {
    format!("{REASONING_PREFIX}{}", entry.id)
}

fn error_key(entry: &TranscriptEntry) -> String {
    format!("{}:error", entry.id)
}

/// Lines painted for one block of the transcript, with what activating each
/// row does, counted from the block's first line.
#[derive(Debug, Default)]
pub(super) struct Painted {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) actions: Vec<(usize, RowAction)>,
}

impl Painted {
    fn plain(lines: Vec<Line<'static>>) -> Self {
        Self {
            lines,
            actions: Vec::new(),
        }
    }
}

/// The key a tool group folds under: the entry it opens with.
#[must_use]
pub(super) fn group_key(first: &TranscriptEntry) -> String {
    format!("group:{}", first.id)
}

/// Reference `PulseSpinner`, which every running status indicator animates
/// with: six filled frames, then four hollow ones.
fn pulse_frame(frame: u64) -> &'static str {
    if frame % 10 < 6 { "■" } else { "□" }
}

/// Reference `ExpandingBorder`: every row but the last carries `⎢`, and the
/// last closes the border with `⎣`, both in the muted color.
fn bordered(painted: Painted, theme: ResolvedTheme) -> Painted {
    let last = painted.lines.len().saturating_sub(1);
    let lines = painted
        .lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let mut spans = vec![Span::styled(
                if index == last { "  ⎣ " } else { "  ⎢ " },
                theme.muted(),
            )];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect();
    let actions = painted
        .actions
        .into_iter()
        .map(|(line, action)| (line, action.indented(BORDER_WIDTH)))
        .collect();
    Painted { lines, actions }
}

/// Columns the `  ⎢ ` border takes before the content it carries.
const BORDER_WIDTH: usize = 4;

/// Reference `ToolGroup`: consecutive tool calls, their results and the
/// reasoning between them fold under one summary line naming what was done.
pub(super) fn tool_group(
    members: &[&TranscriptEntry],
    running: bool,
    folding: Folding<'_>,
    width: u16,
    theme: ResolvedTheme,
) -> Painted {
    let Some(first) = members.first() else {
        return Painted::default();
    };
    let key = group_key(first);
    let collapsed = folding.collapsed(&key);
    let mut kinds = Vec::new();
    let mut reasoning = false;
    let mut settled = Indicator::Success;
    for member in members {
        match transcript::region(member) {
            transcript::Region::Effect(effect) => {
                if !kinds.contains(&effect.kind) {
                    kinds.push(effect.kind);
                }
                // Reference `ToolGroup.settle_indicator`: the last call to
                // settle names the outcome the group settles with.
                if effect.indicator != Indicator::Running {
                    settled = effect.indicator;
                }
            }
            transcript::Region::Reasoning => reasoning = true,
            _ => {}
        }
    }
    let label = group_label(&kinds, reasoning, running);
    let glyph = if running {
        Span::styled(format!("{} ", pulse_frame(folding.frame)), theme.orange())
    } else {
        Span::styled(
            format!("{} ", if collapsed { "⏵" } else { "⏷" }),
            indicator_style(settled, theme),
        )
    };
    let header = Line::from(vec![
        glyph,
        Span::styled(
            truncate_width(&label, usize::from(width.max(3)) - 2),
            theme.muted(),
        ),
    ]);
    let mut painted = Painted {
        lines: vec![header],
        actions: vec![(0, RowAction::Toggle(key))],
    };
    if collapsed {
        return painted;
    }
    let mut content = Painted::default();
    let inner_width = width.saturating_sub(BORDER_WIDTH as u16);
    for (index, member) in members.iter().enumerate() {
        let member_painted = match transcript::region(member) {
            transcript::Region::Effect(effect) => {
                // Reference `_resolve_pending_errors`: a failure a later
                // success recovered from stays muted, and one nothing recovered
                // from turns red once the group closes.
                let recovered = members[index + 1..].iter().any(|later| {
                    matches!(
                        transcript::region(later),
                        transcript::Region::Effect(later) if later.indicator == Indicator::Success
                    )
                });
                effect_block(
                    member,
                    &effect,
                    !running && !recovered,
                    folding,
                    inner_width,
                    theme,
                )
            }
            transcript::Region::Reasoning => reasoning_block(member, folding, inner_width, theme),
            _ => Painted::plain(semantic_lines(member, inner_width, theme, true)),
        };
        let offset = content.lines.len();
        content.lines.extend(member_painted.lines);
        content.actions.extend(
            member_painted
                .actions
                .into_iter()
                .map(|(line, action)| (offset + line, action)),
        );
    }
    let content = bordered(content, theme);
    painted.lines.extend(content.lines);
    painted.actions.extend(
        content
            .actions
            .into_iter()
            .map(|(line, action)| (line + 1, action)),
    );
    painted
}

/// Reference `ToolGroupHeader.get_content`: one label per kind in the order
/// the kinds arrived, then the reasoning, joined and capitalized.
pub(in crate::tui) fn group_label(
    kinds: &[transcript::EffectKind],
    reasoning: bool,
    running: bool,
) -> String {
    let mut labels = kinds
        .iter()
        .map(|kind| category_label(*kind, running))
        .collect::<Vec<_>>();
    if reasoning {
        labels.push(if running { "thinking" } else { "thought" });
    }
    capitalized(&labels.join(", "))
}

/// Reference `_TOOL_CATEGORY_LABELS` and `_TOOL_CATEGORY_RUNNING_LABELS`.
fn category_label(kind: transcript::EffectKind, running: bool) -> &'static str {
    use transcript::EffectKind as Kind;
    match (kind, running) {
        (Kind::FileRead, false) => "read files",
        (Kind::FileRead, true) => "reading files",
        (Kind::FileEdit, false) => "edited files",
        (Kind::FileEdit, true) => "editing files",
        (Kind::FileWrite, false) => "wrote files",
        (Kind::FileWrite, true) => "writing files",
        (Kind::FileSearch, false) => "searched files",
        (Kind::FileSearch, true) => "searching files",
        (Kind::Shell, false) => "ran commands",
        (Kind::Shell, true) => "running commands",
        (Kind::WebSearch, false) => "searched the web",
        (Kind::WebSearch, true) => "searching the web",
        (Kind::WebFetch, false) => "fetched pages",
        (Kind::WebFetch, true) => "fetching pages",
        (Kind::Todo, false) => "updated todos",
        (Kind::Todo, true) => "updating todos",
        (Kind::UserQuestion, false) => "asked questions",
        (Kind::UserQuestion, true) => "asking questions",
        (Kind::Skill, false) => "loaded skills",
        (Kind::Skill, true) => "loading skills",
        (Kind::Subagent, false) => "ran subagents",
        (Kind::Subagent, true) => "running subagents",
        (Kind::Worktree, false) => "created worktrees",
        (Kind::Worktree, true) => "creating worktrees",
        (Kind::Tool | Kind::Process, false) => "called tools",
        (Kind::Tool | Kind::Process, true) => "calling tools",
    }
}

/// Python `str.capitalize`: the first character upper, every other lower.
fn capitalized(text: &str) -> String {
    let mut characters = text.chars();
    characters.next().map_or_else(String::new, |first| {
        first
            .to_uppercase()
            .chain(characters.flat_map(char::to_lowercase))
            .collect()
    })
}

fn indicator_style(indicator: Indicator, theme: ResolvedTheme) -> Style {
    match indicator {
        Indicator::Running => theme.orange(),
        Indicator::Success => theme.success(),
        Indicator::Error => theme.error(),
        Indicator::Muted => theme.muted(),
    }
}

/// Reference `ToolCallMessage` and `ToolResultMessage`: the call's status
/// header, its streamed line while it runs, and once it settles either a
/// header that folds its result (`HeaderCollapsibleSection`) or, for the
/// results that always open, the call header above the bordered result.
///
/// `escalated` says whether a failure reads red, which the reference decides
/// only once the group it belongs to has closed without recovering.
pub(super) fn effect_block(
    entry: &TranscriptEntry,
    effect: &transcript::EffectRegion,
    escalated: bool,
    folding: Folding<'_>,
    width: u16,
    theme: ResolvedTheme,
) -> Painted {
    let max_width = usize::from(width.max(1));
    let running = effect.indicator == Indicator::Running;
    let has_body = !effect.body.is_empty() || !effect.printed.is_empty();
    let collapsed = effect.collapsed_by_default && folding.collapsed(&entry.id);
    let settled_style = match effect.indicator {
        Indicator::Error if !escalated => theme.muted(),
        indicator => indicator_style(indicator, theme),
    };
    let glyph = if running {
        Span::styled(format!("{} ", pulse_frame(folding.frame)), theme.orange())
    } else if !effect.collapsed_by_default {
        Span::styled("⏵ ", settled_style)
    } else if !has_body {
        // Reference `HeaderCollapsibleSection(collapsible=False)`: nothing to
        // unfold, so a muted marker holds the disclosure slot.
        Span::styled("▪ ", theme.muted())
    } else {
        Span::styled(if collapsed { "⏵ " } else { "⏷ " }, settled_style)
    };
    // A running call that will fold reads dimmed, as `running.collapsible-result`
    // styles it.
    let dimmed = running && effect.collapsed_by_default;
    let mut header = vec![glyph];
    if !effect.verb.is_empty() {
        let verb = theme.effect().add_modifier(Modifier::BOLD);
        header.push(Span::styled(
            format!("{} ", sanitize_inline(&effect.verb)),
            if dimmed {
                verb.add_modifier(Modifier::DIM)
            } else {
                verb
            },
        ));
    }
    header.push(Span::styled(
        truncate_width(
            &sanitize_inline(&effect.message),
            max_width.saturating_sub(12),
        ),
        if dimmed { theme.muted() } else { theme.base() },
    ));
    if !effect.suffix.is_empty() {
        header.push(Span::styled(
            format!(" {}", sanitize_inline(&effect.suffix)),
            theme.muted(),
        ));
    }
    let mut painted = Painted::plain(vec![Line::from(header)]);
    if effect.collapsed_by_default && has_body && !running {
        painted
            .actions
            .push((0, RowAction::Toggle(entry.id.clone())));
    }
    // Reference `set_stream_message`: what the last update appended, under
    // the call and only while it runs.
    if let Some(appended) = folding.streams.get(&entry.id).filter(|_| running) {
        let stream = format!("→ {}", appended.trim_end_matches('\n'));
        for line in wrapped_terminal_lines(&stream, max_width.saturating_sub(4).max(1)) {
            painted.lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(line, theme.muted()),
            ]));
        }
    }
    if collapsed || !has_body {
        return painted;
    }
    let body_width = max_width.saturating_sub(BORDER_WIDTH);
    let mut body = Painted::default();
    push_body_lines(&mut body, &effect.printed, body_width, theme);
    if effect.failed && !effect.collapsed_by_default {
        // Reference `_render_result_expanded`: the error of a result that
        // always opens folds behind its line count, under what it printed.
        let key = error_key(entry);
        if folding.collapsed(&key) {
            body.actions
                .push((body.lines.len(), RowAction::Toggle(key)));
            body.lines.push(Line::from(vec![
                Span::styled("⏵ ", theme.muted()),
                Span::styled(
                    lines_label(
                        effect
                            .body
                            .iter()
                            .map(|line| line.text.split('\n').count())
                            .sum(),
                    ),
                    theme.muted(),
                ),
            ]));
        } else {
            push_body_lines(&mut body, &effect.body, body_width, theme);
            body.actions
                .push((body.lines.len(), RowAction::Toggle(key)));
            body.lines.push(Line::from(vec![
                Span::styled("⏷ ", theme.muted()),
                Span::styled("show less", theme.muted()),
            ]));
        }
    } else {
        push_body_lines(&mut body, &effect.body, body_width, theme);
    }
    let body = bordered(body, theme);
    let offset = painted.lines.len();
    painted.lines.extend(body.lines);
    painted.actions.extend(
        body.actions
            .into_iter()
            .map(|(line, action)| (offset + line, action)),
    );
    painted
}

/// Reference `lines_label`.
fn lines_label(count: usize) -> String {
    format!("{count} {}", if count == 1 { "line" } else { "lines" })
}

fn push_body_lines(
    painted: &mut Painted,
    lines: &[transcript::BodyLine],
    width: usize,
    theme: ResolvedTheme,
) {
    for line in lines.iter().take(MAX_RENDER_LINES) {
        let style = match line.style {
            transcript::BodyStyle::Plain => theme.base(),
            transcript::BodyStyle::Added => theme.success(),
            transcript::BodyStyle::Removed => theme.error(),
            transcript::BodyStyle::Warning => theme.warning(),
            transcript::BodyStyle::Error => theme.error(),
            transcript::BodyStyle::Muted => theme.muted(),
        };
        if let Some(url) = &line.link {
            painted.actions.push((
                painted.lines.len(),
                RowAction::Link {
                    column: 0,
                    url: url.clone(),
                },
            ));
        }
        painted.lines.push(Line::from(Span::styled(
            truncate_width(&sanitize_inline(&line.text), width),
            if line.link.is_some() {
                style.add_modifier(Modifier::UNDERLINED)
            } else {
                style
            },
        )));
    }
}

/// Reference `ReasoningMessage`: a pulsing "Thinking" while the model
/// reasons, then "Thought" behind a triangle that unfolds the reasoning.
/// `Ctrl+O` never reaches it; a click does.
fn reasoning_block(
    entry: &TranscriptEntry,
    folding: Folding<'_>,
    width: u16,
    theme: ResolvedTheme,
) -> Painted {
    let key = reasoning_key(entry);
    let collapsed = folding.collapsed(&key);
    let thinking = !entry.status.is_terminal();
    let glyph = if thinking {
        pulse_frame(folding.frame)
    } else if collapsed {
        "⏵"
    } else {
        "⏷"
    };
    let mut painted = Painted {
        lines: vec![Line::from(vec![
            Span::styled(
                format!("{glyph} "),
                if thinking {
                    theme.orange()
                } else {
                    theme.muted()
                },
            ),
            Span::styled(if thinking { "Thinking" } else { "Thought" }, theme.muted()),
        ])],
        actions: vec![(0, RowAction::Toggle(key))],
    };
    if !collapsed {
        painted.lines.extend(
            markdown_lines(&entry.text, usize::from(width), theme)
                .into_iter()
                .take(MAX_RENDER_LINES)
                .map(|line| {
                    Line::from(
                        line.spans
                            .into_iter()
                            .map(|span| Span::styled(span.content, theme.muted()))
                            .collect::<Vec<_>>(),
                    )
                }),
        );
    }
    painted
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
    prompt_message_lines(entry, width, theme, prompt, content)
}

/// One prompted message: the operator's own line under `>`, or the command line
/// a submission ran under `/`, which is the prompt character reference
/// `SlashCommandMessage` overrides `UserMessage` with. The slash form draws no
/// separator, matching that widget's `SHOW_SEPARATOR = False`.
fn prompt_message_lines(
    entry: &TranscriptEntry,
    width: u16,
    theme: ResolvedTheme,
    prompt: char,
    content: &str,
) -> Vec<Line<'static>> {
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

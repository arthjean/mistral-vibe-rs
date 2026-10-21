//! Semantic projection of canonical history into reference transcript regions.
//!
//! The reference renders history by meaning: every effect carries a call
//! display (`verb`, `message`, `suffix`) and, once settled, a result display
//! whose `success` flag is authoritative. This module reproduces that contract
//! as pure functions over an already projected [`TranscriptEntry`], so the
//! renderer never re-derives status from the generation flag and a failed,
//! cancelled, or skipped effect can never be presented as a success.

use vibe_app_server::client::{
    EffectDetail, EffectResultDisplay, HookNotice, HookScope, HookSeverity, NoticeDetail,
    PublicEffectState, PublicHistoryEntry, PublicNoticeLevel, ToolEffectKind,
};

use super::state::{EntrySource, EntryStatus, TranscriptEntry, TranscriptKind};
use bodies::{detail_lines, effect_body, text_lines};

mod bodies;

/// The kind the app server published with the effect. Its verbs, subject and
/// status text arrive on the entry; what stays here is how the terminal lays
/// the result out.
pub type EffectKind = ToolEffectKind;

/// How the terminal folds and groups an effect, which is presentation the wire
/// contract does not carry.
pub trait EffectLayout {
    /// Reference `EFFECT_WIDGETS[...].result.COLLAPSIBLE`: diff-shaped and
    /// question results always render in full, everything else folds into its
    /// header until the operator expands it.
    fn collapses(&self) -> bool;
}

impl EffectLayout for EffectKind {
    fn collapses(&self) -> bool {
        !matches!(self, Self::FileEdit | Self::FileWrite | Self::UserQuestion)
    }
}

/// Reference `MANUAL_SHELL_TOOL_NAME`: the tool name an operator's own `!`
/// command is published under, which is what tells it apart from a model's
/// shell call of the same kind.
pub const MANUAL_SHELL_TOOL_NAME: &str = "shell";

/// The published effect an entry projects, when the entry is one and its
/// canonical form was restored with it.
///
/// An entry replayed from saved history carries no canonical effect, which is
/// what [`restored_effect_region`] presents instead.
fn published_effect(entry: &TranscriptEntry) -> Option<(&EffectDetail, &PublicEffectState)> {
    match entry.source.server()? {
        PublicHistoryEntry::Effect { detail, state, .. } => Some((detail, state)),
        _ => None,
    }
}

/// Reference `IndicatorState` plus the in-flight spinner it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indicator {
    Running,
    Success,
    Error,
    Muted,
}

impl Indicator {
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Running => "⠋",
            Self::Success => "✓",
            Self::Error => "✕",
            Self::Muted => "□",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyStyle {
    Plain,
    Added,
    Removed,
    Warning,
    Error,
    Muted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyLine {
    pub text: String,
    pub style: BodyStyle,
    /// Where activating the line's label leads. Reference `link_content`
    /// attaches the target to a label that does not spell it out.
    pub link: Option<String>,
}

impl BodyLine {
    fn plain(text: impl Into<String>) -> Self {
        Self::styled(text, BodyStyle::Plain)
    }

    fn styled(text: impl Into<String>, style: BodyStyle) -> Self {
        Self {
            text: text.into(),
            style,
            link: None,
        }
    }
}

/// How a notice reads, which is the level the server publishes.
pub type NoticeLevel = PublicNoticeLevel;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectRegion {
    pub kind: EffectKind,
    pub status: EntryStatus,
    pub indicator: Indicator,
    pub verb: String,
    pub message: String,
    pub suffix: String,
    /// Reference default: collapsible results show only their header.
    pub collapsed_by_default: bool,
    pub body: Vec<BodyLine>,
    /// Reference `_mount_manual_shell_output`: what a failed or interrupted
    /// `!` command printed, shown above how it ended.
    pub printed: Vec<BodyLine>,
    /// Whether the body is the error a failure reported, which a result that
    /// always opens folds behind its line count.
    pub failed: bool,
}

impl EffectRegion {
    /// Reference `EffectResultDisplay.text`.
    #[must_use]
    pub fn header_text(&self) -> String {
        let head = if self.verb.is_empty() {
            self.message.clone()
        } else {
            format!("{} {}", self.verb, self.message).trim().to_owned()
        };
        if self.suffix.is_empty() {
            head
        } else {
            format!("{head} {}", self.suffix)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Region {
    UserMessage,
    AssistantMessage,
    Reasoning,
    Effect(Box<EffectRegion>),
    Callback {
        title: String,
        detail: String,
    },
    Compaction {
        message: String,
    },
    Checkpoint {
        message: String,
    },
    Hook {
        icon: &'static str,
        line: String,
    },
    Command {
        message: String,
    },
    /// Markdown a command wrote into the transcript, rendered as Markdown
    /// rather than as wrapped text.
    Document {
        message: String,
    },
    /// The command line a submission ran, which reference `SlashCommandMessage`
    /// paints as a user message under the `/` prompt character.
    SlashCommand {
        message: String,
    },
    Notice {
        level: NoticeLevel,
        message: String,
    },
    Plan,
}

/// Projects one canonical entry into the region the reference renders.
#[must_use]
pub fn region(entry: &TranscriptEntry) -> Region {
    match entry.kind {
        TranscriptKind::UserMessage => Region::UserMessage,
        TranscriptKind::AssistantMessage => Region::AssistantMessage,
        TranscriptKind::Reasoning => Region::Reasoning,
        TranscriptKind::Plan => Region::Plan,
        TranscriptKind::Effect => Region::Effect(Box::new(effect_region(entry))),
        TranscriptKind::Callback => {
            let (title, detail) = entry.text.split_once('\n').unwrap_or((&entry.text, ""));
            Region::Callback {
                title: title.to_owned(),
                detail: detail.to_owned(),
            }
        }
        TranscriptKind::Checkpoint => {
            let message = entry.text.clone();
            if checkpoint_kind(entry) == Some(COMPACTION_CHECKPOINT) {
                Region::Compaction { message }
            } else {
                Region::Checkpoint { message }
            }
        }
        TranscriptKind::Notice => notice_region(entry),
        TranscriptKind::Document => Region::Document {
            message: entry.text.clone(),
        },
        TranscriptKind::Command => Region::SlashCommand {
            message: entry.text.clone(),
        },
    }
}

/// Reference loading status: the newest unsettled effect names what the
/// runtime is doing, reasoning reads as thinking, a hook that started and has
/// not completed names itself (`_handle_hook_notice`), and anything else falls
/// back to the default generation status.
#[must_use]
pub fn activity_status(entries: &[TranscriptEntry]) -> String {
    for entry in entries.iter().rev() {
        match notice_detail(entry) {
            Some(NoticeDetail::HookStarted(hook)) => {
                return format!("Running hook {}", hook.hook_name.as_deref().unwrap_or(""))
                    .trim_end()
                    .to_owned();
            }
            Some(NoticeDetail::HookCompleted(_)) => break,
            _ => {}
        }
        if entry.status.is_terminal() {
            continue;
        }
        match region(entry) {
            Region::Effect(effect) => return effect.kind.status_text().to_owned(),
            Region::Reasoning => return "Thinking".to_owned(),
            _ => {}
        }
    }
    super::diagnostics::DEFAULT_ACTIVITY_STATUS.to_owned()
}

/// Reference `_handle_hook_notice`: of the four notices a hook run raises,
/// only a completion that has something to say is drawn, as one line of its
/// run's container; the other three only open, name and close that container.
#[must_use]
pub fn is_drawn(entry: &TranscriptEntry) -> bool {
    match notice_detail(entry) {
        Some(NoticeDetail::HookCompleted(hook)) => {
            hook.hook_name.is_some() && hook.content.as_deref().is_some_and(|text| !text.is_empty())
        }
        Some(detail) => !is_hook(detail),
        None => true,
    }
}

/// The tool call a drawn hook line belongs beside: before it for a pre-tool
/// run, after it for a post-tool run. A post-agent run has no anchor.
fn hook_anchor(entry: &TranscriptEntry) -> Option<(HookScope, &str)> {
    match notice_detail(entry)? {
        NoticeDetail::HookCompleted(hook) if hook.scope != HookScope::PostAgent => {
            Some((hook.scope, hook.tool_call_id.as_deref()?))
        }
        _ => None,
    }
}

/// The tool call an effect entry answers: the one it names, else its own
/// identifier, which the server gives the call's.
fn effect_call_id(entry: &TranscriptEntry) -> Option<&str> {
    if entry.kind != TranscriptKind::Effect {
        return None;
    }
    match entry.source.server() {
        Some(PublicHistoryEntry::Effect { tool_call_id, .. }) if !tool_call_id.is_empty() => {
            Some(tool_call_id)
        }
        _ => Some(&entry.id),
    }
}

/// Reference `_handle_hook_run_start`: a pre-tool run's container mounts
/// before the call it guards and a post-tool run's after it, or after the
/// container a previous post-tool run of the same call left there; a run whose
/// call is not on screen, and every post-agent run, mounts at the end.
#[must_use]
pub fn place_hook_lines(entries: Vec<&TranscriptEntry>) -> Vec<&TranscriptEntry> {
    let mut placed: Vec<&TranscriptEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some((scope, call_id)) = hook_anchor(entry) else {
            placed.push(entry);
            continue;
        };
        let Some(call) = placed
            .iter()
            .position(|other| effect_call_id(other) == Some(call_id))
        else {
            placed.push(entry);
            continue;
        };
        let at = if scope == HookScope::PreTool {
            call
        } else {
            let mut after = call + 1;
            while placed
                .get(after)
                .is_some_and(|other| hook_anchor(other) == Some((scope, call_id)))
            {
                after += 1;
            }
            after
        };
        placed.insert(at, entry);
    }
    placed
}

/// Reference `entry_keeps_tool_group` (`vibe/cli/textual_ui/widgets/tools.py`):
/// every effect but an operator's own `!` command, reasoning, and hook notices
/// stay inside the current tool group.
#[must_use]
pub fn keeps_tool_group(entry: &TranscriptEntry) -> bool {
    // Grouping is decided per frame for every visible entry, so it reads the
    // discriminants directly instead of projecting the whole region.
    match entry.kind {
        TranscriptKind::Reasoning => true,
        TranscriptKind::Effect => published_effect(entry)
            .is_none_or(|(detail, _)| detail.tool_name != MANUAL_SHELL_TOOL_NAME),
        TranscriptKind::Notice => matches!(notice_detail(entry), Some(detail) if is_hook(detail)),
        _ => false,
    }
}

/// The checkpoint kind the server published, which is what tells a compaction
/// from every other checkpoint.
fn checkpoint_kind(entry: &TranscriptEntry) -> Option<&str> {
    match entry.source.server()? {
        PublicHistoryEntry::Checkpoint { kind, .. } => Some(kind),
        _ => None,
    }
}

/// Reference `CompactionCheckpoint`.
const COMPACTION_CHECKPOINT: &str = "compaction";

/// Why the server wrote this notice, when the server wrote it at all.
fn notice_detail(entry: &TranscriptEntry) -> Option<&NoticeDetail> {
    match entry.source.server()? {
        PublicHistoryEntry::Notice { detail, .. } => Some(detail),
        _ => None,
    }
}

/// The hook run a notice reports, in the four shapes a hook is reported under.
const fn hook_notice(detail: &NoticeDetail) -> Option<&HookNotice> {
    match detail {
        NoticeDetail::HookRunStarted(hook)
        | NoticeDetail::HookRunCompleted(hook)
        | NoticeDetail::HookStarted(hook)
        | NoticeDetail::HookCompleted(hook) => Some(hook),
        _ => None,
    }
}

const fn is_hook(detail: &NoticeDetail) -> bool {
    hook_notice(detail).is_some()
}

fn notice_region(entry: &TranscriptEntry) -> Region {
    match notice_detail(entry) {
        Some(NoticeDetail::ScheduledLoopFired { .. }) => Region::Command {
            message: entry.text.clone(),
        },
        Some(detail) => match hook_notice(detail) {
            Some(hook) => Region::Hook {
                icon: hook_icon(hook.status),
                line: format!(
                    "[{}] {}",
                    hook.hook_name.as_deref().unwrap_or("hook"),
                    hook.content.as_deref().unwrap_or(entry.text.as_str())
                ),
            },
            None => plain_notice(entry),
        },
        None => plain_notice(entry),
    }
}

/// A notice with nothing to present beyond its own text: the level it was
/// written at is the level the server published, or the one this client chose.
fn plain_notice(entry: &TranscriptEntry) -> Region {
    let level = match &entry.source {
        EntrySource::Server(published) => match published.as_ref() {
            PublicHistoryEntry::Notice { level, .. } => *level,
            _ => NoticeLevel::Info,
        },
        EntrySource::Notice { level, .. } => *level,
        EntrySource::Restored => NoticeLevel::Info,
    };
    Region::Notice {
        level,
        message: entry.text.clone(),
    }
}

/// Presents an effect whose canonical state was not restored with it: the
/// first line is the header, the rest is the recorded output, and the entry's
/// own status stays authoritative.
fn restored_effect_region(entry: &TranscriptEntry) -> EffectRegion {
    let (title, output) = entry.text.split_once('\n').unwrap_or((&entry.text, ""));
    EffectRegion {
        kind: EffectKind::Tool,
        status: entry.status,
        indicator: match entry.status {
            EntryStatus::Failed => Indicator::Error,
            EntryStatus::Cancelled | EntryStatus::Skipped => Indicator::Muted,
            EntryStatus::Completed => Indicator::Success,
            EntryStatus::Pending | EntryStatus::Streaming | EntryStatus::Blocked => {
                Indicator::Running
            }
        },
        verb: String::new(),
        message: title.to_owned(),
        suffix: String::new(),
        collapsed_by_default: EffectKind::Tool.collapses(),
        body: text_lines(output),
        printed: Vec::new(),
        failed: false,
    }
}

/// Reference `_HOOK_SEVERITY_ICONS`, defaulting to the warning icon.
const fn hook_icon(severity: Option<HookSeverity>) -> &'static str {
    match severity {
        Some(HookSeverity::Ok) => "✓",
        Some(HookSeverity::Error) => "✗",
        Some(HookSeverity::Warning) | None => "⚠",
    }
}

fn effect_region(entry: &TranscriptEntry) -> EffectRegion {
    // An entry restored from saved history carries no canonical effect: its own
    // settled status and text are then the only truth about it.
    let Some((detail, state)) = published_effect(entry) else {
        return restored_effect_region(entry);
    };
    let status = EntryStatus::of_effect(state);
    // The call and result displays are published with the entry, so the header
    // a reference client renders and the one this terminal renders are the same
    // strings rather than two derivations of the same arguments.
    let settled = settled_display(state);
    // Reference `_failed_header_display`: a failed call keeps the header its
    // call display settles to, whatever message the failure carries.
    let failed_header = match (state, detail.display.settled_message.as_ref()) {
        (PublicEffectState::Failed { .. }, Some(settled_message)) => Some((
            detail.display.settled_verb.clone(),
            settled_message.clone(),
            detail.display.suffix.clone(),
        )),
        _ => None,
    };
    let (verb, message, suffix) = failed_header.unwrap_or_else(|| {
        settled.map_or_else(
            || {
                (
                    detail.display.verb.clone(),
                    detail.display.subject().to_owned(),
                    detail.display.suffix.clone(),
                )
            },
            |display| {
                (
                    display.verb.clone(),
                    display.message.clone(),
                    display.suffix.clone(),
                )
            },
        )
    });
    EffectRegion {
        kind: detail.kind,
        status,
        indicator: indicator(status, settled),
        verb,
        message,
        suffix,
        // Reference `_result_is_collapsible`: an operator's own `!` command
        // opens like a diff does, since its output is the point.
        collapsed_by_default: detail.kind.collapses() && detail.tool_name != MANUAL_SHELL_TOOL_NAME,
        body: effect_body(detail.kind, state, settled),
        printed: match state {
            PublicEffectState::Failed { output_text, .. }
            | PublicEffectState::Cancelled { output_text, .. }
                if detail.tool_name == MANUAL_SHELL_TOOL_NAME =>
            {
                detail_lines(output_text)
            }
            _ => Vec::new(),
        },
        failed: matches!(state, PublicEffectState::Failed { .. }),
    }
}

/// The result display a settled effect published. A cancellation that produced
/// no result is the one settled state the reference lets publish none.
const fn settled_display(state: &PublicEffectState) -> Option<&EffectResultDisplay> {
    match state {
        PublicEffectState::Completed { display, .. }
        | PublicEffectState::Failed { display, .. }
        | PublicEffectState::Skipped { display, .. } => Some(display),
        PublicEffectState::Cancelled { display, .. } => display.as_ref(),
        PublicEffectState::Pending
        | PublicEffectState::Running { .. }
        | PublicEffectState::Blocked { .. } => None,
    }
}

/// The output a published effect has streamed so far, while it still runs.
/// Reference `ToolCallMessage.set_stream_message` shows what each update
/// appends to it, which is why the transcript state diffs successive values.
#[must_use]
pub fn running_output(entry: &TranscriptEntry) -> Option<&str> {
    match published_effect(entry)?.1 {
        PublicEffectState::Running { output_text }
        | PublicEffectState::Blocked { output_text, .. } => Some(output_text),
        _ => None,
    }
}

fn indicator(status: EntryStatus, settled: Option<&EffectResultDisplay>) -> Indicator {
    match status {
        EntryStatus::Pending | EntryStatus::Streaming | EntryStatus::Blocked => Indicator::Running,
        EntryStatus::Cancelled | EntryStatus::Skipped => Indicator::Muted,
        EntryStatus::Failed => Indicator::Error,
        EntryStatus::Completed => {
            if settled.is_some_and(|display| display.success) {
                Indicator::Success
            } else {
                Indicator::Error
            }
        }
    }
}

#[cfg(test)]
#[path = "transcript/transcript_tests.rs"]
mod transcript_tests;

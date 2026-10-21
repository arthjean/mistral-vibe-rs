//! Every event this port raises, with the property keys the reference's own
//! senders put on the wire.
//!
//! Reference `vibe/core/telemetry/send.py:243-473` publishes fifteen named
//! senders, and five satellite modules raise the rest from their own state:
//! `vibe/cli/textual_ui/app.py` the startup and interaction events,
//! `vibe/cli/voice_manager/` the four transcription events,
//! `vibe/cli/narrator_manager/` the three read-aloud ones, and
//! `vibe/core/teleport/telemetry.py` with
//! `vibe/core/vibe_code_project/telemetry.py` the teleport and project-picker
//! payloads. A sender decides its payload where it is called, so every record
//! here carries what its call site resolved rather than resolving anything
//! itself.
//!
//! The payloads live one layer below every adapter because the differential
//! replay measures them: `telemetry_parity_tests` drives each record with the
//! inputs the capture drove the reference with and compares the key set and the
//! value types against `crates/vibe-core/tests/telemetry/corpus.json`. A payload
//! built in `vibe-cli` would be unmeasurable.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use super::{
    LaunchContext, TELEMETRY_CALL_SOURCE, TelemetryAttributes, TelemetryCallType, TelemetryError,
    TelemetryEvent, TelemetryField, VERSION,
};

mod project;
mod teleport;

pub use project::{
    ProjectPicker, ProjectSelectionSource, RemoteProjectOutcome, multi_repo_match_count,
};
pub use teleport::{
    TeleportCompleted, TeleportContextSummaryStatus, TeleportFailed, TeleportFailureStage,
    TeleportProgress, TeleportTracker, teleport_early_failure,
};

// --------------------------------------------------------------------------
// The closed vocabularies a payload draws its values from
// --------------------------------------------------------------------------

/// Reference `ToolExecutionResponse`, the verdict half of a `ToolDecision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryToolVerdict {
    Skip,
    Execute,
}

impl TelemetryToolVerdict {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Execute => "execute",
        }
    }
}

/// Reference `ToolPermission`, the approval half of a `ToolDecision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryApprovalType {
    Always,
    Never,
    Ask,
}

impl TelemetryApprovalType {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Never => "never",
            Self::Ask => "ask",
        }
    }
}

/// Reference `ApprovalSource`: what settled the permission gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryApprovalSource {
    Config,
    Smart,
    User,
    Bypass,
    Never,
}

impl TelemetryApprovalSource {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Smart => "smart",
            Self::User => "user",
            Self::Bypass => "bypass",
            Self::Never => "never",
        }
    }
}

/// What one tool call ended as. Reference `_handle_tool_response`'s three
/// statuses: a call the operator declined is `skipped`, never `cancelled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryToolStatus {
    Success,
    Failure,
    Skipped,
}

impl TelemetryToolStatus {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Skipped => "skipped",
        }
    }
}

/// Reference `send_slash_command_used`'s second argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryCommandKind {
    Builtin,
    Skill,
}

impl TelemetryCommandKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Skill => "skill",
        }
    }
}

/// Why the narrator was asked to speak. The reference passes one literal at its
/// only call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadAloudTrigger {
    AutoplayNextMessage,
}

impl ReadAloudTrigger {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AutoplayNextMessage => "autoplay_next_message",
        }
    }
}

/// How one read-aloud session ended. Reference `_on_read_aloud_ended`'s three
/// call sites, whose cancelled spelling is the American one it writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadAloudStatus {
    Completed,
    Canceled,
    Error,
}

impl ReadAloudStatus {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Canceled => "canceled",
            Self::Error => "error",
        }
    }
}

// --------------------------------------------------------------------------
// The payloads that take more than a field or two
// --------------------------------------------------------------------------

/// Reference `send_new_session`: the session census plus the launch context's
/// own fields, with `unknown` standing in for an absent entrypoint and the
/// client fields carried as null rather than dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub has_agents_md: bool,
    pub nb_skills: u64,
    pub nb_mcp_servers: u64,
    pub nb_models: u64,
}

/// Reference `AgentEntrypoint`'s fallback where no launch context exists.
const UNKNOWN_ENTRYPOINT: &str = "unknown";

/// Reference `_send_startup_telemetry_once`: three durations, each null when
/// its measurement never happened, and how the launch was asked to start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Startup {
    pub first_frame_duration_ms: Option<u64>,
    pub agent_ready_duration_ms: Option<u64>,
    pub session_init_duration_ms: Option<u64>,
    pub has_initial_prompt: bool,
    pub teleport_on_start: bool,
    pub show_resume_picker: bool,
    pub is_resuming_session: bool,
    pub prompt_for_workspace_trust: bool,
    /// Whether the bytecode cache was rebuilt by this launch. A compiled
    /// binary has none, which is the reference's own null for a frozen build.
    pub is_cold_start: Option<bool>,
    /// The `config/read` answer's `harness_selection_source`.
    pub harness_selection_source: Option<String>,
}

/// The session a compaction ran in, which both compaction events name
/// explicitly. Reference `send_auto_compact_triggered` and
/// `send_compaction_failed` write the parent beside the session whenever a
/// session is passed, as null for a session that has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactedSession {
    pub parent_session_id: Option<String>,
}

impl CompactedSession {
    fn report(
        session: Option<&Self>,
        attributes: &mut TelemetryAttributes,
    ) -> Result<(), TelemetryError> {
        if let Some(session) = session {
            attributes.optional_label(
                TelemetryField::ParentSessionId,
                session.parent_session_id.clone(),
            )?;
        }
        Ok(())
    }
}

/// Reference `AdminConfigOutcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminConfigOutcome {
    Applied,
    Disabled,
    NoApiKey,
    FetchFailed,
    ParseFailed,
    ApplyFailed,
}

impl AdminConfigOutcome {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Disabled => "disabled",
            Self::NoApiKey => "no_api_key",
            Self::FetchFailed => "fetch_failed",
            Self::ParseFailed => "parse_failed",
            Self::ApplyFailed => "apply_failed",
        }
    }
}

/// Reference `send_request_sent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestSent {
    pub model: String,
    pub nb_context_chars: u64,
    pub nb_context_messages: u64,
    pub nb_prompt_chars: u64,
    pub call_type: TelemetryCallType,
    pub message_id: Option<String>,
    /// Already gated on the provider's image support, as
    /// [`super::attachment_counts`] gates it. An empty map travels as an empty
    /// object, which is what the reference's comprehension leaves behind.
    pub attachment_counts: BTreeMap<String, u64>,
}

/// Reference `send_tool_call_finished`: eleven fields, three of them derived
/// from the call's own arguments and its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallFinished {
    pub tool_name: String,
    pub status: TelemetryToolStatus,
    pub decision: Option<ToolDecision>,
    pub agent_profile_name: String,
    pub model: String,
    pub nb_files_created: u64,
    pub nb_files_modified: u64,
    pub file_extension: Option<String>,
    pub message_id: Option<String>,
    /// Omitted entirely when neither the result nor the request named a mode,
    /// which is the reference's `if bash_background is not None`.
    pub bash_background: Option<bool>,
}

/// Reference `ToolDecision`, without the operator feedback no event carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolDecision {
    pub verdict: TelemetryToolVerdict,
    pub approval_type: TelemetryApprovalType,
    /// Null on the wire when a gate settled the call without naming a source.
    pub approval_source: Option<TelemetryApprovalSource>,
}

/// One answered tool call, as the event stream reports it. Reference
/// `_handle_tool_response`'s arguments.
pub struct ToolCallReport<'a> {
    pub tool_name: &'a str,
    pub status: TelemetryToolStatus,
    /// The object the model called with, and the typed result the tool
    /// answered, both as they reach the event stream.
    pub arguments: &'a Value,
    pub result: Option<&'a Value>,
    pub decision: Option<ToolDecision>,
    pub agent_profile_name: &'a str,
    pub model: &'a str,
    pub message_id: Option<String>,
}

impl ToolCallFinished {
    /// Reference `_calculate_file_metrics` and `_extract_bash_background`,
    /// applied to the call this port observed.
    ///
    /// Only a successful call that produced a result reports file metrics, and
    /// the result's own mode wins over the requested one because a synchronous
    /// command that soft-timed out becomes a background one.
    #[must_use]
    pub fn new(report: ToolCallReport<'_>) -> Self {
        let (nb_files_created, nb_files_modified, file_extension) = file_metrics(
            report.tool_name,
            report.status,
            report.arguments,
            report.result,
        );
        Self {
            tool_name: report.tool_name.to_owned(),
            status: report.status,
            decision: report.decision,
            agent_profile_name: report.agent_profile_name.to_owned(),
            model: report.model.to_owned(),
            nb_files_created,
            nb_files_modified,
            file_extension,
            message_id: report.message_id,
            bash_background: bash_background(report.arguments, report.result),
        }
    }
}

/// Reference `_calculate_file_metrics`: only `write_file`, `edit` and
/// `read_file` report anything, and only on a success that carries a result.
fn file_metrics(
    tool_name: &str,
    status: TelemetryToolStatus,
    arguments: &Value,
    result: Option<&Value>,
) -> (u64, u64, Option<String>) {
    if status != TelemetryToolStatus::Success || result.is_none() {
        return (0, 0, None);
    }
    let extension = || file_extension(arguments.get("file_path").and_then(Value::as_str));
    match tool_name {
        "write_file" => (1, 0, extension()),
        "edit" => (0, 1, extension()),
        "read_file" => (0, 0, extension()),
        _ => (0, 0, None),
    }
}

/// Reference `_extract_file_extension`: the lowercased suffix of the path,
/// absent when the path names none.
fn file_extension(path: Option<&str>) -> Option<String> {
    let path = path?;
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() || extension.is_empty() {
        return None;
    }
    Some(format!(".{}", extension.to_ascii_lowercase()))
}

/// Reference `_extract_bash_background`: the result reports the actual mode and
/// the request the intended one, and neither being a boolean omits the key.
fn bash_background(arguments: &Value, result: Option<&Value>) -> Option<bool> {
    result
        .and_then(|result| result.get("background"))
        .and_then(Value::as_bool)
        .or_else(|| arguments.get("background").and_then(Value::as_bool))
}

/// Reference `send_at_mention_inserted`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtMentionInserted {
    pub nb_mentions: u64,
    pub context_types: BTreeMap<String, u64>,
    /// Null rather than an empty object when no mention named a file, which is
    /// the reference's `dict | None`.
    pub file_extensions: Option<BTreeMap<String, u64>>,
    pub message_id: Option<String>,
}

// --------------------------------------------------------------------------
// One record
// --------------------------------------------------------------------------

/// One event this port raises, as its call site decided it.
///
/// A record is data: it names the event and carries the values its payload
/// reports, and [`TelemetryRecord::attributes`] is the only place a wire key is
/// spelled. Every event name [`TelemetryEvent`] publishes is reachable through
/// exactly one variant here, which is what keeps a declared name from shipping
/// without a producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryRecord {
    NewSession(NewSession),
    SessionClosed,
    Ready {
        init_duration_ms: u64,
    },
    Startup(Startup),
    /// Reference `send_admin_config_applied`: the count travels only with the
    /// keys an applied layer enforced, and the error flag only with an error.
    AdminConfigApplied {
        outcome: AdminConfigOutcome,
        nb_enforced_fields: Option<u64>,
        has_error: bool,
    },
    RequestSent(RequestSent),
    ToolCallFinished(ToolCallFinished),
    AtMentionInserted(AtMentionInserted),
    AutoCompactTriggered {
        nb_context_tokens_before: u64,
        auto_compact_threshold: u64,
        status: &'static str,
        session: Option<CompactedSession>,
    },
    CompactionFailed {
        reason: &'static str,
        session: Option<CompactedSession>,
    },
    SlashCommandUsed {
        command: String,
        kind: TelemetryCommandKind,
    },
    UserCopiedText {
        text_length: u64,
    },
    /// Reference `_branch_session`: the conversation was forked into a new
    /// session the client did not attach to.
    SessionBranched {
        source_session_id: String,
        new_session_id: String,
    },
    UserCancelledAction {
        action: String,
        /// `slow` for an interrupt still settling after the hint delay, which
        /// the reference reports beside the immediate one.
        outcome: Option<&'static str>,
    },
    VoiceModeToggled {
        enabled: bool,
    },
    OnboardingApiKeyAdded {
        custom_domain: bool,
    },
    FeedbackSubmitted {
        rating: u64,
        model: String,
    },
    TeleportCompleted(TeleportCompleted),
    TeleportFailed(TeleportFailed),
    RemoteProjectConfigured {
        outcome: RemoteProjectOutcome,
        picker: ProjectPicker,
    },
    TranscriptionStarted {
        recording_id: String,
    },
    TranscriptionCancelled {
        recording_id: String,
        recording_duration: Duration,
    },
    TranscriptionDone {
        recording_id: String,
        transcript_length: u64,
        transcription_duration: Duration,
        recording_duration: Duration,
    },
    TranscriptionFailed {
        recording_id: String,
        /// The transcription endpoint's own message, carried as free text: it
        /// is the whole content of the event, and the reference sends it
        /// verbatim.
        message: String,
        transcription_duration: Duration,
        /// Null when the recording never stopped cleanly, rather than filled in
        /// from the transcription's own elapsed time.
        recording_duration: Option<Duration>,
    },
    ReadAloudRequested {
        read_aloud_session_id: String,
        trigger: ReadAloudTrigger,
    },
    ReadAloudPlayStarted {
        read_aloud_session_id: String,
        time_to_first_read: Duration,
    },
    ReadAloudEnded {
        read_aloud_session_id: String,
        status: ReadAloudStatus,
        error_type: Option<String>,
        elapsed: Duration,
    },
}

impl TelemetryRecord {
    #[must_use]
    pub const fn event(&self) -> TelemetryEvent {
        match self {
            Self::NewSession(_) => TelemetryEvent::NewSession,
            Self::SessionClosed => TelemetryEvent::SessionClosed,
            Self::Ready { .. } => TelemetryEvent::Ready,
            Self::Startup(_) => TelemetryEvent::Startup,
            Self::AdminConfigApplied { .. } => TelemetryEvent::AdminConfigApplied,
            Self::RequestSent(_) => TelemetryEvent::RequestSent,
            Self::ToolCallFinished(_) => TelemetryEvent::ToolCallFinished,
            Self::AtMentionInserted(_) => TelemetryEvent::AtMentionInserted,
            Self::AutoCompactTriggered { .. } => TelemetryEvent::AutoCompactTriggered,
            Self::CompactionFailed { .. } => TelemetryEvent::CompactionFailed,
            Self::SlashCommandUsed { .. } => TelemetryEvent::SlashCommandUsed,
            Self::UserCopiedText { .. } => TelemetryEvent::UserCopiedText,
            Self::SessionBranched { .. } => TelemetryEvent::SessionBranched,
            Self::UserCancelledAction { .. } => TelemetryEvent::UserCancelledAction,
            Self::VoiceModeToggled { .. } => TelemetryEvent::VoiceModeToggled,
            Self::OnboardingApiKeyAdded { .. } => TelemetryEvent::OnboardingApiKeyAdded,
            Self::FeedbackSubmitted { .. } => TelemetryEvent::FeedbackSubmitted,
            Self::TeleportCompleted(_) => TelemetryEvent::TeleportCompleted,
            Self::TeleportFailed(_) => TelemetryEvent::TeleportFailed,
            Self::RemoteProjectConfigured { .. } => TelemetryEvent::RemoteProjectConfigured,
            Self::TranscriptionStarted { .. } => TelemetryEvent::TranscriptionStarted,
            Self::TranscriptionCancelled { .. } => TelemetryEvent::TranscriptionCancelled,
            Self::TranscriptionDone { .. } => TelemetryEvent::TranscriptionDone,
            Self::TranscriptionFailed { .. } => TelemetryEvent::TranscriptionFailed,
            Self::ReadAloudRequested { .. } => TelemetryEvent::ReadAloudRequested,
            Self::ReadAloudPlayStarted { .. } => TelemetryEvent::ReadAloudPlayStarted,
            Self::ReadAloudEnded { .. } => TelemetryEvent::ReadAloudEnded,
        }
    }

    /// Reference `send_user_rating_feedback`, the only sender that passes a
    /// correlation identifier: the rating is read against the request it rates.
    #[must_use]
    pub const fn correlates_last_request(&self) -> bool {
        matches!(self, Self::FeedbackSubmitted { .. })
    }

    /// Whether this record reports the request census rather than the base one.
    ///
    /// No sender does: the reference's request-scoped fields travel in
    /// `send_request_sent`'s own payload, and `build_request_metadata` serves
    /// the surfaces that carry a census without an event.
    #[must_use]
    pub const fn call_type(&self) -> Option<TelemetryCallType> {
        None
    }

    /// The payload the reference's sender builds, as the properties the
    /// envelope merges over the census.
    ///
    /// The launch context is read for the one payload that repeats it,
    /// `vibe.new_session`, which the reference builds from the client's own
    /// context rather than from the census it merges beside.
    pub fn attributes(
        &self,
        launch: Option<&LaunchContext>,
    ) -> Result<TelemetryAttributes, TelemetryError> {
        let mut attributes = TelemetryAttributes::default();
        match self {
            Self::NewSession(session) => {
                attributes
                    .flag(TelemetryField::HasAgentsMd, session.has_agents_md)
                    .count(TelemetryField::NbSkills, session.nb_skills)
                    .count(TelemetryField::NbMcpServers, session.nb_mcp_servers)
                    .count(TelemetryField::NbModels, session.nb_models)
                    .label(
                        TelemetryField::Entrypoint,
                        launch.map_or(UNKNOWN_ENTRYPOINT, |launch| {
                            launch.agent_entrypoint.as_str()
                        }),
                    )?
                    .label(TelemetryField::HostKind, super::TELEMETRY_HOST_KIND)?
                    .label(TelemetryField::Version, VERSION)?
                    .optional_label(
                        TelemetryField::ClientName,
                        launch.map(|launch| launch.client_name.clone()),
                    )?
                    .optional_label(
                        TelemetryField::ClientVersion,
                        launch.map(|launch| launch.client_version.clone()),
                    )?
                    .optional_label(
                        TelemetryField::TerminalEmulator,
                        launch.and_then(|launch| launch.terminal_emulator.clone()),
                    )?;
            }
            Self::SessionClosed => {}
            Self::Ready { init_duration_ms } => {
                attributes.count(TelemetryField::InitDurationMs, *init_duration_ms);
            }
            Self::Startup(startup) => {
                attributes
                    .optional_count(
                        TelemetryField::FirstFrameDurationMs,
                        startup.first_frame_duration_ms,
                    )
                    .optional_count(
                        TelemetryField::AgentReadyDurationMs,
                        startup.agent_ready_duration_ms,
                    )
                    .optional_count(
                        TelemetryField::SessionInitDurationMs,
                        startup.session_init_duration_ms,
                    )
                    .flag(TelemetryField::HasInitialPrompt, startup.has_initial_prompt)
                    .flag(TelemetryField::TeleportOnStart, startup.teleport_on_start)
                    .flag(TelemetryField::ShowResumePicker, startup.show_resume_picker)
                    .flag(
                        TelemetryField::IsResumingSession,
                        startup.is_resuming_session,
                    )
                    .flag(
                        TelemetryField::PromptForWorkspaceTrust,
                        startup.prompt_for_workspace_trust,
                    )
                    .optional_flag(TelemetryField::IsColdStart, startup.is_cold_start)
                    .optional_label(
                        TelemetryField::HarnessSelectionSource,
                        startup.harness_selection_source.clone(),
                    )?;
            }
            Self::AdminConfigApplied {
                outcome,
                nb_enforced_fields,
                has_error,
            } => {
                attributes.label(TelemetryField::Outcome, outcome.label())?;
                if let Some(count) = nb_enforced_fields {
                    attributes.count(TelemetryField::NbEnforcedFields, *count);
                }
                if *has_error {
                    attributes.flag(TelemetryField::HasError, true);
                }
            }
            Self::RequestSent(request) => {
                attributes
                    .label(TelemetryField::Model, &request.model)?
                    .count(TelemetryField::NbContextChars, request.nb_context_chars)
                    .count(
                        TelemetryField::NbContextMessages,
                        request.nb_context_messages,
                    )
                    .count(TelemetryField::NbPromptChars, request.nb_prompt_chars)
                    .label(TelemetryField::CallSource, TELEMETRY_CALL_SOURCE)?
                    .label(TelemetryField::CallType, request.call_type.label())?
                    .optional_label(TelemetryField::MessageId, request.message_id.clone())?
                    .counts(TelemetryField::AttachmentCounts, &request.attachment_counts);
            }
            Self::ToolCallFinished(call) => {
                attributes
                    .label(TelemetryField::ToolName, &call.tool_name)?
                    .label(TelemetryField::Status, call.status.label())?
                    .optional_label(
                        TelemetryField::Decision,
                        call.decision
                            .map(|decision| decision.verdict.label().to_owned()),
                    )?
                    .optional_label(
                        TelemetryField::ApprovalType,
                        call.decision
                            .map(|decision| decision.approval_type.label().to_owned()),
                    )?
                    .optional_label(
                        TelemetryField::ApprovalSource,
                        call.decision
                            .and_then(|decision| decision.approval_source)
                            .map(|source| source.label().to_owned()),
                    )?
                    .label(TelemetryField::AgentProfileName, &call.agent_profile_name)?
                    .label(TelemetryField::Model, &call.model)?
                    .count(TelemetryField::NbFilesCreated, call.nb_files_created)
                    .count(TelemetryField::NbFilesModified, call.nb_files_modified)
                    .optional_label(TelemetryField::FileExtension, call.file_extension.clone())?
                    .optional_label(TelemetryField::MessageId, call.message_id.clone())?;
                if let Some(background) = call.bash_background {
                    attributes.flag(TelemetryField::BashBackground, background);
                }
            }
            Self::AtMentionInserted(mentions) => {
                attributes
                    .count(TelemetryField::NbMentions, mentions.nb_mentions)
                    .counts(TelemetryField::ContextTypes, &mentions.context_types)
                    .optional_counts(
                        TelemetryField::FileExtensions,
                        mentions.file_extensions.as_ref(),
                    )
                    .optional_label(TelemetryField::MessageId, mentions.message_id.clone())?;
            }
            Self::AutoCompactTriggered {
                nb_context_tokens_before,
                auto_compact_threshold,
                status,
                session,
            } => {
                attributes
                    .count(
                        TelemetryField::NbContextTokensBefore,
                        *nb_context_tokens_before,
                    )
                    .count(
                        TelemetryField::AutoCompactThreshold,
                        *auto_compact_threshold,
                    )
                    .label(TelemetryField::Status, *status)?;
                CompactedSession::report(session.as_ref(), &mut attributes)?;
            }
            Self::CompactionFailed { reason, session } => {
                attributes.label(TelemetryField::Reason, *reason)?;
                CompactedSession::report(session.as_ref(), &mut attributes)?;
            }
            Self::SlashCommandUsed { command, kind } => {
                attributes
                    // Reference `command.lstrip("/")`.
                    .label(TelemetryField::Command, command.trim_start_matches('/'))?
                    .label(TelemetryField::CommandType, kind.label())?;
            }
            Self::UserCopiedText { text_length } => {
                attributes.count(TelemetryField::TextLength, *text_length);
            }
            Self::SessionBranched {
                source_session_id,
                new_session_id,
            } => {
                attributes
                    .label(TelemetryField::SourceSessionId, source_session_id)?
                    .label(TelemetryField::NewSessionId, new_session_id)?;
            }
            Self::UserCancelledAction { action, outcome } => {
                attributes.label(TelemetryField::Action, action)?;
                if let Some(outcome) = outcome {
                    attributes.label(TelemetryField::Outcome, *outcome)?;
                }
            }
            Self::VoiceModeToggled { enabled } => {
                attributes.flag(TelemetryField::Enabled, *enabled);
            }
            Self::OnboardingApiKeyAdded { custom_domain } => {
                attributes
                    .label(TelemetryField::Version, VERSION)?
                    .flag(TelemetryField::CustomDomain, *custom_domain);
            }
            Self::FeedbackSubmitted { rating, model } => {
                attributes
                    .count(TelemetryField::Rating, *rating)
                    .label(TelemetryField::Version, VERSION)?
                    .label(TelemetryField::Model, model)?;
            }
            Self::TeleportCompleted(teleport) => {
                attributes
                    .flag(TelemetryField::PushRequired, teleport.push_required)
                    .count(
                        TelemetryField::NbSessionMessages,
                        teleport.nb_session_messages,
                    )
                    .label(
                        TelemetryField::ContextSummary,
                        teleport.context_summary.label(),
                    )?
                    .optional_count(
                        TelemetryField::ContextSummaryChars,
                        teleport.context_summary_chars,
                    );
                if let Some(picker) = teleport.picker {
                    picker.write(&mut attributes)?;
                }
            }
            Self::TeleportFailed(teleport) => {
                attributes
                    .label(TelemetryField::Stage, teleport.stage.label())?
                    .label(TelemetryField::ErrorClass, &teleport.error_class)?
                    .flag(TelemetryField::PushRequired, teleport.push_required)
                    .count(
                        TelemetryField::NbSessionMessages,
                        teleport.nb_session_messages,
                    )
                    .label(
                        TelemetryField::ContextSummary,
                        teleport.context_summary.label(),
                    )?
                    .optional_count(
                        TelemetryField::ContextSummaryChars,
                        teleport.context_summary_chars,
                    );
                if let Some(kind) = teleport.failure_kind.as_deref() {
                    attributes.label(TelemetryField::FailureKind, kind)?;
                }
                if let Some(status) = teleport.http_status_code {
                    attributes.count(TelemetryField::HttpStatusCode, status);
                }
                if let Some(picker) = teleport.picker {
                    picker.write(&mut attributes)?;
                }
            }
            Self::RemoteProjectConfigured { outcome, picker } => {
                attributes.label(TelemetryField::Outcome, outcome.label())?;
                picker.write(&mut attributes)?;
            }
            // The recording identifier is the endpoint's request identifier,
            // opaque and empty for a session that failed before it opened, and
            // the reference sends it as it is.
            Self::TranscriptionStarted { recording_id } => {
                attributes.text(TelemetryField::RecordingId, recording_id);
            }
            Self::TranscriptionCancelled {
                recording_id,
                recording_duration,
            } => {
                attributes
                    .text(TelemetryField::RecordingId, recording_id)
                    .millis(TelemetryField::RecordingDurationMs, *recording_duration);
            }
            Self::TranscriptionDone {
                recording_id,
                transcript_length,
                transcription_duration,
                recording_duration,
            } => {
                attributes
                    .text(TelemetryField::RecordingId, recording_id)
                    .count(TelemetryField::TranscriptLength, *transcript_length)
                    .millis(
                        TelemetryField::TranscriptionDurationMs,
                        *transcription_duration,
                    )
                    .millis(TelemetryField::RecordingDurationMs, *recording_duration);
            }
            Self::TranscriptionFailed {
                recording_id,
                message,
                transcription_duration,
                recording_duration,
            } => {
                attributes
                    .text(TelemetryField::RecordingId, recording_id)
                    .text(TelemetryField::ErrorMessage, message)
                    .millis(
                        TelemetryField::TranscriptionDurationMs,
                        *transcription_duration,
                    )
                    .optional_millis(TelemetryField::RecordingDurationMs, *recording_duration);
            }
            Self::ReadAloudRequested {
                read_aloud_session_id,
                trigger,
            } => {
                attributes
                    .label(TelemetryField::ReadAloudSessionId, read_aloud_session_id)?
                    .label(TelemetryField::Trigger, trigger.label())?;
            }
            Self::ReadAloudPlayStarted {
                read_aloud_session_id,
                time_to_first_read,
            } => {
                attributes
                    .label(TelemetryField::ReadAloudSessionId, read_aloud_session_id)?
                    .seconds(TelemetryField::TimeToFirstReadS, *time_to_first_read)
                    // The reference carries the key and has no speed selection
                    // to put in it at this pin.
                    .null(TelemetryField::SpeedSelection);
            }
            Self::ReadAloudEnded {
                read_aloud_session_id,
                status,
                error_type,
                elapsed,
            } => {
                attributes
                    .label(TelemetryField::ReadAloudSessionId, read_aloud_session_id)?
                    .label(TelemetryField::Status, status.label())?
                    .optional_label(TelemetryField::ErrorType, error_type.clone())?
                    .null(TelemetryField::SpeedSelection)
                    .seconds(TelemetryField::ElapsedSeconds, *elapsed);
            }
        }
        Ok(attributes)
    }
}

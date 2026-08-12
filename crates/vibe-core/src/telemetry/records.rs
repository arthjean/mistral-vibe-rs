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

/// Reference `TeleportFailureStage`, in its declaration order: the tracker
/// walks it forward as each yield event arrives, so the last stage entered is
/// the one a failure is attributed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeleportFailureStage {
    NoHistory,
    Ineligible,
    ContextSummary,
    GitCheck,
    Push,
    WorkflowStart,
    Cancelled,
}

impl TeleportFailureStage {
    /// The vocabulary in declaration order, which is the order the corpus
    /// records it in.
    pub const ALL: [Self; 7] = [
        Self::NoHistory,
        Self::Ineligible,
        Self::ContextSummary,
        Self::GitCheck,
        Self::Push,
        Self::WorkflowStart,
        Self::Cancelled,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoHistory => "no_history",
            Self::Ineligible => "ineligible",
            Self::ContextSummary => "context_summary",
            Self::GitCheck => "git_check",
            Self::Push => "push",
            Self::WorkflowStart => "workflow_start",
            // The reference emits the British spelling for this stage, as it
            // does for the ACP stop reason and `TodoStatus`, so it stays.
            Self::Cancelled => "cancelled",
        }
    }
}

/// Reference `TeleportContextSummaryStatus`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TeleportContextSummaryStatus {
    #[default]
    Skipped,
    Generated,
    Failed,
}

impl TeleportContextSummaryStatus {
    pub const ALL: [Self; 3] = [Self::Skipped, Self::Generated, Self::Failed];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::Generated => "generated",
            Self::Failed => "failed",
        }
    }
}

/// Reference `ProjectSelectionSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSelectionSource {
    SavedLink,
    SelectedExisting,
    MatchedProject,
    CreatedProject,
    Cancelled,
}

impl ProjectSelectionSource {
    pub const ALL: [Self; 5] = [
        Self::SavedLink,
        Self::SelectedExisting,
        Self::MatchedProject,
        Self::CreatedProject,
        Self::Cancelled,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SavedLink => "saved_link",
            Self::SelectedExisting => "selected_existing",
            Self::MatchedProject => "matched_project",
            Self::CreatedProject => "created_project",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Reference `RemoteProjectOutcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteProjectOutcome {
    Configured,
    Created,
    Unlinked,
    Cancelled,
}

impl RemoteProjectOutcome {
    pub const ALL: [Self; 4] = [
        Self::Configured,
        Self::Created,
        Self::Unlinked,
        Self::Cancelled,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Created => "created",
            Self::Unlinked => "unlinked",
            Self::Cancelled => "cancelled",
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
/// its measurement never happened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Startup {
    pub first_frame_duration_ms: Option<u64>,
    pub agent_ready_duration_ms: Option<u64>,
    pub session_init_duration_ms: Option<u64>,
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

/// Reference `ProjectPickerTelemetryPayload`: six optional keys merged into the
/// teleport and remote-project payloads, each absent when the picker never
/// answered it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectPicker {
    pub shown: bool,
    pub selection_source: Option<ProjectSelectionSource>,
    pub candidate_count_loaded: Option<u64>,
    pub multi_repo_match_count: Option<u64>,
    pub saved_project_link_cleared: Option<bool>,
    pub repo_remote_changed: Option<bool>,
}

impl ProjectPicker {
    /// Reference `build_project_picker_telemetry`: a picker that was never
    /// shown reports only that.
    #[must_use]
    pub const fn hidden() -> Self {
        Self {
            shown: false,
            selection_source: None,
            candidate_count_loaded: None,
            multi_repo_match_count: None,
            saved_project_link_cleared: None,
            repo_remote_changed: None,
        }
    }

    fn write(self, attributes: &mut TelemetryAttributes) -> Result<(), TelemetryError> {
        attributes.flag(TelemetryField::ProjectPickerShown, self.shown);
        if let Some(source) = self.selection_source {
            attributes.label(TelemetryField::ProjectSelectionSource, source.label())?;
        }
        if let Some(count) = self.candidate_count_loaded {
            attributes.count(TelemetryField::ProjectCandidateCountLoaded, count);
        }
        if let Some(count) = self.multi_repo_match_count {
            attributes.count(TelemetryField::ProjectMultiRepoMatchCount, count);
        }
        if let Some(cleared) = self.saved_project_link_cleared {
            attributes.flag(TelemetryField::SavedProjectLinkCleared, cleared);
        }
        if let Some(changed) = self.repo_remote_changed {
            attributes.flag(TelemetryField::ProjectRepoRemoteChanged, changed);
        }
        Ok(())
    }
}

/// Reference `count_multi_repo_matches`: a project counts when it carries more
/// than one repository *and* one of them is the remote the workspace is on. A
/// workspace on no remote counts nothing, as the reference's empty-url guard
/// answers zero.
#[must_use]
pub fn multi_repo_match_count<'a>(
    projects: impl IntoIterator<Item = &'a [String]>,
    remote: &str,
) -> u64 {
    if remote.is_empty() {
        return 0;
    }
    projects
        .into_iter()
        .filter(|repositories| {
            repositories.len() > 1
                && repositories
                    .iter()
                    .any(|candidate| candidate.as_str() == remote)
        })
        .count() as u64
}

/// Reference `send_teleport_completed`, merged with the picker payload when the
/// run showed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeleportCompleted {
    pub push_required: bool,
    pub nb_session_messages: u64,
    pub context_summary: TeleportContextSummaryStatus,
    pub context_summary_chars: Option<u64>,
    pub picker: Option<ProjectPicker>,
}

/// Reference `send_teleport_failed`: the completed payload plus the stage, the
/// error class and the two optional failure details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeleportFailed {
    pub stage: TeleportFailureStage,
    pub error_class: String,
    pub push_required: bool,
    pub nb_session_messages: u64,
    pub context_summary: TeleportContextSummaryStatus,
    pub context_summary_chars: Option<u64>,
    pub failure_kind: Option<String>,
    pub http_status_code: Option<u64>,
    pub picker: Option<ProjectPicker>,
}

/// Reference `TeleportTelemetryTracker`: the stage machine one teleport run
/// walks, and the two events it ends with.
///
/// The tracker is fed the progress the run reports and answers a completed or a
/// failed record at the end. A run that succeeded never sends a failure, which
/// is the reference's own short circuit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeleportTracker {
    nb_session_messages: u64,
    stage: TeleportFailureStage,
    picker: Option<ProjectPicker>,
    push_required: bool,
    success: bool,
    error_class: Option<String>,
    failure_kind: Option<String>,
    http_status_code: Option<u64>,
    context_summary: TeleportContextSummaryStatus,
    context_summary_chars: Option<u64>,
}

/// What one teleport run reported about its progress. Reference
/// `TeleportYieldEvent`'s six variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeleportProgress {
    SummarizingContext,
    CheckingGit,
    PushRequired,
    Pushing,
    StartingWorkflow,
    Complete,
}

impl TeleportTracker {
    /// A tracker starting at `stage`, which is what an early failure before any
    /// progress is attributed to.
    #[must_use]
    pub fn new(
        nb_session_messages: u64,
        stage: TeleportFailureStage,
        picker: Option<ProjectPicker>,
    ) -> Self {
        Self {
            nb_session_messages,
            stage,
            picker,
            push_required: false,
            success: false,
            error_class: None,
            failure_kind: None,
            http_status_code: None,
            context_summary: TeleportContextSummaryStatus::Skipped,
            context_summary_chars: None,
        }
    }

    /// Reference `record_event`: a push request leaves the run at `cancelled`,
    /// because the operator has to answer it before anything else happens.
    pub fn record_progress(&mut self, progress: TeleportProgress) {
        match progress {
            TeleportProgress::SummarizingContext => {
                self.stage = TeleportFailureStage::ContextSummary;
            }
            TeleportProgress::CheckingGit => self.stage = TeleportFailureStage::GitCheck,
            TeleportProgress::PushRequired => {
                self.push_required = true;
                self.stage = TeleportFailureStage::Cancelled;
            }
            TeleportProgress::Pushing => self.stage = TeleportFailureStage::Push,
            TeleportProgress::StartingWorkflow => {
                self.stage = TeleportFailureStage::WorkflowStart;
            }
            TeleportProgress::Complete => self.success = true,
        }
    }

    /// Reference `record_service_error`: a saved-link selection whose service
    /// error is a 403 or a 404 reports the link as cleared, because that is the
    /// link the service just refused.
    pub fn record_service_error(
        &mut self,
        error_class: impl Into<String>,
        failure_kind: Option<String>,
        http_status_code: Option<u64>,
    ) {
        self.error_class = Some(error_class.into());
        self.failure_kind = failure_kind;
        self.http_status_code = http_status_code;
        if let Some(picker) = self.picker.as_mut()
            && picker.selection_source == Some(ProjectSelectionSource::SavedLink)
            && matches!(http_status_code, Some(403 | 404))
        {
            picker.saved_project_link_cleared = Some(true);
        }
    }

    /// Reference `record_context_summary_generated`.
    pub fn record_context_summary_generated(&mut self, chars: u64) {
        self.context_summary = TeleportContextSummaryStatus::Generated;
        self.context_summary_chars = Some(chars);
    }

    /// Reference `record_context_summary_failed`.
    pub fn record_context_summary_failed(&mut self) {
        self.context_summary = TeleportContextSummaryStatus::Failed;
        self.context_summary_chars = None;
        self.stage = TeleportFailureStage::ContextSummary;
    }

    /// Reference `record_cancelled`.
    pub fn record_cancelled(&mut self) {
        self.stage = TeleportFailureStage::Cancelled;
        self.error_class = Some(CANCELLED_ERROR_CLASS.to_owned());
    }

    /// Reference `record_unexpected_error`.
    pub fn record_unexpected_error(&mut self, error_class: impl Into<String>) {
        self.error_class = Some(error_class.into());
    }

    /// Reference `send_success`.
    #[must_use]
    pub fn completed(&self) -> TelemetryRecord {
        TelemetryRecord::TeleportCompleted(TeleportCompleted {
            push_required: self.push_required,
            nb_session_messages: self.nb_session_messages,
            context_summary: self.context_summary,
            context_summary_chars: self.context_summary_chars,
            picker: self.picker,
        })
    }

    /// Reference `send_failure_if_needed`: a run that succeeded, or one that
    /// never classified an error, sends nothing.
    #[must_use]
    pub fn failed(&self) -> Option<TelemetryRecord> {
        if self.success {
            return None;
        }
        let error_class = self.error_class.clone()?;
        Some(TelemetryRecord::TeleportFailed(TeleportFailed {
            stage: self.stage,
            error_class,
            push_required: self.push_required,
            nb_session_messages: self.nb_session_messages,
            context_summary: self.context_summary,
            context_summary_chars: self.context_summary_chars,
            failure_kind: self.failure_kind.clone(),
            http_status_code: self.http_status_code,
            picker: self.picker,
        }))
    }
}

/// Reference `record_cancelled`, which names the exception Python raises.
const CANCELLED_ERROR_CLASS: &str = "CancelledError";

/// Reference `send_teleport_early_failure_telemetry`: a run that failed before
/// it started reports the stage it never left, with no push and no summary.
#[must_use]
pub fn teleport_early_failure(
    stage: TeleportFailureStage,
    error_class: impl Into<String>,
    nb_session_messages: u64,
) -> TelemetryRecord {
    TelemetryRecord::TeleportFailed(TeleportFailed {
        stage,
        error_class: error_class.into(),
        push_required: false,
        nb_session_messages,
        context_summary: TeleportContextSummaryStatus::Skipped,
        context_summary_chars: None,
        failure_kind: None,
        http_status_code: None,
        picker: None,
    })
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
    RequestSent(RequestSent),
    ToolCallFinished(ToolCallFinished),
    AtMentionInserted(AtMentionInserted),
    AutoCompactTriggered {
        nb_context_tokens_before: u64,
        auto_compact_threshold: u64,
        status: &'static str,
    },
    CompactionFailed {
        reason: &'static str,
    },
    SlashCommandUsed {
        command: String,
        kind: TelemetryCommandKind,
    },
    UserCopiedText {
        text_length: u64,
    },
    UserCancelledAction {
        action: String,
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
            Self::RequestSent(_) => TelemetryEvent::RequestSent,
            Self::ToolCallFinished(_) => TelemetryEvent::ToolCallFinished,
            Self::AtMentionInserted(_) => TelemetryEvent::AtMentionInserted,
            Self::AutoCompactTriggered { .. } => TelemetryEvent::AutoCompactTriggered,
            Self::CompactionFailed { .. } => TelemetryEvent::CompactionFailed,
            Self::SlashCommandUsed { .. } => TelemetryEvent::SlashCommandUsed,
            Self::UserCopiedText { .. } => TelemetryEvent::UserCopiedText,
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
                    );
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
            }
            Self::CompactionFailed { reason } => {
                attributes.label(TelemetryField::Reason, *reason)?;
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
            Self::UserCancelledAction { action } => {
                attributes.label(TelemetryField::Action, action)?;
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
            Self::TranscriptionStarted { recording_id } => {
                attributes.label(TelemetryField::RecordingId, recording_id)?;
            }
            Self::TranscriptionCancelled {
                recording_id,
                recording_duration,
            } => {
                attributes
                    .label(TelemetryField::RecordingId, recording_id)?
                    .millis(TelemetryField::RecordingDurationMs, *recording_duration);
            }
            Self::TranscriptionDone {
                recording_id,
                transcript_length,
                transcription_duration,
                recording_duration,
            } => {
                attributes
                    .label(TelemetryField::RecordingId, recording_id)?
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
                    .label(TelemetryField::RecordingId, recording_id)?
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

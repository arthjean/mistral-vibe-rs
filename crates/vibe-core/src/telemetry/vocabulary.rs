//! The published names: which events exist, which fields they carry, and what
//! an attribute is worth.
//!
//! This is the half of the surface a parity measurement reads. An event's name,
//! a field's name and the JSON shape a value takes are all contracts with the
//! collector, so they are declared as enums rather than composed at each call
//! site: a field the port spells differently is a compile error here and a
//! silent divergence anywhere else.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{TelemetryError, validate_safe_label};

/// Every event name this port publishes.
///
/// The reference raises 26 across its client and its five satellite emitters.
/// This vocabulary carries 25 of them: `vibe.admin_config_applied` reports on
/// the org-managed configuration layer, which no part of this port fetches or
/// composes, and the accepted-divergence table of `docs/parity.md` records that
/// rather than declaring a name nothing can raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryEvent {
    NewSession,
    SessionClosed,
    Ready,
    Startup,
    RequestSent,
    ToolCallFinished,
    AtMentionInserted,
    AutoCompactTriggered,
    CompactionFailed,
    SlashCommandUsed,
    UserCopiedText,
    UserCancelledAction,
    VoiceModeToggled,
    OnboardingApiKeyAdded,
    TeleportCompleted,
    TeleportFailed,
    FeedbackSubmitted,
    RemoteProjectConfigured,
    TranscriptionStarted,
    TranscriptionCancelled,
    TranscriptionDone,
    TranscriptionFailed,
    ReadAloudRequested,
    ReadAloudPlayStarted,
    ReadAloudEnded,
}

impl TelemetryEvent {
    /// Every name this port publishes, which is what the replay measures the
    /// vocabulary against.
    pub const ALL: [Self; 25] = [
        Self::NewSession,
        Self::SessionClosed,
        Self::Ready,
        Self::Startup,
        Self::RequestSent,
        Self::ToolCallFinished,
        Self::AtMentionInserted,
        Self::AutoCompactTriggered,
        Self::CompactionFailed,
        Self::SlashCommandUsed,
        Self::UserCopiedText,
        Self::UserCancelledAction,
        Self::VoiceModeToggled,
        Self::OnboardingApiKeyAdded,
        Self::TeleportCompleted,
        Self::TeleportFailed,
        Self::FeedbackSubmitted,
        Self::RemoteProjectConfigured,
        Self::TranscriptionStarted,
        Self::TranscriptionCancelled,
        Self::TranscriptionDone,
        Self::TranscriptionFailed,
        Self::ReadAloudRequested,
        Self::ReadAloudPlayStarted,
        Self::ReadAloudEnded,
    ];

    #[must_use]
    pub const fn event_name(self) -> &'static str {
        match self {
            Self::NewSession => "vibe.new_session",
            Self::SessionClosed => "vibe.session_closed",
            Self::Ready => "vibe.ready",
            Self::Startup => "vibe.startup",
            Self::RequestSent => "vibe.request_sent",
            Self::ToolCallFinished => "vibe.tool_call_finished",
            Self::AtMentionInserted => "vibe.at_mention_inserted",
            Self::AutoCompactTriggered => "vibe.auto_compact_triggered",
            Self::CompactionFailed => "vibe.compaction_failed",
            Self::SlashCommandUsed => "vibe.slash_command_used",
            Self::UserCopiedText => "vibe.user_copied_text",
            Self::UserCancelledAction => "vibe.user_cancelled_action",
            Self::VoiceModeToggled => "vibe.voice_mode_toggled",
            Self::OnboardingApiKeyAdded => "vibe.onboarding_api_key_added",
            Self::TeleportCompleted => "vibe.teleport_completed",
            Self::TeleportFailed => "vibe.teleport_failed",
            Self::FeedbackSubmitted => "vibe.user_rating_feedback",
            Self::RemoteProjectConfigured => "vibe.remote_project_configured",
            Self::TranscriptionStarted => "vibe.audio.transcription.start",
            Self::TranscriptionCancelled => "vibe.audio.transcription.cancel_recording",
            Self::TranscriptionDone => "vibe.audio.transcription.done",
            Self::TranscriptionFailed => "vibe.audio.transcription.error",
            Self::ReadAloudRequested => "vibe.read_aloud.requested",
            Self::ReadAloudPlayStarted => "vibe.read_aloud.play_started",
            Self::ReadAloudEnded => "vibe.read_aloud.ended",
        }
    }
}

/// Every property key an event this port authors can carry, drawn from the
/// reference's own senders. A key is spelled once, here, so a payload cannot
/// invent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryField {
    Action,
    AgentProfileName,
    AgentReadyDurationMs,
    ApprovalType,
    AttachmentCounts,
    AutoCompactThreshold,
    BashBackground,
    CallSource,
    CallType,
    ClientName,
    ClientVersion,
    Command,
    CommandType,
    ContextSummary,
    ContextSummaryChars,
    ContextTypes,
    CustomDomain,
    Decision,
    ElapsedSeconds,
    Enabled,
    Entrypoint,
    ErrorClass,
    ErrorMessage,
    ErrorType,
    FailureKind,
    FileExtension,
    FileExtensions,
    FirstFrameDurationMs,
    HasAgentsMd,
    HttpStatusCode,
    InitDurationMs,
    MessageId,
    Model,
    NbContextChars,
    NbContextMessages,
    NbContextTokensBefore,
    NbFilesCreated,
    NbFilesModified,
    NbMcpServers,
    NbMentions,
    NbModels,
    NbPromptChars,
    NbSessionMessages,
    NbSkills,
    Outcome,
    ProjectCandidateCountLoaded,
    ProjectMultiRepoMatchCount,
    ProjectPickerShown,
    ProjectRepoRemoteChanged,
    ProjectSelectionSource,
    PushRequired,
    Rating,
    ReadAloudSessionId,
    Reason,
    RecordingDurationMs,
    RecordingId,
    SavedProjectLinkCleared,
    SessionInitDurationMs,
    SpeedSelection,
    Stage,
    Status,
    TerminalEmulator,
    TextLength,
    TimeToFirstReadS,
    ToolName,
    TranscriptLength,
    TranscriptionDurationMs,
    Trigger,
    Version,
}

impl TelemetryField {
    const fn key(self) -> &'static str {
        match self {
            Self::Action => "action",
            Self::AgentProfileName => "agent_profile_name",
            Self::AgentReadyDurationMs => "agent_ready_duration_ms",
            Self::ApprovalType => "approval_type",
            Self::AttachmentCounts => "attachment_counts",
            Self::AutoCompactThreshold => "auto_compact_threshold",
            Self::BashBackground => "bash_background",
            Self::CallSource => "call_source",
            Self::CallType => "call_type",
            Self::ClientName => "client_name",
            Self::ClientVersion => "client_version",
            Self::Command => "command",
            Self::CommandType => "command_type",
            Self::ContextSummary => "context_summary",
            Self::ContextSummaryChars => "context_summary_chars",
            Self::ContextTypes => "context_types",
            Self::CustomDomain => "custom_domain",
            Self::Decision => "decision",
            Self::ElapsedSeconds => "elapsed_seconds",
            Self::Enabled => "enabled",
            Self::Entrypoint => "entrypoint",
            Self::ErrorClass => "error_class",
            Self::ErrorMessage => "error_message",
            Self::ErrorType => "error_type",
            Self::FailureKind => "failure_kind",
            Self::FileExtension => "file_extension",
            Self::FileExtensions => "file_extensions",
            Self::FirstFrameDurationMs => "first_frame_duration_ms",
            Self::HasAgentsMd => "has_agents_md",
            Self::HttpStatusCode => "http_status_code",
            Self::InitDurationMs => "init_duration_ms",
            Self::MessageId => "message_id",
            Self::Model => "model",
            Self::NbContextChars => "nb_context_chars",
            Self::NbContextMessages => "nb_context_messages",
            Self::NbContextTokensBefore => "nb_context_tokens_before",
            Self::NbFilesCreated => "nb_files_created",
            Self::NbFilesModified => "nb_files_modified",
            Self::NbMcpServers => "nb_mcp_servers",
            Self::NbMentions => "nb_mentions",
            Self::NbModels => "nb_models",
            Self::NbPromptChars => "nb_prompt_chars",
            Self::NbSessionMessages => "nb_session_messages",
            Self::NbSkills => "nb_skills",
            Self::Outcome => "outcome",
            Self::ProjectCandidateCountLoaded => "project_candidate_count_loaded",
            Self::ProjectMultiRepoMatchCount => "project_multi_repo_match_count",
            Self::ProjectPickerShown => "project_picker_shown",
            Self::ProjectRepoRemoteChanged => "project_repo_remote_changed",
            Self::ProjectSelectionSource => "project_selection_source",
            Self::PushRequired => "push_required",
            Self::Rating => "rating",
            Self::ReadAloudSessionId => "read_aloud_session_id",
            Self::Reason => "reason",
            Self::RecordingDurationMs => "recording_duration_ms",
            Self::RecordingId => "recording_id",
            Self::SavedProjectLinkCleared => "saved_project_link_cleared",
            Self::SessionInitDurationMs => "session_init_duration_ms",
            Self::SpeedSelection => "speed_selection",
            Self::Stage => "stage",
            Self::Status => "status",
            Self::TerminalEmulator => "terminal_emulator",
            Self::TextLength => "text_length",
            Self::TimeToFirstReadS => "time_to_first_read_s",
            Self::ToolName => "tool_name",
            Self::TranscriptLength => "transcript_length",
            Self::TranscriptionDurationMs => "transcription_duration_ms",
            Self::Trigger => "trigger",
            Self::Version => "version",
        }
    }
}

/// The payload of one event this port authors itself.
///
/// Every label passes [`validate_safe_label`], which is the invariant that
/// survived the move to the reference's open envelope: a path, a secret-shaped
/// token or a control character has no representation here. Properties a client
/// recorded through `telemetry/record` never travel through this type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TelemetryAttributes(Map<String, Value>);

impl TelemetryAttributes {
    pub fn label(
        &mut self,
        field: TelemetryField,
        value: impl Into<String>,
    ) -> Result<&mut Self, TelemetryError> {
        let value = value.into();
        validate_safe_label(&value)?;
        self.0.insert(field.key().to_owned(), Value::String(value));
        Ok(self)
    }

    /// A label the reference carries as null rather than dropping when its
    /// source has no value: `decision`, `message_id`, `file_extension` and the
    /// read-aloud error type all travel that way.
    pub fn optional_label(
        &mut self,
        field: TelemetryField,
        value: Option<impl Into<String>>,
    ) -> Result<&mut Self, TelemetryError> {
        match value {
            Some(value) => self.label(field, value),
            None => Ok(self.null(field)),
        }
    }

    /// A value that is content rather than a label.
    ///
    /// The one payload that needs it is the transcription failure, whose whole
    /// subject is the endpoint's message; the validators exist to keep a path
    /// or a secret out of a *label*, and refusing this one would drop the event
    /// the reference sends.
    pub fn text(&mut self, field: TelemetryField, value: impl Into<String>) -> &mut Self {
        self.0
            .insert(field.key().to_owned(), Value::String(value.into()));
        self
    }

    pub fn count(&mut self, field: TelemetryField, value: u64) -> &mut Self {
        self.0
            .insert(field.key().to_owned(), Value::Number(value.into()));
        self
    }

    pub fn optional_count(&mut self, field: TelemetryField, value: Option<u64>) -> &mut Self {
        match value {
            Some(value) => self.count(field, value),
            None => self.null(field),
        }
    }

    pub fn flag(&mut self, field: TelemetryField, value: bool) -> &mut Self {
        self.0.insert(field.key().to_owned(), Value::Bool(value));
        self
    }

    /// A map of counts, as `attachment_counts`, `context_types` and
    /// `file_extensions` carry. An empty map travels as an empty object, which
    /// is what the reference's comprehension over no attachments leaves.
    pub fn counts(&mut self, field: TelemetryField, values: &BTreeMap<String, u64>) -> &mut Self {
        let object = values
            .iter()
            .map(|(key, value)| (key.clone(), Value::Number((*value).into())))
            .collect::<Map<String, Value>>();
        self.0.insert(field.key().to_owned(), Value::Object(object));
        self
    }

    pub fn optional_counts(
        &mut self,
        field: TelemetryField,
        values: Option<&BTreeMap<String, u64>>,
    ) -> &mut Self {
        match values {
            Some(values) => self.counts(field, values),
            None => self.null(field),
        }
    }

    /// A duration in fractional milliseconds, which is what the reference's
    /// `time.monotonic()` arithmetic produces for the audio events.
    pub fn millis(&mut self, field: TelemetryField, value: Duration) -> &mut Self {
        self.number(field, value.as_secs_f64() * 1_000.0)
    }

    pub fn optional_millis(&mut self, field: TelemetryField, value: Option<Duration>) -> &mut Self {
        match value {
            Some(value) => self.millis(field, value),
            None => self.null(field),
        }
    }

    /// A duration in fractional seconds, as the two read-aloud measures carry.
    pub fn seconds(&mut self, field: TelemetryField, value: Duration) -> &mut Self {
        self.number(field, value.as_secs_f64())
    }

    /// A key the reference sends with no value, as opposed to one it omits.
    pub fn null(&mut self, field: TelemetryField) -> &mut Self {
        self.0.insert(field.key().to_owned(), Value::Null);
        self
    }

    /// A finite fractional number, which is the only kind JSON has. A reading
    /// that is not finite cannot be serialized at all, so it travels as null
    /// rather than dropping the event that carries it.
    fn number(&mut self, field: TelemetryField, value: f64) -> &mut Self {
        match serde_json::Number::from_f64(value) {
            Some(number) => {
                self.0.insert(field.key().to_owned(), Value::Number(number));
            }
            None => {
                self.0.insert(field.key().to_owned(), Value::Null);
            }
        }
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn into_properties(self) -> Map<String, Value> {
        self.0
    }
}

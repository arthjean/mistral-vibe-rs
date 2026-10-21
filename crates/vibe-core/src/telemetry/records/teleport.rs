//! The stage machine one teleport run walks, and the two events it ends with.
//!
//! Reference `vibe/core/teleport/telemetry.py`: `TeleportTelemetryTracker`
//! and the early-failure sender.

use super::{ProjectPicker, ProjectSelectionSource, TelemetryRecord};

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

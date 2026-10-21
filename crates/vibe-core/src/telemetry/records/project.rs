//! The project picker's payload, which the teleport and remote-project events
//! both carry.
//!
//! Reference `ProjectPickerTelemetryPayload` (`vibe/core/telemetry/types.py`),
//! and `build_project_picker_telemetry` with `count_multi_repo_matches`
//! (`vibe/core/vibe_code_project/telemetry.py`).

use crate::telemetry::{TelemetryAttributes, TelemetryError, TelemetryField};

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

    pub(super) fn write(self, attributes: &mut TelemetryAttributes) -> Result<(), TelemetryError> {
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

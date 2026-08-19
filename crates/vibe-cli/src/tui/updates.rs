//! Update presentation pinned to the reference `vibe.cli.cli` update routes and
//! `vibe.setup.update_prompt` dialog.
//!
//! Discovery itself lives in `vibe_core::updates`; this module owns only what the
//! operator sees and the order in which the reference asks.

use vibe_core::updates::{UpdateAvailability, UpdateError, UpgradeOutcome};

/// Reference `UpdatePromptMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePromptMode {
    Startup,
    CheckUpgrade,
}

/// Reference `UpdateChoice`, in the order the dialog lays them out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateChoice {
    Update,
    Continue,
}

impl UpdateChoice {
    pub const ORDER: [Self; 2] = [Self::Update, Self::Continue];

    /// Reference `_choice_labels` and `_CONTINUE_LABELS`.
    #[must_use]
    pub const fn label(self, mode: UpdatePromptMode) -> &'static str {
        match (self, mode) {
            (Self::Update, _) => "Update now",
            (Self::Continue, UpdatePromptMode::Startup) => "Continue with current version",
            (Self::Continue, UpdatePromptMode::CheckUpgrade) => "Cancel upgrade",
        }
    }
}

/// Reference `#update-dialog-title`.
pub const UPDATE_DIALOG_TITLE: &str = "A new Vibe release is available";

/// Reference `#update-dialog-version`.
#[must_use]
pub fn version_line(current_version: &str, latest_version: &str) -> String {
    format!("{current_version} → {latest_version}")
}

/// Reference `_run_check_upgrade` success path.
#[must_use]
pub fn up_to_date_message(current_version: &str) -> String {
    format!("Vibe is already up to date ({current_version}).")
}

/// Reference `_run_check_upgrade` gateway failure.
#[must_use]
pub fn check_failed_message(reason: &str) -> String {
    format!("✗ Update check failed: {reason}")
}

/// Reference `_run_check_upgrade` cache failure.
pub const CACHE_WRITE_FAILED_MESSAGE: &str =
    "✗ Update check failed while writing the update cache.";

/// Reference `_show_update_prompt` UPDATE_FAILED branch, naming the manual
/// path this port actually publishes: the reference names its package managers,
/// and this port is installed by the installers that live in
/// [`crate::distribution::REPOSITORY_URL`].
#[must_use]
pub fn update_failed_message(current_version: &str) -> String {
    format!(
        "Vibe could not update automatically.\n  Update manually by rerunning the installer from \
         {}, or keep using the current version ({current_version}) for now.",
        crate::distribution::REPOSITORY_URL
    )
}

/// Reference `_show_update_prompt` UPDATED branch.
#[must_use]
pub fn updated_message(previous_version: &str, latest_version: &str) -> String {
    format!(
        "\u{2714} Vibe was updated from {previous_version} to {latest_version}.\n  Run vibe to \
         start using the new version."
    )
}

/// What the operator sees while the upgrade commands run. The reference shows
/// an updating dialog instead; this port has already restored the terminal by
/// then, so it names the cancellation key rather than owning the screen.
pub const UPDATING_MESSAGE: &str = "Updating Vibe. Press Ctrl+C to cancel.";

/// Reference `_check_and_show_whats_new` content source.
#[must_use]
pub fn whats_new_content() -> Option<&'static str> {
    let content = include_str!("../../whats_new.md").trim();
    (!content.is_empty()).then_some(content)
}

/// What `--check-upgrade` reports before exiting, without starting a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckUpgradeOutcome {
    UpToDate { message: String },
    Prompt { latest_version: String },
    Failed { message: String },
}

impl CheckUpgradeOutcome {
    /// Reference `_run_check_upgrade` exits non-zero only on a failed check.
    #[must_use]
    pub const fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// Reference `_run_check_upgrade`.
#[must_use]
pub fn classify_check_upgrade(
    result: Result<Option<UpdateAvailability>, UpdateError>,
    current_version: &str,
) -> CheckUpgradeOutcome {
    match result {
        Ok(None) => CheckUpgradeOutcome::UpToDate {
            message: up_to_date_message(current_version),
        },
        Ok(Some(update)) => CheckUpgradeOutcome::Prompt {
            latest_version: update.latest_version,
        },
        Err(UpdateError::Gateway(reason)) => CheckUpgradeOutcome::Failed {
            message: check_failed_message(&reason),
        },
        Err(UpdateError::CacheWrite) => CheckUpgradeOutcome::Failed {
            message: CACHE_WRITE_FAILED_MESSAGE.to_owned(),
        },
    }
}

/// Reference `UpdatePromptResult`: how the update prompt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePromptResult {
    /// Keep the installed version. Startup dismisses the offered version.
    Continue,
    /// An upgrade command exited zero, so the binary on disk is the new one.
    Updated,
    /// Every upgrade command failed, so the installed version is unchanged.
    UpdateFailed,
    Quit,
}

impl UpdatePromptResult {
    /// Reference `UpdatePromptApp`: `on_update_prompt_dialog_update_finished`
    /// answers with the two outcomes the upgrade itself produced, and
    /// `action_quit_prompt` cancels the running upgrade and answers `QUIT`, so
    /// an interrupted upgrade closes the prompt rather than dismissing the
    /// release and starting a session.
    #[must_use]
    pub const fn from_upgrade(outcome: UpgradeOutcome) -> Self {
        match outcome {
            UpgradeOutcome::Succeeded => Self::Updated,
            UpgradeOutcome::Failed => Self::UpdateFailed,
            UpgradeOutcome::Cancelled => Self::Quit,
        }
    }

    /// Reference `_show_update_prompt`: an update that ran and failed is the
    /// only non-zero exit, and only `Continue` lets a session start.
    #[must_use]
    pub const fn exit_code(self) -> Option<u8> {
        match self {
            Self::Continue => None,
            Self::Updated | Self::Quit => Some(0),
            Self::UpdateFailed => Some(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use vibe_core::updates::UpdateAvailability;

    use super::*;

    #[test]
    fn check_upgrade_reports_every_reference_outcome() {
        assert_eq!(
            classify_check_upgrade(Ok(None), "2.23.1"),
            CheckUpgradeOutcome::UpToDate {
                message: "Vibe is already up to date (2.23.1).".to_owned()
            }
        );
        assert_eq!(
            classify_check_upgrade(
                Ok(Some(UpdateAvailability {
                    latest_version: "2.24.0".to_owned(),
                    should_notify: true,
                })),
                "2.23.1"
            ),
            CheckUpgradeOutcome::Prompt {
                latest_version: "2.24.0".to_owned()
            }
        );
        let failure = classify_check_upgrade(
            Err(UpdateError::Gateway(
                "Network error while checking for updates.".to_owned(),
            )),
            "2.23.1",
        );
        assert_eq!(
            failure,
            CheckUpgradeOutcome::Failed {
                message: "✗ Update check failed: Network error while checking for updates."
                    .to_owned()
            }
        );
        assert!(failure.is_failure());
        assert_eq!(
            classify_check_upgrade(Err(UpdateError::CacheWrite), "2.23.1"),
            CheckUpgradeOutcome::Failed {
                message: CACHE_WRITE_FAILED_MESSAGE.to_owned()
            }
        );
    }

    #[test]
    fn dialog_labels_depend_on_the_prompt_mode() {
        assert_eq!(
            UpdateChoice::Continue.label(UpdatePromptMode::Startup),
            "Continue with current version"
        );
        assert_eq!(
            UpdateChoice::Continue.label(UpdatePromptMode::CheckUpgrade),
            "Cancel upgrade"
        );
        assert_eq!(
            UpdateChoice::Update.label(UpdatePromptMode::Startup),
            "Update now"
        );
        assert_eq!(version_line("2.23.1", "2.24.0"), "2.23.1 → 2.24.0");
    }

    #[test]
    fn the_prompt_publishes_four_outcomes_and_the_reference_exit_codes() {
        // Reference `UpdatePromptResult`: continue, updated, update-failed and
        // quit, with only a failed installation exiting non-zero and only
        // `Continue` letting a session start.
        assert_eq!(UpdatePromptResult::Continue.exit_code(), None);
        assert_eq!(UpdatePromptResult::Updated.exit_code(), Some(0));
        assert_eq!(UpdatePromptResult::UpdateFailed.exit_code(), Some(1));
        assert_eq!(UpdatePromptResult::Quit.exit_code(), Some(0));
        // Reference `UpdatePromptApp.action_quit_prompt`: cancelling a running
        // upgrade cancels the task and exits the prompt with `QUIT`, which is
        // neither a session start nor a dismissal of the offered release.
        assert_eq!(
            UpdatePromptResult::from_upgrade(UpgradeOutcome::Succeeded),
            UpdatePromptResult::Updated
        );
        assert_eq!(
            UpdatePromptResult::from_upgrade(UpgradeOutcome::Failed),
            UpdatePromptResult::UpdateFailed
        );
        assert_eq!(
            UpdatePromptResult::from_upgrade(UpgradeOutcome::Cancelled),
            UpdatePromptResult::Quit
        );
        assert_eq!(
            updated_message("2.23.1", "2.24.0"),
            "\u{2714} Vibe was updated from 2.23.1 to 2.24.0.\n  Run vibe to start using the new \
             version."
        );
        assert!(
            update_failed_message("2.23.1").contains(crate::distribution::REPOSITORY_URL),
            "the failed branch must name the manual path this port publishes"
        );
    }

    #[test]
    fn release_notes_are_present_and_bounded() {
        let content = whats_new_content().expect("release notes ship with the binary");
        assert!(content.starts_with("# What's new in v"));
        assert!(!content.ends_with('\n'));
    }
}

//! What a Teleport run publishes, read into the three things the terminal does
//! with it: the stage its telemetry tracker walks, the failure class it
//! reports, and the transcript line the operator sees.

use serde_json::Value;
use vibe_core::telemetry::records::TeleportProgress;

use super::runtime::InteractiveRuntime;
use super::state::EntryStatus;

pub(super) fn record_teleport_progress(event: Option<&Value>, runtime: &mut InteractiveRuntime) {
    let Some(event) = event else {
        return;
    };
    let Some(tracker) = runtime.teleport_telemetry.as_mut() else {
        return;
    };
    let kind = event
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(progress) = match kind {
        "summarizing_context" => Some(TeleportProgress::SummarizingContext),
        "checking_git" => Some(TeleportProgress::CheckingGit),
        "push_required" => Some(TeleportProgress::PushRequired),
        "pushing" => Some(TeleportProgress::Pushing),
        "starting_workflow" => Some(TeleportProgress::StartingWorkflow),
        "complete" => Some(TeleportProgress::Complete),
        _ => None,
    } {
        tracker.record_progress(progress);
    }
    let record = match kind {
        "complete" => Some(tracker.completed()),
        "cancelled" => {
            tracker.record_cancelled();
            tracker.failed()
        }
        "failed" => {
            let (code, status) = service_error_of(event);
            tracker.record_service_error(code, status.map(|_| "http".to_owned()), status);
            tracker.failed()
        }
        _ => None,
    };
    let Some(record) = record else {
        return;
    };
    runtime.teleport_telemetry = None;
    runtime.project_picker = None;
    runtime.report(&record);
}

/// Reference `record_service_error`'s two arguments, read off the failure event
/// the server published: the class the service named, and the HTTP status it
/// answered with when it answered with one. A saved-link selection the service
/// refused with a 403 or a 404 is what the status decides.
pub(super) fn service_error_of(event: &Value) -> (&str, Option<u64>) {
    let error = event.get("error");
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(TELEPORT_ERROR_CLASS);
    let status = error
        .and_then(|error| error.pointer("/details/httpStatusCode"))
        .and_then(Value::as_u64);
    (code, status)
}

/// What a failure that named no class reports as one.
const TELEPORT_ERROR_CLASS: &str = "TeleportError";

pub(super) fn teleport_event_message(
    event: Option<&Value>,
) -> Result<(String, EntryStatus), &'static str> {
    let event = event.ok_or("Teleport event omitted its payload")?;
    let kind = event
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("Teleport event omitted its kind")?;
    let (message, status) = match kind {
        "summarizing_context" => ("Summarizing context...".to_owned(), EntryStatus::Streaming),
        "checking_git" => ("Preparing workspace...".to_owned(), EntryStatus::Streaming),
        "push_required" => {
            let count = event
                .get("unpushedCount")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let branch = event
                .get("branchNotPushed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let message = if branch {
                "Teleport requires publishing the current branch.".to_owned()
            } else {
                format!(
                    "Teleport requires pushing {count} commit{}.",
                    if count == 1 { "" } else { "s" }
                )
            };
            (message, EntryStatus::Streaming)
        }
        "pushing" => ("Syncing with remote...".to_owned(), EntryStatus::Streaming),
        "starting_workflow" => ("Teleporting...".to_owned(), EntryStatus::Streaming),
        "complete" => {
            let url = event
                .get("url")
                .and_then(Value::as_str)
                .ok_or("Completed Teleport event omitted its URL")?;
            (
                format!("Teleported to Vibe Code Web: {url}"),
                EntryStatus::Completed,
            )
        }
        "failed" => {
            let message = event
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Teleport failed");
            return Ok((format!("Teleport failed: {message}"), EntryStatus::Failed));
        }
        "cancelled" => ("Teleport cancelled.".to_owned(), EntryStatus::Cancelled),
        _ => return Err("Teleport event kind is unknown"),
    };
    Ok((message, status))
}
#[cfg(test)]
mod tests {
    use serde_json::json;
    use vibe_app_server::workspace::WorkspaceService;

    use super::*;
    use crate::tui::runtime;

    /// US-011: a teleport run walks the stages its own notifications report,
    /// and the completed event carries the picker payload the run started with.
    #[test]
    fn a_teleport_run_reports_the_stage_it_reached() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let workspace = WorkspaceService::for_runtime_session_root(
            temporary.path().join(".vibe/sessions"),
            temporary.path().join("workspace"),
        );
        let mut runtime = runtime::interactive_test_runtime_with_server(
            "teleport-telemetry",
            vibe_app_server::server::AppServer::with_workspace_service(workspace),
        );
        runtime.project_picker = Some(vibe_core::telemetry::records::ProjectPicker {
            shown: true,
            selection_source: Some(
                vibe_core::telemetry::records::ProjectSelectionSource::SavedLink,
            ),
            candidate_count_loaded: Some(3),
            multi_repo_match_count: Some(1),
            saved_project_link_cleared: Some(false),
            repo_remote_changed: Some(false),
        });
        runtime.teleport_telemetry = Some(vibe_core::telemetry::records::TeleportTracker::new(
            12,
            vibe_core::telemetry::records::TeleportFailureStage::Ineligible,
            runtime.project_picker,
        ));

        for kind in ["summarizing_context", "checking_git", "pushing"] {
            record_teleport_progress(Some(&json!({"kind": kind})), &mut runtime);
        }
        let tracker = runtime
            .teleport_telemetry
            .clone()
            .expect("the run is still open");
        let failed = tracker.failed();
        assert!(
            failed.is_none(),
            "a run that classified no error reports nothing"
        );

        // The service refuses the saved link with a 403, which is what clears
        // it, and the failure is attributed to the stage the run had reached.
        record_teleport_progress(
            Some(&json!({
                "kind": "failed",
                "error": {
                    "code": "ServiceTeleportError",
                    "message": "refused",
                    "details": {"httpStatusCode": 403},
                },
            })),
            &mut runtime,
        );
        assert!(
            runtime.teleport_telemetry.is_none() && runtime.project_picker.is_none(),
            "a terminal event closes the run"
        );

        let mut tracker = vibe_core::telemetry::records::TeleportTracker::new(
            12,
            vibe_core::telemetry::records::TeleportFailureStage::Ineligible,
            None,
        );
        tracker.record_progress(vibe_core::telemetry::records::TeleportProgress::Pushing);
        tracker.record_service_error("ServiceTeleportError", Some("http".to_owned()), Some(403));
        let record = tracker.failed().expect("a classified error is a failure");
        let properties = record
            .attributes(None)
            .expect("the payload carries no unsafe label")
            .into_properties();
        assert_eq!(properties["stage"], json!("push"));
        assert_eq!(properties["http_status_code"], json!(403));
    }

    /// US-011: the failure event the server publishes is where the class and
    /// the status come from, and a failure carrying neither still names a
    /// class rather than an empty one.
    #[test]
    fn a_failure_event_answers_the_class_and_the_status_the_service_named() {
        assert_eq!(
            service_error_of(&json!({
                "kind": "failed",
                "error": {
                    "message": "refused",
                    "code": "ServiceTeleportError",
                    "details": {"httpStatusCode": 403},
                },
            })),
            ("ServiceTeleportError", Some(403))
        );
        assert_eq!(
            service_error_of(&json!({
                "kind": "failed",
                "error": {"message": "the remote went away", "details": Value::Null},
            })),
            ("TeleportError", None)
        );
    }
}

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use vibe_app_server::client::PublicDispatch;
use vibe_core::telemetry::TelemetryRecord;
use vibe_core::telemetry::records::{
    ProjectPicker, ProjectSelectionSource, RemoteProjectOutcome, TeleportFailureStage,
    TeleportTracker, multi_repo_match_count, teleport_early_failure,
};

use super::cloud_workflow::ProjectSelection;
use super::interaction::RemoteProjectAction;
use super::pickers::remote_projects_overlay;
use super::runtime::schedule_ui_call;
use super::{EntryStatus, InteractiveRuntime, TuiState, UiOperation, push_local_notice};

#[derive(Debug, Clone)]
pub(in crate::tui) enum ProjectPendingOperation {
    Open {
        working_directory: PathBuf,
        teleport: bool,
        prompt: Option<String>,
    },
    Select {
        working_directory: PathBuf,
        project_id: String,
    },
    More {
        query: String,
    },
    Create {
        working_directory: PathBuf,
        requested_name: String,
    },
    ClosePicker {
        unlink: bool,
    },
    TeleportResponse,
    TeleportStart {
        operation_id: String,
    },
}

pub(super) fn handle_project_action(
    action: RemoteProjectAction,
    working_directory: &Path,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    execute_project_command(action.into(), working_directory, runtime, state);
}

pub(super) fn handle_project_command(
    command_arguments: &str,
    working_directory: &Path,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let Some(parsed_arguments) = shlex::split(command_arguments) else {
        state.push_diagnostic("Invalid quoting in /remote-project arguments");
        return;
    };
    let command = match parsed_arguments.as_slice() {
        [] => ProjectCommand::Open,
        [open] if open == "open" => ProjectCommand::Open,
        [select, project_id] if select == "select" => ProjectCommand::Select(project_id.clone()),
        [more] if more == "more" => ProjectCommand::More,
        [create, name] if create == "create" => ProjectCommand::Create {
            name: name.clone(),
            default_branch: "main".to_owned(),
        },
        [create, name, default_branch] if create == "create" => ProjectCommand::Create {
            name: name.clone(),
            default_branch: default_branch.clone(),
        },
        [unlink] if unlink == "unlink" => ProjectCommand::Unlink,
        [cancel] if cancel == "cancel" => ProjectCommand::Cancel,
        _ => {
            state.push_diagnostic(
                "Usage: /remote-project [open|more|select <id>|create <name> [branch]|unlink|cancel]",
            );
            return;
        }
    };
    execute_project_command(command, working_directory, runtime, state);
}

#[derive(PartialEq, Eq)]
enum ProjectCommand {
    Open,
    Select(String),
    More,
    Create {
        name: String,
        default_branch: String,
    },
    Unlink,
    Cancel,
}

impl From<RemoteProjectAction> for ProjectCommand {
    fn from(action: RemoteProjectAction) -> Self {
        match action {
            RemoteProjectAction::Select { project_id } => Self::Select(project_id),
            RemoteProjectAction::Create {
                name,
                default_branch,
            } => Self::Create {
                name,
                default_branch,
            },
            RemoteProjectAction::More => Self::More,
            RemoteProjectAction::Unlink => Self::Unlink,
            RemoteProjectAction::Cancel => Self::Cancel,
        }
    }
}

fn execute_project_command(
    command: ProjectCommand,
    working_directory: &Path,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    match command {
        ProjectCommand::Open => {
            if let Err(message) = runtime.cloud.ensure_idle() {
                state.push_diagnostic(message);
                return;
            }
            schedule_project_call(
                runtime,
                "vibeCode/projects/open",
                json!({
                    "workingDirectory": working_directory,
                    "purpose": "configure",
                }),
                ProjectPendingOperation::Open {
                    working_directory: working_directory.to_owned(),
                    teleport: false,
                    prompt: None,
                },
                state,
            );
        }
        ProjectCommand::Select(project_id) => {
            let Some(picker_id) = runtime.cloud.picker_id().map(ToOwned::to_owned) else {
                state.push_diagnostic("Open the remote project picker first");
                return;
            };
            schedule_project_call(
                runtime,
                "vibeCode/projects/select",
                json!({"pickerId": picker_id, "projectId": project_id}),
                ProjectPendingOperation::Select {
                    working_directory: working_directory.to_owned(),
                    project_id,
                },
                state,
            );
        }
        ProjectCommand::More => {
            let Some(picker_id) = runtime.cloud.picker_id().map(ToOwned::to_owned) else {
                state.push_diagnostic("Open the remote project picker first");
                return;
            };
            let query = state
                .overlay
                .as_ref()
                .map(|overlay| overlay.query.clone())
                .unwrap_or_default();
            schedule_project_call(
                runtime,
                "vibeCode/projects/loadMore",
                json!({"pickerId": picker_id}),
                ProjectPendingOperation::More { query },
                state,
            );
        }
        ProjectCommand::Create {
            name,
            default_branch,
        } => {
            let Some(picker_id) = runtime.cloud.picker_id().map(ToOwned::to_owned) else {
                state.push_diagnostic("Open the remote project picker first");
                return;
            };
            schedule_project_call(
                runtime,
                "vibeCode/projects/create",
                json!({
                    "pickerId": picker_id,
                    "name": name,
                    "defaultBranch": default_branch,
                }),
                ProjectPendingOperation::Create {
                    working_directory: working_directory.to_owned(),
                    requested_name: name,
                },
                state,
            );
        }
        action @ (ProjectCommand::Unlink | ProjectCommand::Cancel) => {
            let Some(picker_id) = runtime.cloud.picker_id().map(ToOwned::to_owned) else {
                state.push_diagnostic("No remote project picker is active");
                return;
            };
            let unlink = action == ProjectCommand::Unlink;
            let method = if unlink {
                "vibeCode/projects/unlink"
            } else {
                "vibeCode/projects/cancel"
            };
            schedule_project_call(
                runtime,
                method,
                json!({"pickerId": picker_id}),
                ProjectPendingOperation::ClosePicker { unlink },
                state,
            );
        }
    }
}

pub(super) fn handle_teleport_command(
    command_arguments: &str,
    working_directory: &Path,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let arguments = command_arguments.split_whitespace().collect::<Vec<_>>();
    if let Some(action) = arguments.first()
        && matches!(*action, "approve" | "deny" | "cancel")
    {
        let Some(operation_id) = runtime.cloud.teleport_operation_id().map(ToOwned::to_owned)
        else {
            state.push_diagnostic("No Teleport operation is active");
            return;
        };
        let (method, params) = if *action == "cancel" {
            (
                "vibeCode/teleport/cancel",
                json!({"operationId": operation_id}),
            )
        } else {
            (
                "vibeCode/teleport/push/respond",
                json!({"operationId": operation_id, "approved": *action == "approve"}),
            )
        };
        schedule_project_call(
            runtime,
            method,
            params,
            ProjectPendingOperation::TeleportResponse,
            state,
        );
        return;
    }
    let prompt = (!arguments.is_empty()).then(|| arguments.join(" "));
    start_teleport(prompt.as_deref(), working_directory, runtime, state);
}

pub(super) fn handle_teleport_push_response(
    action: super::interaction::TeleportPushAction,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    state.overlay = None;
    schedule_project_call(
        runtime,
        "vibeCode/teleport/push/respond",
        json!({"operationId": action.operation_id, "approved": action.approved}),
        ProjectPendingOperation::TeleportResponse,
        state,
    );
}

pub(super) fn start_teleport(
    prompt: Option<&str>,
    working_directory: &Path,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    if let Err(message) = runtime.cloud.ensure_idle() {
        state.push_diagnostic(message);
        return;
    }
    schedule_project_call(
        runtime,
        "vibeCode/projects/open",
        with_optional_prompt(
            json!({
                "workingDirectory": working_directory,
                "purpose": "teleport",
            }),
            prompt,
        ),
        ProjectPendingOperation::Open {
            working_directory: working_directory.to_owned(),
            teleport: true,
            prompt: prompt.map(ToOwned::to_owned),
        },
        state,
    );
}

fn schedule_project_call(
    runtime: &mut InteractiveRuntime,
    method: &str,
    params: Value,
    operation: ProjectPendingOperation,
    state: &mut TuiState,
) {
    schedule_ui_call(
        runtime,
        method,
        params,
        UiOperation::RemoteProject(operation),
        state,
    );
}

pub(in crate::tui) fn apply_pending_operation(
    operation: ProjectPendingOperation,
    result: Result<PublicDispatch, String>,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let dispatch = match result {
        Ok(dispatch) => dispatch,
        Err(error) => {
            report_teleport_start_failure(&operation, runtime, state);
            state.push_diagnostic(error);
            restore_remote_project_overlay(runtime, state);
            return;
        }
    };
    let value = Value::Object(dispatch.result.clone().into_iter().collect());
    match operation {
        ProjectPendingOperation::Open {
            working_directory,
            teleport,
            prompt,
        } => apply_open_result(&value, working_directory, teleport, prompt, runtime, state),
        ProjectPendingOperation::Select {
            working_directory,
            project_id,
        } => {
            resolve_selection(runtime, ProjectSelectionSource::SelectedExisting);
            let project_name = value
                .pointer("/project/name")
                .and_then(Value::as_str)
                .unwrap_or(&project_id)
                .to_owned();
            state.overlay = None;
            runtime.remote_project_overlay = None;
            runtime.remote_project_draft = None;
            complete_project_selection(
                project_id,
                project_name,
                &working_directory,
                runtime,
                state,
            );
        }
        ProjectPendingOperation::More { query } => {
            let Some(view) = value.get("view") else {
                state.push_diagnostic("Remote project picker omitted its view");
                return;
            };
            let mut overlay = remote_projects_overlay(view);
            overlay.set_query(query);
            if let Some(project_id) = value
                .get("focusOptionId")
                .and_then(Value::as_str)
                .and_then(|id| id.strip_prefix("project:"))
            {
                overlay.select_by_id(&format!("remote-project:select:{project_id}"));
            }
            runtime.remote_project_overlay = Some(overlay.clone());
            state.overlay = Some(overlay);
        }
        ProjectPendingOperation::Create {
            working_directory,
            requested_name,
        } => {
            resolve_selection(runtime, ProjectSelectionSource::CreatedProject);
            let Some(project_id) = value
                .pointer("/project/projectId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
            else {
                state.push_diagnostic("Created remote project omitted its identity");
                restore_remote_project_overlay(runtime, state);
                return;
            };
            let project_name = value
                .pointer("/project/name")
                .and_then(Value::as_str)
                .unwrap_or(&requested_name)
                .to_owned();
            state.overlay = None;
            runtime.remote_project_overlay = None;
            runtime.remote_project_draft = None;
            complete_project_selection(
                project_id,
                project_name,
                &working_directory,
                runtime,
                state,
            );
        }
        ProjectPendingOperation::ClosePicker { unlink } => {
            runtime.cloud.cancel_project_selection();
            state.overlay = None;
            runtime.remote_project_overlay = None;
            runtime.remote_project_draft = None;
            // Reference `RemoteProjectOutcome`: closing the picker is an
            // outcome of its own, and unlinking is a different one from
            // walking away.
            if let Some(picker) = resolve_selection(runtime, ProjectSelectionSource::Cancelled) {
                let outcome = if unlink {
                    RemoteProjectOutcome::Unlinked
                } else {
                    RemoteProjectOutcome::Cancelled
                };
                report_remote_project(runtime, outcome, picker);
            }
            runtime.project_picker = None;
            if unlink {
                push_local_notice(
                    state,
                    "Remote Vibe Code project link cleared.",
                    EntryStatus::Completed,
                );
            }
        }
        ProjectPendingOperation::TeleportResponse => runtime.cloud.complete_teleport(),
        ProjectPendingOperation::TeleportStart { operation_id } => {
            if !teleport_dispatch_is_terminal(&dispatch)
                && let Err(message) = runtime.cloud.start_teleport(operation_id)
            {
                state.push_diagnostic(message);
            }
        }
    }
}

/// Reference `build_project_picker_telemetry`: what the picker reports about
/// itself before the operator answers it.
///
/// `shown` is decided by the caller, because a teleport whose project resolved
/// from the saved link never opens one.
fn picker_payload(view: Option<&Value>, shown: bool) -> ProjectPicker {
    let Some(view) = view else {
        return ProjectPicker {
            shown,
            ..ProjectPicker::hidden()
        };
    };
    let projects = view
        .pointer("/state/projects")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let remote = view
        .pointer("/state/repoUrl")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let repositories = projects
        .iter()
        .map(|project| {
            project
                .get("repositories")
                .and_then(Value::as_array)
                .map(|repositories| {
                    repositories
                        .iter()
                        .filter_map(|repository| {
                            repository
                                .get("repoUrl")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    ProjectPicker {
        shown,
        selection_source: None,
        candidate_count_loaded: Some(projects.len() as u64),
        multi_repo_match_count: Some(multi_repo_match_count(
            repositories.iter().map(Vec::as_slice),
            remote,
        )),
        saved_project_link_cleared: Some(
            view.get("savedProjectLinkCleared")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
        repo_remote_changed: Some(
            view.get("projectRepoRemoteChanged")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    }
}

/// Names how the project this run uses was chosen, and answers the payload the
/// events carry.
fn resolve_selection(
    runtime: &mut InteractiveRuntime,
    source: ProjectSelectionSource,
) -> Option<ProjectPicker> {
    let picker = runtime.project_picker.as_mut()?;
    picker.selection_source = Some(source);
    Some(*picker)
}

/// What a teleport reports as its error class when the refusal reached the
/// client as prose rather than as a classified service error.
const TELEPORT_START_ERROR_CLASS: &str = "TeleportStartError";

/// Reference `_fail_early` and `send_failure_if_needed`: a teleport the service
/// refused still reports a failure, even when the refusal answered the request
/// itself and no progress event ever arrived.
///
/// A run already under way is attributed to the stage its tracker had reached;
/// one refused before it started has no tracker, which is the early-failure
/// payload the reference sends from the same place.
fn report_teleport_start_failure(
    operation: &ProjectPendingOperation,
    runtime: &mut InteractiveRuntime,
    state: &TuiState,
) {
    if !matches!(
        operation,
        ProjectPendingOperation::Open { teleport: true, .. }
            | ProjectPendingOperation::TeleportStart { .. }
            | ProjectPendingOperation::TeleportResponse
    ) {
        return;
    }
    let record = match runtime.teleport_telemetry.as_mut() {
        Some(tracker) => {
            tracker.record_unexpected_error(TELEPORT_START_ERROR_CLASS);
            tracker.failed()
        }
        // Reference `_require_teleport_available`: the refusal that never
        // starts a run is attributed to the eligibility stage.
        None => Some(teleport_early_failure(
            TeleportFailureStage::Ineligible,
            TELEPORT_START_ERROR_CLASS,
            state.entries.len() as u64,
        )),
    };
    runtime.teleport_telemetry = None;
    runtime.project_picker = None;
    if let Some(record) = record {
        runtime.report(&record);
    }
}

/// Reference `send_remote_project_configured`, raised where the operator's
/// answer settles the link.
fn report_remote_project(
    runtime: &InteractiveRuntime,
    outcome: RemoteProjectOutcome,
    picker: ProjectPicker,
) {
    runtime.report(&TelemetryRecord::RemoteProjectConfigured { outcome, picker });
}

fn apply_open_result(
    value: &Value,
    working_directory: PathBuf,
    teleport: bool,
    prompt: Option<String>,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let Some(picker_id) = value
        .get("pickerId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
    else {
        state.push_diagnostic("Remote project picker omitted its identity");
        return;
    };
    runtime.project_picker = Some(picker_payload(value.get("view"), true));
    if !teleport {
        let Some(view) = value.get("view") else {
            state.push_diagnostic("Remote project picker omitted its view");
            return;
        };
        if let Err(message) = runtime.cloud.configure_project(picker_id) {
            state.push_diagnostic(message);
            return;
        }
        show_remote_project_overlay(runtime, state, view);
        return;
    }
    if let Some(project_id) = value
        .get("resolvedProjectId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
    {
        // A project the saved link resolved is never picked, so the payload
        // reports a picker that was not shown. `resolvedProjectId` is answered
        // for a saved link alone, which is the selection source it names, and
        // the source is what decides whether a refused link is reported as
        // cleared.
        runtime.project_picker = Some(ProjectPicker {
            selection_source: Some(ProjectSelectionSource::SavedLink),
            ..picker_payload(value.get("view"), false)
        });
        begin_teleport(
            picker_id,
            project_id,
            prompt.as_deref(),
            &working_directory,
            runtime,
            state,
        );
        return;
    }
    if let Err(message) = runtime.cloud.select_teleport_project(picker_id, prompt) {
        state.push_diagnostic(message);
        return;
    }
    let Some(view) = value.get("view") else {
        state.push_diagnostic("Teleport project picker omitted its view");
        runtime.cloud.cancel_project_selection();
        return;
    };
    show_remote_project_overlay(runtime, state, view);
}

fn show_remote_project_overlay(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    view: &Value,
) {
    let overlay = remote_projects_overlay(view);
    runtime.remote_project_overlay = Some(overlay.clone());
    state.overlay = Some(overlay);
}

fn restore_remote_project_overlay(runtime: &InteractiveRuntime, state: &mut TuiState) {
    if state.overlay.is_none() {
        state.overlay.clone_from(&runtime.remote_project_overlay);
    }
}

fn complete_project_selection(
    project_id: String,
    project_name: String,
    working_directory: &Path,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    match runtime.cloud.complete_project_selection() {
        Some(ProjectSelection::StartTeleport { picker_id, prompt }) => begin_teleport(
            picker_id,
            project_id,
            prompt.as_deref(),
            working_directory,
            runtime,
            state,
        ),
        Some(ProjectSelection::Configured) => {
            if let Some(picker) = runtime.project_picker {
                let outcome = match picker.selection_source {
                    Some(ProjectSelectionSource::CreatedProject) => RemoteProjectOutcome::Created,
                    _ => RemoteProjectOutcome::Configured,
                };
                report_remote_project(runtime, outcome, picker);
            }
            runtime.project_picker = None;
            push_local_notice(
                state,
                &format!("Linked this repository to Vibe Code project **{project_name}**."),
                EntryStatus::Completed,
            );
        }
        None => {
            state.push_diagnostic("Remote project selection completed without an active picker");
        }
    }
}

fn begin_teleport(
    picker_id: String,
    project_id: String,
    prompt: Option<&str>,
    working_directory: &Path,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    // Reference `TeleportTelemetryTracker`, built where the run starts: a
    // failure before any progress is attributed to the eligibility stage, which
    // is the last thing checked before the first yield.
    runtime.teleport_telemetry = Some(TeleportTracker::new(
        state.entries.len() as u64,
        TeleportFailureStage::Ineligible,
        runtime.project_picker,
    ));
    let operation_id = format!("teleport-{}", vibe_core::clock::now_millis());
    schedule_project_call(
        runtime,
        "vibeCode/teleport/start",
        with_optional_prompt(
            json!({
                "operationId": operation_id,
                "pickerId": picker_id,
                "projectId": project_id,
                "workingDirectory": working_directory,
            }),
            prompt,
        ),
        ProjectPendingOperation::TeleportStart { operation_id },
        state,
    );
}

fn teleport_dispatch_is_terminal(dispatch: &PublicDispatch) -> bool {
    dispatch.notifications.iter().rev().any(|notification| {
        notification.method == "vibeCode/teleport/event"
            && notification
                .params
                .get("event")
                .and_then(|event| event.get("kind"))
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "complete" | "failed" | "cancelled"))
    })
}

fn with_optional_prompt(mut params: Value, prompt: Option<&str>) -> Value {
    if let Some(prompt) = prompt
        && let Some(params) = params.as_object_mut()
    {
        params.insert("prompt".to_owned(), json!(prompt));
    }
    params
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;
    use vibe_app_server::client::PublicNotification;

    use super::super::interaction::Overlay;
    use super::super::runtime::interactive_test_runtime;
    use super::super::state::TuiState;
    use super::super::{interaction, pickers, workflow};
    use super::*;

    #[test]
    fn teleport_terminal_detection_is_notification_driven() {
        let dispatch = PublicDispatch {
            result: BTreeMap::new(),
            notifications: vec![PublicNotification {
                method: "vibeCode/teleport/event".to_owned(),
                params: BTreeMap::from([(
                    "event".to_owned(),
                    json!({"kind": "complete", "url": "https://example.test/project"}),
                )]),
            }],
        };
        assert!(teleport_dispatch_is_terminal(&dispatch));
    }

    /// The picker view a saved link resolves a project from.
    fn saved_link_open_result() -> Value {
        json!({
            "pickerId": "picker-1",
            "resolvedProjectId": "project-1",
            "view": {
                "state": {
                    "repoUrl": "git@example.test:vibe.git",
                    "projects": [
                        {
                            "projectId": "project-1",
                            "name": "vibe",
                            "isReadOnly": false,
                            "repositories": [
                                {"repoUrl": "git@example.test:vibe.git", "defaultBranch": "main"},
                                {"repoUrl": "git@example.test:other.git", "defaultBranch": null},
                            ],
                        },
                    ],
                },
                "savedProjectLinkCleared": false,
                "projectRepoRemoteChanged": false,
            },
        })
    }

    /// US-011: `resolvedProjectId` is answered for a saved link alone, so the
    /// run it starts reports that source, and a service that refuses the link
    /// with a 403 is what reports it as cleared.
    #[test]
    fn a_saved_link_teleport_reports_its_source_and_clears_a_refused_link() {
        let mut runtime = interactive_test_runtime("teleport-saved-link");
        let mut state = TuiState::new("teleport-saved-link");

        apply_open_result(
            &saved_link_open_result(),
            PathBuf::from("/workspace"),
            true,
            None,
            &mut runtime,
            &mut state,
        );

        let picker = runtime.project_picker.expect("the run carries a payload");
        assert_eq!(
            picker.selection_source,
            Some(ProjectSelectionSource::SavedLink)
        );
        assert!(!picker.shown, "a link that resolved opens no picker");
        assert_eq!(
            picker.multi_repo_match_count,
            Some(1),
            "the linked project carries a second repository"
        );

        let mut tracker = runtime
            .teleport_telemetry
            .clone()
            .expect("the run opened a tracker");
        tracker.record_service_error("ServiceTeleportError", Some("http".to_owned()), Some(403));
        let record = tracker.failed().expect("a classified error is a failure");
        let properties = record
            .attributes(None)
            .expect("the payload carries no unsafe label")
            .into_properties();
        assert_eq!(properties["saved_project_link_cleared"], json!(true));
    }

    /// US-011: a teleport the service refused answers the request rather than
    /// reporting progress, and the refusal still closes the run.
    #[test]
    fn a_refused_teleport_request_closes_the_run() {
        let mut runtime = interactive_test_runtime("teleport-refused");
        let mut state = TuiState::new("teleport-refused");
        apply_open_result(
            &saved_link_open_result(),
            PathBuf::from("/workspace"),
            true,
            None,
            &mut runtime,
            &mut state,
        );
        assert!(runtime.teleport_telemetry.is_some());

        apply_pending_operation(
            ProjectPendingOperation::TeleportStart {
                operation_id: "teleport-1".to_owned(),
            },
            Err("Teleport requires an active Mistral model".to_owned()),
            &mut runtime,
            &mut state,
        );

        assert!(
            runtime.teleport_telemetry.is_none() && runtime.project_picker.is_none(),
            "a refused request is terminal for the run"
        );

        // A refusal that never opened a run reports the stage it never left.
        let record = teleport_early_failure(TeleportFailureStage::Ineligible, "OracleError", 7);
        let properties = record
            .attributes(None)
            .expect("the payload carries no unsafe label")
            .into_properties();
        assert_eq!(properties["stage"], json!("ineligible"));
        assert_eq!(properties["push_required"], json!(false));
        assert_eq!(properties["nb_session_messages"], json!(7));
    }

    #[test]
    fn optional_prompt_only_mutates_object_requests() {
        assert_eq!(
            with_optional_prompt(json!({"purpose": "teleport"}), Some("continue")),
            json!({"purpose": "teleport", "prompt": "continue"})
        );
        assert_eq!(
            with_optional_prompt(Value::Null, Some("ignored")),
            Value::Null
        );
    }

    #[test]
    fn remote_project_create_draft_survives_failure_and_clears_on_success_or_cancel() {
        let draft = interaction::RemoteProjectDraft {
            name: "vibe-rs".to_owned(),
            default_branch: "main".to_owned(),
        };
        let picker = Overlay::new(
            interaction::OverlayKind::RemoteProjects,
            "Projects",
            Vec::new(),
        );
        let mut runtime = interactive_test_runtime("remote-project-create");
        runtime
            .cloud
            .configure_project("picker".to_owned())
            .expect("picker starts");
        runtime.remote_project_overlay = Some(picker.clone());
        runtime.remote_project_draft = Some(draft.clone());
        let mut state = TuiState::new("remote-project-create");
        let mut create_overlay = pickers::remote_project_create_overlay(&draft);
        create_overlay.select_by_id("remote-project:create:submit");
        state.overlay = Some(create_overlay);
        let mut submitting_runtime = Some(runtime);

        assert!(matches!(
            workflow::handle_remote_project_create_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &mut submitting_runtime,
                &mut state,
            ),
            workflow::OverlayKeyResult::Effect(workflow::OverlayEffect::RemoteProject(
                interaction::RemoteProjectAction::Create { .. }
            ))
        ));
        assert_eq!(
            submitting_runtime
                .as_ref()
                .expect("runtime remains mounted")
                .remote_project_draft,
            Some(draft.clone())
        );

        let mut runtime = interactive_test_runtime("remote-project-create-failure");
        runtime.remote_project_overlay = Some(picker.clone());
        runtime.remote_project_draft = Some(draft.clone());
        state.overlay = Some(pickers::remote_project_create_overlay(&draft));
        apply_pending_operation(
            ProjectPendingOperation::Create {
                working_directory: PathBuf::from("/workspace"),
                requested_name: draft.name.clone(),
            },
            Err("creation failed".to_owned()),
            &mut runtime,
            &mut state,
        );
        assert_eq!(runtime.remote_project_draft, Some(draft.clone()));
        assert_eq!(
            state.overlay.as_ref().map(|overlay| overlay.kind),
            Some(interaction::OverlayKind::RemoteProjectCreate)
        );

        runtime
            .cloud
            .configure_project("picker".to_owned())
            .expect("picker restarts");
        apply_pending_operation(
            ProjectPendingOperation::Create {
                working_directory: PathBuf::from("/workspace"),
                requested_name: draft.name.clone(),
            },
            Ok(PublicDispatch {
                result: BTreeMap::from([(
                    "project".to_owned(),
                    json!({"projectId": "project-1", "name": "vibe-rs"}),
                )]),
                notifications: Vec::new(),
            }),
            &mut runtime,
            &mut state,
        );
        assert!(runtime.remote_project_draft.is_none());
        assert!(state.overlay.is_none());

        runtime.remote_project_overlay = Some(picker);
        runtime.remote_project_draft = Some(draft.clone());
        state.overlay = Some(pickers::remote_project_create_overlay(&draft));
        let mut runtime = Some(runtime);
        assert_eq!(
            workflow::handle_remote_project_create_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &mut runtime,
                &mut state,
            ),
            workflow::OverlayKeyResult::Handled
        );
        assert!(
            runtime
                .as_ref()
                .expect("runtime remains mounted")
                .remote_project_draft
                .is_none()
        );
        assert_eq!(
            state.overlay.as_ref().map(|overlay| overlay.kind),
            Some(interaction::OverlayKind::RemoteProjects)
        );
    }
}

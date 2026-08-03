use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use vibe_app_server::client::PublicDispatch;

use super::cloud_workflow::ProjectSelection;
use super::interaction::RemoteProjectAction;
use super::pickers::remote_projects_overlay;
use super::{
    EntryStatus, InteractiveRuntime, TuiState, UiOperation, push_local_notice, schedule_ui_call,
};

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
        Some(ProjectSelection::Configured) => push_local_notice(
            state,
            &format!("Linked this repository to Vibe Code project **{project_name}**."),
            EntryStatus::Completed,
        ),
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
    let operation_id = format!(
        "teleport-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis())
    );
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

    use vibe_app_server::client::PublicNotification;

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
}

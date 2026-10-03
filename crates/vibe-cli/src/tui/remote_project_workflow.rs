use std::path::Path;

use serde_json::{Value, json};
use vibe_app_server::client::{PublicDispatch, PublicNotification};

use super::cloud_workflow::ProjectSelection;
use super::interaction::RemoteProjectAction;
use super::pickers::remote_projects_overlay;
use super::runtime::{UiOperation, schedule_ui_background, schedule_ui_call};
use super::{
    EntryStatus, InteractiveRuntime, TuiState, apply_public_notifications, push_local_notice,
};

/// The failure code a Teleport run reports when the project its saved link
/// named is gone; reference `_handle_teleport_failure` reopens the picker on it.
const SAVED_PROJECT_STALE: &str = "saved_project_stale";

#[derive(Debug, Clone)]
pub(in crate::tui) enum ProjectPendingOperation {
    Open {
        teleport: bool,
        prompt: Option<String>,
    },
    Select {
        project_id: String,
    },
    More {
        query: String,
    },
    Create {
        requested_name: String,
    },
    ClosePicker {
        unlink: bool,
    },
    TeleportResponse,
    /// The answer to `vibeCode/teleport/start`; the run reports through
    /// [`Self::TeleportEvent`] after it.
    TeleportStart,
    TeleportEvent {
        picker_id: String,
        prompt: Option<String>,
    },
    /// Reference `recover_stale_link`, asked after a run failed because the
    /// saved project is gone; `failure` is shown when nothing is recovered.
    Recover {
        picker_id: String,
        prompt: Option<String>,
        failure: String,
    },
}

pub(super) fn handle_project_action(
    action: RemoteProjectAction,
    _working_directory: &Path,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    execute_project_command(action.into(), runtime, state);
}

/// Reference `_vibe_code_project_command`: `/remote-project` opens the
/// project picker and reads no arguments; what the picker offers is chosen in
/// the picker.
pub(super) fn open_project_picker(
    _working_directory: &Path,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    execute_project_command(ProjectCommand::Open, runtime, state);
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
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    if command == ProjectCommand::Open {
        if let Err(message) = runtime.cloud.ensure_idle() {
            state.push_diagnostic(message);
            return;
        }
        schedule_project_call(
            runtime,
            "vibeCode/projects/open",
            json!({"purpose": "configure"}),
            ProjectPendingOperation::Open {
                teleport: false,
                prompt: None,
            },
            state,
        );
        return;
    }
    let Some(picker_id) = runtime.cloud.picker_id().map(ToOwned::to_owned) else {
        state.push_diagnostic("Open the remote project picker first");
        return;
    };
    let (method, params, operation) = match command {
        ProjectCommand::Open => return,
        ProjectCommand::Select(project_id) => (
            "vibeCode/projects/select",
            json!({"pickerId": picker_id, "projectId": project_id}),
            ProjectPendingOperation::Select { project_id },
        ),
        ProjectCommand::More => {
            let query = state
                .overlay
                .as_ref()
                .map(|overlay| overlay.query.clone())
                .unwrap_or_default();
            (
                "vibeCode/projects/loadMore",
                json!({"pickerId": picker_id}),
                ProjectPendingOperation::More { query },
            )
        }
        ProjectCommand::Create {
            name,
            default_branch,
        } => (
            "vibeCode/projects/create",
            json!({
                "pickerId": picker_id,
                "name": name,
                "defaultBranch": default_branch,
            }),
            ProjectPendingOperation::Create {
                requested_name: name,
            },
        ),
        ProjectCommand::Unlink => (
            "vibeCode/projects/unlink",
            json!({"pickerId": picker_id}),
            ProjectPendingOperation::ClosePicker { unlink: true },
        ),
        ProjectCommand::Cancel => (
            "vibeCode/projects/cancel",
            json!({"pickerId": picker_id}),
            ProjectPendingOperation::ClosePicker { unlink: false },
        ),
    };
    schedule_project_call(runtime, method, params, operation, state);
}

/// Reference `_handle_teleport_command`: `/teleport` teleports the session
/// as it stands, and `&prompt` teleports it with `prompt` to run remotely.
pub(super) fn handle_teleport_command(
    prompt: Option<&str>,
    working_directory: &Path,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    start_teleport(
        prompt.filter(|prompt| !prompt.is_empty()),
        working_directory,
        runtime,
        state,
    );
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

/// Reference `_resolve_vibe_code_project_for_teleport`: the server gates the
/// run and answers the project a saved link resolves, or a picker to choose
/// one from.
pub(super) fn start_teleport(
    prompt: Option<&str>,
    _working_directory: &Path,
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
        with_optional_prompt(json!({"purpose": "teleport"}), prompt),
        ProjectPendingOperation::Open {
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
            if matches!(
                operation,
                ProjectPendingOperation::TeleportStart | ProjectPendingOperation::TeleportResponse
            ) {
                runtime.cloud.complete_teleport();
            }
            if let ProjectPendingOperation::Recover { failure, .. } = operation {
                state.push_diagnostic(failure);
            }
            state.push_diagnostic(error);
            restore_remote_project_overlay(runtime, state);
            return;
        }
    };
    let value = Value::Object(dispatch.result.clone().into_iter().collect());
    match operation {
        ProjectPendingOperation::Open { teleport, prompt } => {
            apply_open_result(&value, teleport, prompt, runtime, state);
        }
        ProjectPendingOperation::Select { project_id } => {
            let project_name = value
                .pointer("/project/name")
                .and_then(Value::as_str)
                .unwrap_or(&project_id)
                .to_owned();
            state.overlay = None;
            runtime.remote_project_overlay = None;
            runtime.remote_project_draft = None;
            complete_project_selection(project_id, project_name, runtime, state);
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
        // Reference `on_vibe_code_project_create_app_submitted`: a created
        // project is then selected, which is what saves the link.
        ProjectPendingOperation::Create { requested_name } => {
            let Some(project_id) = value
                .pointer("/project/projectId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
            else {
                state.push_diagnostic(format!(
                    "Created remote project {requested_name} omitted its identity"
                ));
                restore_remote_project_overlay(runtime, state);
                return;
            };
            state.overlay = None;
            runtime.remote_project_overlay = None;
            runtime.remote_project_draft = None;
            execute_project_command(ProjectCommand::Select(project_id), runtime, state);
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
        ProjectPendingOperation::TeleportResponse | ProjectPendingOperation::TeleportStart => {}
        ProjectPendingOperation::TeleportEvent { picker_id, prompt } => {
            apply_teleport_event(&dispatch, picker_id, prompt, runtime, state);
        }
        ProjectPendingOperation::Recover {
            picker_id,
            prompt,
            failure,
        } => {
            let recovered = value
                .get("recovered")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match value.get("view") {
                Some(view) if recovered => {
                    if let Err(message) = runtime.cloud.select_teleport_project(picker_id, prompt) {
                        state.push_diagnostic(message);
                        return;
                    }
                    push_local_notice(
                        state,
                        "The saved Vibe Code project is gone; pick the project this repository should use.",
                        EntryStatus::Completed,
                    );
                    show_remote_project_overlay(runtime, state, view);
                }
                _ => state.push_diagnostic(failure),
            }
        }
    }
}

/// One event of a running Teleport, delivered as it is published. A failure
/// that names a stale saved project asks the picker to recover before it is
/// shown, as reference `_handle_teleport_failure` does.
fn apply_teleport_event(
    dispatch: &PublicDispatch,
    picker_id: String,
    prompt: Option<String>,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let event = dispatch
        .notifications
        .first()
        .and_then(|notification| notification.params.get("event"));
    let kind = event
        .and_then(|event| event.get("kind"))
        .and_then(Value::as_str);
    if matches!(kind, Some("complete" | "failed" | "cancelled")) {
        runtime.cloud.complete_teleport();
    }
    if kind == Some("failed")
        && event
            .and_then(|event| event.pointer("/error/code"))
            .and_then(Value::as_str)
            == Some(SAVED_PROJECT_STALE)
    {
        let failure = event
            .and_then(|event| event.pointer("/error/message"))
            .and_then(Value::as_str)
            .unwrap_or("Teleport failed")
            .to_owned();
        schedule_project_call(
            runtime,
            "vibeCode/projects/recover",
            json!({"pickerId": picker_id}),
            ProjectPendingOperation::Recover {
                picker_id,
                prompt,
                failure,
            },
            state,
        );
        return;
    }
    apply_public_notifications(dispatch, state);
}

fn apply_open_result(
    value: &Value,
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
        begin_teleport(picker_id, project_id, prompt, runtime, state);
        return;
    }
    let Some(view) = value.get("view") else {
        state.push_diagnostic("Teleport project picker omitted its view");
        return;
    };
    if view
        .get("savedProjectLinkCleared")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        push_local_notice(
            state,
            "The saved Vibe Code project link names another repository remote; pick the project this repository should use.",
            EntryStatus::Completed,
        );
    }
    if let Err(message) = runtime.cloud.select_teleport_project(picker_id, prompt) {
        state.push_diagnostic(message);
        return;
    }
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
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    match runtime.cloud.complete_project_selection() {
        Some(ProjectSelection::StartTeleport { picker_id, prompt }) => {
            begin_teleport(picker_id, project_id, prompt, runtime, state);
        }
        Some(ProjectSelection::Configured) => {
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

/// Reference `_teleport`: the start is answered first, then every event of
/// the run arrives on its own as the server publishes it.
fn begin_teleport(
    picker_id: String,
    project_id: String,
    prompt: Option<String>,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let operation_id = vibe_core::session_id::uuid_v4();
    if let Err(message) = runtime.cloud.start_teleport(operation_id.clone()) {
        state.push_diagnostic(message);
        return;
    }
    let params = with_optional_prompt(
        json!({
            "operationId": operation_id,
            "pickerId": picker_id,
            "projectId": project_id,
        }),
        prompt.as_deref(),
    );
    let wanted = operation_id;
    let progress = move |notification: &PublicNotification| {
        let event = notification.params.get("event")?;
        (notification.method == "vibeCode/teleport/event"
            && event.get("operationId").and_then(Value::as_str) == Some(wanted.as_str()))
        .then(|| {
            UiOperation::RemoteProject(ProjectPendingOperation::TeleportEvent {
                picker_id: picker_id.clone(),
                prompt: prompt.clone(),
            })
        })
    };
    if !schedule_ui_background(
        runtime,
        "vibeCode/teleport/start",
        params,
        progress,
        UiOperation::RemoteProject(ProjectPendingOperation::TeleportStart),
        state,
    ) {
        runtime.cloud.complete_teleport();
    }
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

    use super::super::cloud_workflow::CloudWorkflowState;
    use super::super::interaction::Overlay;
    use super::super::runtime::interactive_test_runtime;
    use super::super::state::TuiState;
    use super::super::{interaction, pickers, workflow};
    use super::*;
    use crate::tui::interaction::RemoteProjectField;

    fn event(event: Value) -> PublicDispatch {
        PublicDispatch {
            result: BTreeMap::new(),
            notifications: vec![PublicNotification {
                method: "vibeCode/teleport/event".to_owned(),
                params: BTreeMap::from([("event".to_owned(), event)]),
            }],
        }
    }

    /// A project the saved link resolved starts the run without a picker, and
    /// the run holds the workflow until an event ends it.
    #[tokio::test]
    async fn a_resolved_project_starts_the_run_and_a_terminal_event_ends_it() {
        let mut runtime = interactive_test_runtime("teleport-resolved");
        let mut state = TuiState::new("teleport-resolved");
        apply_open_result(
            &json!({"pickerId": "picker-1", "resolvedProjectId": "project-1", "view": {}}),
            true,
            None,
            &mut runtime,
            &mut state,
        );
        assert!(matches!(
            runtime.cloud,
            CloudWorkflowState::Teleporting { .. }
        ));
        assert!(state.overlay.is_none());

        apply_pending_operation(
            ProjectPendingOperation::TeleportEvent {
                picker_id: "picker-1".to_owned(),
                prompt: None,
            },
            Ok(event(
                json!({"kind": "complete", "url": "https://example.test/s"}),
            )),
            &mut runtime,
            &mut state,
        );
        assert_eq!(runtime.cloud, CloudWorkflowState::Idle);
    }

    /// Reference `_handle_teleport_failure`: a run that failed because the
    /// saved project is gone asks the picker to recover before the failure is
    /// shown, and a recovered picker reopens for the same prompt.
    #[tokio::test]
    async fn a_stale_saved_project_reopens_the_picker() {
        let mut runtime = interactive_test_runtime("teleport-stale");
        let mut state = TuiState::new("teleport-stale");
        runtime
            .cloud
            .start_teleport("operation-1".to_owned())
            .expect("idle workflow starts");
        let entries = state.entries.len();
        apply_pending_operation(
            ProjectPendingOperation::TeleportEvent {
                picker_id: "picker-1".to_owned(),
                prompt: Some("ship it".to_owned()),
            },
            Ok(event(json!({
                "kind": "failed",
                "error": {"code": SAVED_PROJECT_STALE, "message": "gone"},
            }))),
            &mut runtime,
            &mut state,
        );
        assert_eq!(runtime.cloud, CloudWorkflowState::Idle);
        assert_eq!(
            state.entries.len(),
            entries,
            "the failure waits on recovery"
        );

        apply_pending_operation(
            ProjectPendingOperation::Recover {
                picker_id: "picker-1".to_owned(),
                prompt: Some("ship it".to_owned()),
                failure: "gone".to_owned(),
            },
            Ok(PublicDispatch {
                result: BTreeMap::from([
                    ("recovered".to_owned(), json!(true)),
                    ("view".to_owned(), json!({"state": {"projects": []}})),
                ]),
                notifications: Vec::new(),
            }),
            &mut runtime,
            &mut state,
        );
        assert_eq!(
            runtime.cloud,
            CloudWorkflowState::SelectingTeleportProject {
                picker_id: "picker-1".to_owned(),
                prompt: Some("ship it".to_owned()),
            }
        );
        assert!(state.overlay.is_some());
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

    #[tokio::test]
    async fn remote_project_create_draft_survives_failure_and_clears_on_success_or_cancel() {
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
        create_overlay.select_by_id(RemoteProjectField::Submit.id());
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

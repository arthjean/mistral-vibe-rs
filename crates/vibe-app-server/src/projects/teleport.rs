//! Teleport: handing a local session to the cloud.
//!
//! What travels is decided from the working tree: a repository that can be
//! pushed is pushed, and one that cannot travels as an encoded diff. The
//! operation is staged so a push the user has to confirm can be answered
//! without losing what was already inspected.

use super::selection::picker;
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TeleportState {
    SummarizingContext,
    CheckingGit,
    PushRequired,
    Pushing,
    StartingWorkflow,
    Complete,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TeleportOperation {
    pub(super) id: String,
    pub(super) session_id: String,
    pub(super) project_id: String,
    pub(super) working_directory: PathBuf,
    pub(super) summary: String,
    pub(super) repository: TeleportRepository,
    pub(super) state: TeleportState,
    pub(super) push_response: Option<bool>,
    pub(super) unpushed_count: u64,
    pub(super) branch_not_pushed: bool,
    pub(super) url: Option<String>,
    pub(super) error: Option<String>,
    /// The HTTP status a failed start answered with, when a status is what it
    /// answered with. Reference `TeleportFailureDetails.http_status_code`.
    pub(super) error_status: Option<u16>,
}

pub(super) fn teleport_start_request(operation: &TeleportOperation) -> TeleportStartRequest {
    TeleportStartRequest {
        project_id: operation.project_id.clone(),
        idempotency_key: operation.id.clone(),
        summary: operation.summary.clone(),
        repository: operation.repository.clone(),
    }
}

pub(super) fn teleport_notification(operation: &TeleportOperation) -> ProjectsNotification {
    let event = match operation.state {
        TeleportState::SummarizingContext => {
            json!({"kind": "summarizing_context", "operationId": operation.id})
        }
        TeleportState::CheckingGit => {
            json!({"kind": "checking_git", "operationId": operation.id})
        }
        TeleportState::PushRequired => json!({
            "kind": "push_required",
            "operationId": operation.id,
            "unpushedCount": operation.unpushed_count,
            "branchNotPushed": operation.branch_not_pushed,
        }),
        TeleportState::Pushing => json!({"kind": "pushing", "operationId": operation.id}),
        TeleportState::StartingWorkflow => {
            json!({"kind": "starting_workflow", "operationId": operation.id})
        }
        TeleportState::Complete => json!({
            "kind": "complete",
            "operationId": operation.id,
            "url": operation.url,
        }),
        TeleportState::Failed => json!({
            "kind": "failed",
            "operationId": operation.id,
            "error": {
                "message": operation.error,
                "code": "teleport_failed",
                // Reference `TeleportFailureDetails`: the status is what tells
                // a saved project link the service refused from an outage, so
                // it travels when the service answered with one and the key
                // stays null when it did not.
                "details": operation
                    .error_status
                    .map(|status| json!({"httpStatusCode": status})),
            },
        }),
        TeleportState::Cancelled => {
            json!({"kind": "cancelled", "operationId": operation.id})
        }
    };
    notification("vibeCode/teleport/event", [("event", event)])
}

impl ProjectsService {
    pub(super) async fn teleport_start_cloud(
        &self,
        request: TeleportStartRequest,
    ) -> Result<String, TeleportStartFailure> {
        match self.teleport_cloud.clone() {
            TeleportCloudBackend::Sync(cloud) => {
                tokio::task::spawn_blocking(move || cloud.start(&request))
                    .await
                    .map_err(|_| {
                        TeleportStartFailure::from(CloudError::Unavailable(
                            "Teleport background task stopped unexpectedly".to_owned(),
                        ))
                    })?
            }
            TeleportCloudBackend::Async(cloud) => cloud.start(&request).await,
        }
    }

    pub(super) async fn inspect_git(
        &self,
        working_directory: PathBuf,
    ) -> Result<(GitSnapshot, TeleportRepository, GitPushStatus), CloudError> {
        let git = self.git.clone();
        tokio::task::spawn_blocking(move || git.inspect_for_teleport(&working_directory))
            .await
            .map_err(|_| {
                CloudError::Git("Git inspection background task stopped unexpectedly".to_owned())
            })?
    }

    pub(super) async fn push_git(&self, working_directory: PathBuf) -> Result<(), CloudError> {
        let git = self.git.clone();
        tokio::task::spawn_blocking(move || git.push(&working_directory))
            .await
            .map_err(|_| {
                CloudError::Git("Git push background task stopped unexpectedly".to_owned())
            })?
    }

    pub(super) async fn teleport_start(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        let session_id = required_string(params, "sessionId")?.to_owned();
        let operation_id = required_string(params, "operationId")?.to_owned();
        let picker_id = required_string(params, "pickerId")?;
        let project_id = required_string(params, "projectId")?.to_owned();
        {
            let state = self.lock_projects()?;
            let picker = picker(&state, picker_id, &session_id)?;
            if !picker.projects.contains_key(&project_id) {
                return Err(ProjectsServiceError::NotFound(format!(
                    "project `{project_id}` is not available in picker `{picker_id}`"
                )));
            }
            if picker.selected.as_deref() != Some(&project_id) {
                return Err(ProjectsServiceError::Conflict(format!(
                    "project `{project_id}` is not selected in picker `{picker_id}`"
                )));
            }
        }
        let summary = optional_string(params, "prompt")?
            .filter(|prompt| !prompt.trim().is_empty())
            .unwrap_or("Continue this session in Vibe Code")
            .to_owned();
        validate_cloud_text(&summary, "Teleport message").map_err(ProjectsServiceError::Cloud)?;
        let working_directory =
            PathBuf::from(optional_string(params, "workingDirectory")?.unwrap_or("."));
        let mut operation = TeleportOperation {
            id: operation_id.clone(),
            session_id: session_id.clone(),
            project_id,
            working_directory: working_directory.clone(),
            summary,
            repository: TeleportRepository {
                repo_url: String::new(),
                branch: None,
                commit_sha: None,
                diff: None,
            },
            state: TeleportState::SummarizingContext,
            push_response: None,
            unpushed_count: 0,
            branch_not_pushed: false,
            url: None,
            error: None,
            error_status: None,
        };
        let mut notifications = vec![teleport_notification(&operation)];
        operation.state = TeleportState::CheckingGit;
        notifications.push(teleport_notification(&operation));
        {
            let mut teleports = self.lock_teleports()?;
            if teleports.contains_key(&operation_id) {
                return Err(ProjectsServiceError::Conflict(format!(
                    "Teleport operation `{operation_id}` already exists"
                )));
            }
            teleports.insert(operation_id.clone(), operation);
        }

        let inspection = self.inspect_git(working_directory).await;
        let cloud_request = {
            let mut teleports = self.lock_teleports()?;
            let operation = teleports.get_mut(&operation_id).ok_or_else(|| {
                ProjectsServiceError::NotFound(format!(
                    "Teleport operation `{operation_id}` was not found"
                ))
            })?;
            if operation.state == TeleportState::Cancelled {
                return Ok(ProjectsDispatch::with_notifications(
                    [("operationId", json!(operation_id))],
                    notifications,
                ));
            }
            let (snapshot, repository, push_status) = match inspection {
                Ok(inspection) => inspection,
                Err(error) => {
                    operation.state = TeleportState::Failed;
                    operation.error = Some(error.to_string());
                    notifications.push(teleport_notification(operation));
                    return Ok(ProjectsDispatch::with_notifications(
                        [("operationId", json!(operation_id))],
                        notifications,
                    ));
                }
            };
            operation.repository = repository;
            if snapshot.unpushed {
                operation.state = TeleportState::PushRequired;
                operation.unpushed_count = push_status.unpushed_count;
                operation.branch_not_pushed = push_status.branch_not_pushed;
                notifications.push(teleport_notification(operation));
                return Ok(ProjectsDispatch::with_notifications(
                    [("operationId", json!(operation_id))],
                    notifications,
                ));
            }
            operation.state = TeleportState::StartingWorkflow;
            teleport_start_request(operation)
        };
        let result = self.teleport_start_cloud(cloud_request).await;
        let mut teleports = self.lock_teleports()?;
        let operation = teleports.get_mut(&operation_id).ok_or_else(|| {
            ProjectsServiceError::NotFound(format!(
                "Teleport operation `{operation_id}` was not found"
            ))
        })?;
        if operation.state == TeleportState::Cancelled {
            return Ok(ProjectsDispatch::with_notifications(
                [("operationId", json!(operation_id))],
                notifications,
            ));
        }
        match result {
            Ok(url) => {
                operation.state = TeleportState::StartingWorkflow;
                notifications.push(teleport_notification(operation));
                operation.url = Some(url);
                operation.state = TeleportState::Complete;
                notifications.push(teleport_notification(operation));
            }
            Err(failure) => {
                operation.error = Some(failure.to_string());
                operation.error_status = failure.http_status_code;
                operation.state = TeleportState::Failed;
                notifications.push(teleport_notification(operation));
            }
        }
        Ok(ProjectsDispatch::with_notifications(
            [("operationId", json!(operation_id))],
            notifications,
        ))
    }

    pub(super) async fn teleport_push_respond(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        let session_id = required_string(params, "sessionId")?.to_owned();
        let operation_id = required_string(params, "operationId")?.to_owned();
        let accepted = required_bool(params, "approved")?;
        let mut notifications = Vec::new();
        let working_directory = {
            let mut teleports = self.lock_teleports()?;
            let operation = teleports.get_mut(&operation_id).ok_or_else(|| {
                ProjectsServiceError::NotFound(format!(
                    "Teleport operation `{operation_id}` was not found"
                ))
            })?;
            if operation.session_id != session_id {
                return Err(ProjectsServiceError::NotFound(format!(
                    "Teleport operation `{operation_id}` is not owned by session `{session_id}`"
                )));
            }
            if let Some(previous) = operation.push_response {
                if previous == accepted {
                    return Ok(ProjectsDispatch::result([] as [(&str, Value); 0]));
                }
                return Err(ProjectsServiceError::Conflict(
                    "Teleport push response conflicts with the recorded answer".to_owned(),
                ));
            }
            if operation.state != TeleportState::PushRequired {
                return Err(ProjectsServiceError::Conflict(
                    "Teleport operation is not waiting for a push response".to_owned(),
                ));
            }
            operation.push_response = Some(accepted);
            if !accepted {
                operation.state = TeleportState::Failed;
                operation.error = Some(
                    "Git push was denied; the local session and working tree were not changed"
                        .to_owned(),
                );
                notifications.push(teleport_notification(operation));
                return Ok(ProjectsDispatch::with_notifications(
                    [] as [(&str, Value); 0],
                    notifications,
                ));
            }
            operation.state = TeleportState::Pushing;
            notifications.push(teleport_notification(operation));
            operation.working_directory.clone()
        };
        if let Err(error) = self.push_git(working_directory).await {
            let mut teleports = self.lock_teleports()?;
            let operation = teleports.get_mut(&operation_id).ok_or_else(|| {
                ProjectsServiceError::NotFound(format!(
                    "Teleport operation `{operation_id}` was not found"
                ))
            })?;
            if operation.state != TeleportState::Cancelled {
                operation.state = TeleportState::Failed;
                operation.error = Some(error.to_string());
                notifications.push(teleport_notification(operation));
            }
            return Ok(ProjectsDispatch::with_notifications(
                [] as [(&str, Value); 0],
                notifications,
            ));
        }
        let cloud_request = {
            let mut teleports = self.lock_teleports()?;
            let operation = teleports.get_mut(&operation_id).ok_or_else(|| {
                ProjectsServiceError::NotFound(format!(
                    "Teleport operation `{operation_id}` was not found"
                ))
            })?;
            if operation.state == TeleportState::Cancelled {
                return Ok(ProjectsDispatch::with_notifications(
                    [] as [(&str, Value); 0],
                    notifications,
                ));
            }
            operation.state = TeleportState::StartingWorkflow;
            teleport_start_request(operation)
        };
        let result = self.teleport_start_cloud(cloud_request).await;
        let mut teleports = self.lock_teleports()?;
        let operation = teleports.get_mut(&operation_id).ok_or_else(|| {
            ProjectsServiceError::NotFound(format!(
                "Teleport operation `{operation_id}` was not found"
            ))
        })?;
        if operation.state != TeleportState::Cancelled {
            match result {
                Ok(url) => {
                    operation.state = TeleportState::StartingWorkflow;
                    notifications.push(teleport_notification(operation));
                    operation.url = Some(url);
                    operation.state = TeleportState::Complete;
                    notifications.push(teleport_notification(operation));
                }
                Err(failure) => {
                    operation.error = Some(failure.to_string());
                    operation.error_status = failure.http_status_code;
                    operation.state = TeleportState::Failed;
                    notifications.push(teleport_notification(operation));
                }
            }
        }
        Ok(ProjectsDispatch::with_notifications(
            [] as [(&str, Value); 0],
            notifications,
        ))
    }

    pub(super) fn teleport_cancel(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        let session_id = required_string(params, "sessionId")?;
        let operation_id = required_string(params, "operationId")?;
        let mut teleports = self.lock_teleports()?;
        let operation = teleports.get_mut(operation_id).ok_or_else(|| {
            ProjectsServiceError::NotFound(format!(
                "Teleport operation `{operation_id}` was not found"
            ))
        })?;
        if operation.session_id != session_id {
            return Err(ProjectsServiceError::NotFound(format!(
                "Teleport operation `{operation_id}` is not owned by session `{session_id}`"
            )));
        }
        match operation.state {
            TeleportState::Complete | TeleportState::Failed => {
                return Err(ProjectsServiceError::Conflict(
                    "Teleport operation is already terminal".to_owned(),
                ));
            }
            TeleportState::Pushing | TeleportState::StartingWorkflow => {
                return Err(ProjectsServiceError::Conflict(
                    "Teleport operation is already performing irreversible work".to_owned(),
                ));
            }
            TeleportState::Cancelled => {}
            _ => {
                operation.state = TeleportState::Cancelled;
            }
        }
        Ok(ProjectsDispatch::with_notifications(
            [("cancelled", json!(true))],
            Vec::new(),
        ))
    }

    pub(super) fn lock_teleports(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, TeleportOperation>>, ProjectsServiceError>
    {
        self.teleports
            .lock()
            .map_err(|_| ProjectsServiceError::StatePoisoned)
    }
}

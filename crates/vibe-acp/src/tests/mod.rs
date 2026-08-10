//! Shared fixtures for the adapter test modules.

mod auth;
mod client_tools;
mod commands;
mod lifecycle;
mod prompt;
mod updates;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use vibe_app_server::client::{
    DriverError, DriverFuture, EchoTurnDriver, TurnDriver, TurnReservation,
};
use vibe_app_server::release4::{
    CloudError, GitProbe, GitSnapshot, Project, ProjectCloud, ProjectPage, ProjectRepository,
    TeleportCloud, TeleportStartRequest,
};
use vibe_app_server::server::ToolInvocation;

use crate::agent::AcpAgent;
use crate::client_tools::{AcpClientFuture, AcpClientPort};
use crate::protocol::{AcpError, AcpNewSession, AcpPromptResponse, AcpSession};

pub(super) fn text_prompt(text: &str) -> Vec<Value> {
    vec![json!({"type": "text", "text": text})]
}

pub(super) fn start_session<D>(agent: &AcpAgent<D>, cwd: &str) -> AcpSession
where
    D: TurnDriver,
{
    agent
        .new_session(AcpNewSession {
            cwd: cwd.to_owned(),
            additional_directories: None,
            mcp_servers: Vec::new(),
            meta: None,
        })
        .expect("session starts")
}

/// Runs a prompt to completion, discarding the streamed updates.
pub(super) async fn prompt<D>(
    agent: &AcpAgent<D>,
    session_id: &str,
    text: &str,
) -> Result<AcpPromptResponse, AcpError>
where
    D: TurnDriver,
{
    agent
        .prompt_content(session_id, text_prompt(text), |_| Ok(()))
        .await
}

pub(super) struct RecordingTurnDriver {
    inner: EchoTurnDriver,
    reservations: Arc<Mutex<Vec<TurnReservation>>>,
}

impl RecordingTurnDriver {
    pub(super) fn new(reservations: Arc<Mutex<Vec<TurnReservation>>>) -> Self {
        Self {
            inner: EchoTurnDriver::new("answer"),
            reservations,
        }
    }
}

impl TurnDriver for RecordingTurnDriver {
    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
        self.reservations
            .lock()
            .expect("reservations")
            .push(reservation.clone());
        self.inner.run(reservation)
    }
}

pub(super) struct RecordingClient {
    pub(super) calls: Mutex<Vec<String>>,
    pub(super) delay: Duration,
}

impl AcpClientPort for RecordingClient {
    fn request<'a>(&'a self, method: &'a str, _params: Value) -> AcpClientFuture<'a> {
        Box::pin(async move {
            self.calls
                .lock()
                .map_err(|_| "lock poisoned".to_owned())?
                .push(method.to_owned());
            tokio::time::sleep(self.delay).await;
            Ok(json!({"ok": true}))
        })
    }
}

pub(super) struct PermissionClient {
    pub(super) params: Mutex<Option<Value>>,
}

impl AcpClientPort for PermissionClient {
    fn request<'a>(&'a self, method: &'a str, params: Value) -> AcpClientFuture<'a> {
        Box::pin(async move {
            if method != "session/request_permission" {
                return Err(format!("unexpected method `{method}`"));
            }
            *self.params.lock().map_err(|_| "lock poisoned".to_owned())? = Some(params);
            Ok(json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "allow_always",
                }
            }))
        })
    }
}

pub(super) struct BlockingPermissionClient {
    pub(super) started: tokio::sync::Notify,
}

impl AcpClientPort for BlockingPermissionClient {
    fn request<'a>(&'a self, method: &'a str, _params: Value) -> AcpClientFuture<'a> {
        Box::pin(async move {
            if method != "session/request_permission" {
                return Err(format!("unexpected method `{method}`"));
            }
            self.started.notify_one();
            std::future::pending::<Result<Value, String>>().await
        })
    }
}

/// Invokes a workspace tool that requires approval before answering.
pub(super) struct ApprovalInvokingDriver {
    pub(super) inner: EchoTurnDriver,
}

impl TurnDriver for ApprovalInvokingDriver {
    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
        Box::pin(async move {
            reservation
                .tools
                .invoke(
                    "read_file",
                    ToolInvocation {
                        call_id: "read-for-approval".to_owned(),
                        arguments: json!({"file_path": "approval.txt"}),
                    },
                )
                .await
                .map_err(|error| DriverError::Tool(error.to_string()))?;
            self.inner.run(reservation).await
        })
    }
}

/// Requests an interactive question on its first turn, which ACP v1 must
/// decline, then behaves normally.
pub(super) struct UserInputOnceDriver {
    pub(super) inner: EchoTurnDriver,
    pub(super) attempts: AtomicU64,
}

impl TurnDriver for UserInputOnceDriver {
    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
        Box::pin(async move {
            if self.attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                let result = reservation
                    .tools
                    .invoke(
                        "ask_user_question",
                        ToolInvocation {
                            call_id: "unsupported-user-input".to_owned(),
                            arguments: json!({
                                "questions": [{
                                    "question": "Choose one",
                                    "header": "Choice",
                                    "options": [
                                        {"label": "First", "description": "First choice"},
                                        {"label": "Second", "description": "Second choice"}
                                    ],
                                    "multiSelect": false,
                                    "hideOther": true
                                }]
                            }),
                        },
                    )
                    .await;
                return Err(DriverError::Tool(match result {
                    Ok(_) => "unsupported ACP user input unexpectedly succeeded".to_owned(),
                    Err(error) => error.to_string(),
                }));
            }
            self.inner.run(reservation).await
        })
    }
}

pub(super) struct FixtureProjectCloud;

impl ProjectCloud for FixtureProjectCloud {
    fn create(
        &self,
        name: &str,
        repo_url: &str,
        default_branch: &str,
    ) -> Result<Project, CloudError> {
        Ok(Project {
            project_id: "project-1".to_owned(),
            name: name.to_owned(),
            repositories: vec![ProjectRepository {
                repo_url: repo_url.to_owned(),
                default_branch: Some(default_branch.to_owned()),
            }],
            is_read_only: false,
        })
    }

    fn list(&self, _cursor: Option<&str>) -> Result<ProjectPage, CloudError> {
        Ok(ProjectPage {
            projects: vec![Project {
                project_id: "project-1".to_owned(),
                name: "Fixture".to_owned(),
                repositories: vec![ProjectRepository {
                    repo_url: "https://github.com/example/repository.git".to_owned(),
                    default_branch: Some("main".to_owned()),
                }],
                is_read_only: false,
            }],
            next_cursor: None,
        })
    }
}

pub(super) struct FixtureTeleportCloud;

impl TeleportCloud for FixtureTeleportCloud {
    fn start(&self, request: &TeleportStartRequest) -> Result<String, CloudError> {
        Ok(format!(
            "https://cloud.example.test/teleport/{}",
            request.idempotency_key
        ))
    }
}

pub(super) struct CleanFixtureGit;

impl GitProbe for CleanFixtureGit {
    fn inspect(&self, _working_directory: &Path) -> Result<GitSnapshot, CloudError> {
        Ok(GitSnapshot {
            repository: "https://github.com/example/repository.git".to_owned(),
            dirty: false,
            unpushed: false,
        })
    }

    fn push(&self, _working_directory: &Path) -> Result<(), CloudError> {
        Ok(())
    }
}

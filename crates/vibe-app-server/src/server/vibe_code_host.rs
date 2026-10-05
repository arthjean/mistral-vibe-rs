//! The session side of `vibeCode/*`: what a [`VibeCodeController`] reads and
//! drives of the session that owns it, and the dispatch that runs its calls
//! off the request loop.
//!
//! The execution slot is the session's: a Teleport run holds it the way a
//! turn, a manual command or a compaction does, and every one of them is
//! refused while another holds it (reference `SessionExecution`,
//! `vibe/app_server/_execution.py`).

use super::*;
use crate::projects::store::{ProjectLink, ProjectsStore};
use crate::vibe_code::{
    Host, HostFuture, Purpose, Refusal, SavedLink, StartParams, VibeCodeController,
};
use vibe_core::events::ModelMessage;
use vibe_core::provider::{AssistantMessage, ProviderInput};
use vibe_core::telemetry::TelemetryRecord;

/// Where the frames a call publishes after it answered go.
pub type FrameSink = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

/// What the client that issued a call declared about itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientLaunch {
    pub entrypoint: String,
    pub client_name: Option<String>,
}

struct SessionHost {
    server: AppServer,
    session_id: String,
    deliver: FrameSink,
    launch: ClientLaunch,
}

impl SessionHost {
    /// The link store of the vibe home this server reads, which the
    /// session-less `projectLinks/*` surface shares.
    fn link_store(&self) -> ProjectsStore {
        ProjectsStore::in_home(self.server.workspace.vibe_home())
    }

    fn with_session<T>(&self, read: impl FnOnce(&mut SessionRuntime) -> T) -> Option<T> {
        let mut sessions = self.server.lock_sessions().ok()?;
        sessions.get_mut(&self.session_id).map(read)
    }
}

/// What holds the session's execution slot, as the reference names it in a
/// refusal: the kind, then the identifier.
pub(super) fn active_execution(session: &SessionRuntime) -> Option<String> {
    if let Some(turn_id) = &session.active_turn {
        return Some(format!("turn {turn_id}"));
    }
    if let Some(shell) = &session.shell_operation {
        return Some(format!("shell {}", shell.id));
    }
    if let Some(operation_id) = &session.teleport_operation {
        return Some(format!("teleport {operation_id}"));
    }
    session
        .compaction_pending
        .then(|| "lifecycle compact".to_owned())
}

impl Host for SessionHost {
    fn require_idle(&self) -> Result<(), Refusal> {
        match self
            .with_session(|session| active_execution(session))
            .flatten()
        {
            Some(active) => Err(Refusal::conflict(format!(
                "Session is busy running {active}"
            ))),
            None => Ok(()),
        }
    }

    fn begin_teleport(&self, operation_id: &str) -> Result<(), Refusal> {
        self.with_session(|session| {
            if let Some(active) = active_execution(session) {
                return Err(Refusal::conflict(format!(
                    "Session is already running {active}"
                )));
            }
            session.teleport_operation = Some(operation_id.to_owned());
            Ok(())
        })
        .unwrap_or_else(|| Err(Refusal::invalid("Session not found")))
    }

    fn finish_teleport(&self, operation_id: &str) {
        self.with_session(|session| {
            if session.teleport_operation.as_deref() == Some(operation_id) {
                session.teleport_operation = None;
            }
        });
    }

    fn config(&self) -> toml::Table {
        self.server
            .workspace
            .layered_config()
            .load()
            .map(|snapshot| snapshot.effective.clone())
            .unwrap_or_default()
    }

    fn cwd(&self) -> PathBuf {
        self.with_session(|session| PathBuf::from(&session.working_directory))
            .unwrap_or_default()
    }

    fn session_id(&self) -> String {
        self.with_session(|session| session.id.clone())
            .unwrap_or_else(|| self.session_id.clone())
    }

    /// The transcript as the session's model reads it: the system message
    /// the session composed, then what the store holds.
    fn messages(&self) -> Vec<ModelMessage> {
        let Some((id, persisted, system)) = self.with_session(|session| {
            (
                session.id.clone(),
                session.persisted.clone(),
                session
                    .system_prompt
                    .as_ref()
                    .map(|prompt| prompt.text.clone()),
            )
        }) else {
            return Vec::new();
        };
        let mut messages = match self.server.workspace.session_store().open(&id) {
            Ok(hydrated) => hydrated.messages,
            Err(_) => persisted
                .map(|hydrated| hydrated.messages)
                .unwrap_or_default(),
        };
        if let Some(content) = system
            && !matches!(messages.first(), Some(ModelMessage::System { .. }))
        {
            messages.insert(0, ModelMessage::System { content });
        }
        messages
    }

    fn read_account(&self) -> HostFuture<'_, Value> {
        Box::pin(self.server.workspace.read_account())
    }

    fn credential(&self, variable: &str) -> Option<String> {
        self.server.workspace.resolve_credential(variable)
    }

    fn summarize(
        &self,
        messages: Vec<ModelMessage>,
    ) -> HostFuture<'_, Result<AssistantMessage, String>> {
        Box::pin(async move {
            let provider = self
                .server
                .secondary_provider
                .clone()
                .ok_or_else(|| "no model is available to summarize with".to_owned())?;
            let input = ProviderInput {
                turn_id: None,
                session_id: Some(self.session_id()),
                model_override: self.server.workspace.compaction_settings().compaction_model,
                model: None,
                messages,
                stream: false,
                images: Vec::new(),
                tools: Vec::new(),
                tool_choice: None,
                thinking: false,
                reasoning_effort: None,
                headers: BTreeMap::new(),
                limits: Default::default(),
                metadata: BTreeMap::new(),
            };
            provider
                .complete(&input)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn record(&self, record: &TelemetryRecord) {
        let session_id = self.session_id();
        self.server
            .client_telemetry
            .record(record, Some(&session_id));
    }

    fn notify(&self, event: Value) {
        (self.deliver)(encode_notification(
            "vibeCode/teleport/event",
            BTreeMap::from([("event".to_owned(), event)]),
        ));
    }

    fn launch(&self) -> (String, Option<String>) {
        (
            self.launch.entrypoint.clone(),
            self.launch.client_name.clone(),
        )
    }

    fn saved_link(&self, repo_root: &str) -> Option<SavedLink> {
        match self.link_store().get_remote_project(Path::new(repo_root))? {
            ProjectLink::Remote {
                repo_url,
                project_id,
                project_name,
                ..
            } => Some(SavedLink {
                repo_root: repo_root.to_owned(),
                repo_url,
                project_id,
                project_name,
            }),
            ProjectLink::Local { .. } => None,
        }
    }

    fn save_link(&self, link: &SavedLink) -> Result<(), String> {
        self.link_store()
            .upsert_project_link(&ProjectLink::Remote {
                repo_root: PathBuf::from(&link.repo_root),
                repo_url: link.repo_url.clone(),
                project_id: link.project_id.clone(),
                project_name: link.project_name.clone(),
            })
            .map_err(|error| error.to_string())
    }

    fn delete_link(&self, repo_root: &str) {
        let _ = self
            .link_store()
            .delete_remote_project(Path::new(repo_root));
    }
}

impl AppServer {
    /// Runs one `vibeCode/*` call against the session's controller. A
    /// Teleport start answers through `deliver` before its run begins, so the
    /// answer always precedes the run's first event.
    pub(crate) async fn execute_vibe_code(
        &self,
        request_id: RequestId,
        session_id: String,
        method: String,
        params: BTreeMap<String, Value>,
        launch: ClientLaunch,
        deliver: FrameSink,
    ) -> DispatchBatch {
        let controller = match self.lock_sessions() {
            Ok(sessions) => sessions
                .get(&session_id)
                .map(|session| Arc::clone(&session.vibe_code)),
            Err(error) => return internal_error_batch(request_id, &error),
        };
        let Some(controller) = controller else {
            return plain_error_batch(request_id, ProtocolErrorCode::NotFound, "Session not found");
        };
        let host: Arc<dyn Host> = Arc::new(SessionHost {
            server: self.clone(),
            session_id,
            deliver: Arc::clone(&deliver),
            launch,
        });
        let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
        let picker_id = text("pickerId").unwrap_or_default();
        let operation_id = text("operationId").unwrap_or_default();
        let answer = match method.as_str() {
            "vibeCode/projects/open" => {
                controller
                    .open(
                        &host,
                        Purpose::parse(text("purpose").as_deref()),
                        text("prompt"),
                    )
                    .await
            }
            "vibeCode/projects/loadMore" => controller.load_more(&host, &picker_id).await,
            "vibeCode/projects/create" => {
                controller
                    .create(
                        &host,
                        &picker_id,
                        &text("name").unwrap_or_default(),
                        &text("defaultBranch").unwrap_or_default(),
                    )
                    .await
            }
            "vibeCode/projects/select" => {
                controller
                    .select(&host, &picker_id, &text("projectId").unwrap_or_default())
                    .await
            }
            "vibeCode/projects/unlink" => controller.unlink(&host, &picker_id).await,
            "vibeCode/projects/cancel" => controller.cancel(&host, &picker_id).await,
            "vibeCode/projects/recover" => controller.recover(&host, &picker_id).await,
            "vibeCode/teleport/start" => {
                let start = StartParams {
                    picker_id,
                    operation_id,
                    prompt: text("prompt"),
                    project_id: text("projectId").unwrap_or_default(),
                };
                match controller.reserve(&host, &start).await {
                    Ok(result) => {
                        deliver(success_bytes(request_id, object(result)));
                        controller.start(host, start);
                        return DispatchBatch::empty();
                    }
                    Err(refusal) => Err(refusal),
                }
            }
            "vibeCode/teleport/cancel" => controller.cancel_teleport(&host, &operation_id).await,
            "vibeCode/teleport/push/respond" => controller.respond_to_push(
                &operation_id,
                params
                    .get("approved")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            _ => Err(Refusal {
                code: ProtocolErrorCode::MethodNotFound,
                message: format!("Method not found: {method}"),
            }),
        };
        match answer {
            Ok(result) => success_batch(request_id, object(result)),
            Err(refusal) => plain_error_batch(request_id, refusal.code, &refusal.message),
        }
    }

    /// Reference `VibeCodeController.reset`, for a session that is going
    /// away: its runs stop and its picker is dropped.
    pub(crate) async fn reset_vibe_code(&self, session_id: &str) {
        let controller = self.lock_sessions().ok().and_then(|sessions| {
            sessions
                .get(session_id)
                .map(|session| Arc::clone(&session.vibe_code))
        });
        let Some(controller) = controller else {
            return;
        };
        let host = SessionHost {
            server: self.clone(),
            session_id: session_id.to_owned(),
            deliver: Arc::new(|_| {}),
            launch: ClientLaunch::default(),
        };
        controller.reset(&host).await;
    }
}

/// A fresh controller for a session being registered.
pub(crate) fn new_controller() -> Arc<VibeCodeController> {
    Arc::new(VibeCodeController::default())
}

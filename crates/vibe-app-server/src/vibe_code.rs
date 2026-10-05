//! `vibeCode/*`: the project picker a session links its repository through,
//! and the Teleport runs that hand the session to Vibe Code Web.
//!
//! Reference `VibeCodeController` (`vibe/app_server/_vibe_code.py`) with the
//! picker service (`vibe/core/vibe_code_project/picker_service.py`), the
//! orchestrator (`vibe/core/teleport/orchestrator.py`) and the run itself
//! (`vibe/core/teleport/teleport.py`). Each session owns one controller: one
//! picker at a time, identified by a fresh identifier and replaced by every
//! `open`, and any number of runs, each reserved on the session's execution
//! slot so nothing else runs beside it.
//!
//! What the controller needs from the session (its configuration, its
//! transcript, its execution slot, the account, the model, telemetry and the
//! wire) comes through [`Host`], which the server implements. Every sentence a
//! refusal or a failure carries is this port's own; the predicate that tells a
//! stale saved project from any other failure reads the status and the body the
//! service answered with, which those sentences quote.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::oneshot;
use vibe_core::events::ModelMessage;
use vibe_core::provider::AssistantMessage;
use vibe_core::telemetry::TelemetryRecord;
use vibe_core::telemetry::records::{
    ProjectPicker, ProjectSelectionSource, RemoteProjectOutcome, TeleportContextSummaryStatus,
    TeleportFailed, TeleportFailureStage, TeleportProgress, TeleportTracker,
    teleport_early_failure,
};
use vibe_protocol::ProtocolErrorCode;

pub(crate) mod git;
pub(crate) mod http;

use git::{FailureClass, GitFailure, GitRepoInfo, GitRepository, normalize_repo_url};
use http::{Project, ProjectClient};

/// The longest summary a session start carries (reference
/// `TELEPORT_MESSAGE_CONTEXT_MAX_LENGTH`).
const SUMMARY_MAX_CHARS: usize = 8_000;

/// How long tearing a controller down waits for its runs (reference
/// `SHUTDOWN_TIMEOUT_SECONDS`).
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the hosted chat lives when the configuration names nothing.
const DEFAULT_BASE_URL: &str = "https://chat.mistral.ai";

/// The request timeout a configuration that names none runs with.
const DEFAULT_API_TIMEOUT_SECONDS: f64 = 720.0;

pub(crate) type HostFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A refused request: the code the wire answers with and its sentence, with
/// no structured detail (reference `RequestFailure` with `data: null`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) code: ProtocolErrorCode,
    pub(crate) message: String,
}

impl Refusal {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: ProtocolErrorCode::InvalidParams,
            message: message.into(),
        }
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self {
            code: ProtocolErrorCode::Conflict,
            message: message.into(),
        }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            code: ProtocolErrorCode::Forbidden,
            message: message.into(),
        }
    }
}

/// A saved association between a checkout and a project (reference
/// `RemoteProjectLink`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedLink {
    pub(crate) repo_root: String,
    pub(crate) repo_url: String,
    pub(crate) project_id: String,
    pub(crate) project_name: String,
}

/// What a controller reads and drives of the session it belongs to
/// (reference `VibeCodeSession`).
pub(crate) trait Host: Send + Sync + 'static {
    /// Reference `SessionExecution.require_idle`.
    fn require_idle(&self) -> Result<(), Refusal>;
    /// Reference `SessionExecution.begin` for a Teleport run.
    fn begin_teleport(&self, operation_id: &str) -> Result<(), Refusal>;
    /// Releases the slot when `operation_id` still holds it.
    fn finish_teleport(&self, operation_id: &str);
    /// The merged configuration the session runs under.
    fn config(&self) -> toml::Table;
    /// The directory the session runs in.
    fn cwd(&self) -> PathBuf;
    fn session_id(&self) -> String;
    /// The session's transcript, system message first.
    fn messages(&self) -> Vec<ModelMessage>;
    /// `account/read`'s view.
    fn read_account(&self) -> HostFuture<'_, Value>;
    /// The credential a variable names, or `None`.
    fn credential(&self, variable: &str) -> Option<String>;
    /// One model call with no tools, on the model the session compacts with.
    fn summarize(
        &self,
        messages: Vec<ModelMessage>,
    ) -> HostFuture<'_, Result<AssistantMessage, String>>;
    fn record(&self, record: &TelemetryRecord);
    /// Publishes one `vibeCode/teleport/event`.
    fn notify(&self, event: Value);
    /// What the client that started the session declared about itself:
    /// its entrypoint and its name.
    fn launch(&self) -> (String, Option<String>);
    fn saved_link(&self, repo_root: &str) -> Option<SavedLink>;
    fn save_link(&self, link: &SavedLink) -> Result<(), String>;
    fn delete_link(&self, repo_root: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    Configure,
    Teleport,
}

impl Purpose {
    pub(crate) fn parse(value: Option<&str>) -> Self {
        match value {
            Some("teleport") => Self::Teleport,
            _ => Self::Configure,
        }
    }
}

/// The project API a picker was opened against (reference
/// `VibeCodeProjectPickerService`).
#[derive(Debug, Clone)]
pub(crate) struct Service {
    base_url: String,
    api_key: String,
    timeout: Duration,
}

impl Service {
    /// Reference `_build_service`: the endpoint and timeout the configuration
    /// names, under the Mistral provider's key, or `None` when no key
    /// resolves.
    pub(crate) fn from_config(
        config: &toml::Table,
        credential: impl Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        let api_key = configured_api_key(config, credential)?;
        let timeout = config
            .get("api_timeout")
            .and_then(|value| {
                value
                    .as_float()
                    .or_else(|| value.as_integer().map(|value| value as f64))
            })
            .filter(|seconds| *seconds > 0.0)
            .unwrap_or(DEFAULT_API_TIMEOUT_SECONDS);
        Some(Self {
            base_url: config_text(config, "vibe_code_sessions_base_url", DEFAULT_BASE_URL),
            api_key,
            timeout: Duration::from_secs_f64(timeout),
        })
    }

    pub(crate) fn client(&self) -> Result<ProjectClient, String> {
        ProjectClient::new(&self.base_url, &self.api_key, self.timeout)
    }

    pub(crate) async fn page(&self, cursor: Option<&str>) -> Result<http::Page, String> {
        self.client()?.list(cursor).await
    }
}

/// Reference `ProjectPickerContext`.
#[derive(Debug, Clone)]
struct Context {
    repo_root: String,
    repo_url: String,
    repo_name: String,
    saved_link: Option<SavedLink>,
}

/// Reference `VibeCodeProjectPickerState`.
#[derive(Debug, Clone)]
struct State {
    projects: Vec<Project>,
    next_cursor: Option<String>,
    repo_url: String,
}

/// The one picker a session holds.
struct Picker {
    id: String,
    purpose: Purpose,
    service: Service,
    git: GitRepoInfo,
    context: Context,
    state: State,
    selected: Option<String>,
    created: BTreeSet<String>,
    saved_link_cleared: bool,
    remote_changed: bool,
    /// What a Teleport run started from this picker reports about how its
    /// project was chosen (reference `_project_picker`).
    telemetry: Option<ProjectPicker>,
}

impl Picker {
    fn view(&self) -> Value {
        json!({
            "context": {
                "repoRoot": self.context.repo_root,
                "repoUrl": self.context.repo_url,
                "repoName": self.context.repo_name,
                "savedLink": self.context.saved_link.as_ref().map(link_view),
            },
            "state": {
                "projects": self.state.projects.iter().map(project_view).collect::<Vec<_>>(),
                "nextCursor": self.state.next_cursor,
                "repoUrl": self.state.repo_url,
            },
            "git": {
                "remoteName": self.git.remote_name,
                "remoteUrl": self.git.remote_url,
                "repo": self.git.repo,
                "branch": self.git.branch,
                "defaultBranch": self.git.default_branch,
            },
            "savedProjectLinkCleared": self.saved_link_cleared,
            "projectRepoRemoteChanged": self.remote_changed,
        })
    }

    /// Reference `build_project_picker_telemetry`, read at the moment of the
    /// decision.
    fn telemetry(&self, source: ProjectSelectionSource, shown: bool) -> ProjectPicker {
        ProjectPicker {
            shown,
            selection_source: Some(source),
            candidate_count_loaded: Some(self.state.projects.len() as u64),
            multi_repo_match_count: Some(multi_repo_matches(
                &self.state.projects,
                &self.context.repo_url,
            )),
            saved_project_link_cleared: Some(self.saved_link_cleared),
            repo_remote_changed: Some(self.remote_changed),
        }
    }
}

/// The runs a controller holds, by operation.
#[derive(Default)]
struct Runs {
    /// Runs the wire accepted and that start once the answer is out, with the
    /// picker payload they report.
    reserved: BTreeMap<String, ProjectPicker>,
    running: BTreeMap<String, Running>,
    /// The push questions waiting for `vibeCode/teleport/push/respond`.
    push: BTreeMap<String, oneshot::Sender<bool>>,
}

struct Running {
    cancel: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

/// What `vibeCode/teleport/start` asked for.
#[derive(Debug, Clone)]
pub(crate) struct StartParams {
    pub(crate) picker_id: String,
    pub(crate) operation_id: String,
    pub(crate) prompt: Option<String>,
    pub(crate) project_id: String,
}

#[derive(Default)]
pub(crate) struct VibeCodeController {
    /// The picker, behind the lock that serializes every picker call
    /// (reference `_picker_lock`).
    picker: tokio::sync::Mutex<Option<Picker>>,
    runs: Arc<Mutex<Runs>>,
}

impl std::fmt::Debug for VibeCodeController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("VibeCodeController")
    }
}

type Answer = Result<Value, Refusal>;

impl VibeCodeController {
    /// Reference `open`: the Teleport gate first, then the checkout and the
    /// first page, with a saved link resolved without listing when it still
    /// matches the remote.
    pub(crate) async fn open(
        &self,
        host: &Arc<dyn Host>,
        purpose: Purpose,
        prompt: Option<String>,
    ) -> Answer {
        host.require_idle()?;
        if purpose == Purpose::Teleport {
            require_teleport_available(host.as_ref(), prompt.as_deref()).await?;
        }
        let mut guard = self.picker.lock().await;
        *guard = None;
        let service = make_service(host.as_ref())?;
        let git = read_git(host.as_ref()).await?;
        let repo_root = git
            .repo_root
            .clone()
            .unwrap_or_else(|| host.cwd())
            .to_string_lossy()
            .into_owned();
        let mut context = Context {
            saved_link: host.saved_link(&repo_root),
            repo_root,
            repo_url: git.remote_url.clone(),
            repo_name: git.repo.clone(),
        };
        let matching_link = context.saved_link.as_ref().is_some_and(|link| {
            normalize_repo_url(&link.repo_url) == normalize_repo_url(&context.repo_url)
        });
        let (projects, next_cursor) = if purpose == Purpose::Teleport && matching_link {
            (Vec::new(), None)
        } else {
            service.page(None).await.map_err(Refusal::invalid)?
        };
        let mut resolved = None;
        let mut cleared = false;
        if purpose == Purpose::Teleport {
            if matching_link {
                resolved = context
                    .saved_link
                    .as_ref()
                    .map(|link| link.project_id.clone());
            } else if context.saved_link.is_some() {
                host.delete_link(&context.repo_root);
                context.saved_link = None;
                cleared = true;
            }
        }
        let mut picker = Picker {
            id: vibe_core::session_id::uuid_v4(),
            purpose,
            service,
            state: State {
                projects,
                next_cursor,
                repo_url: git.remote_url.clone(),
            },
            git,
            context,
            selected: resolved.clone(),
            created: BTreeSet::new(),
            saved_link_cleared: cleared,
            remote_changed: cleared,
            telemetry: None,
        };
        if resolved.is_some() {
            picker.telemetry = Some(picker.telemetry(ProjectSelectionSource::SavedLink, false));
        }
        let answer = json!({
            "pickerId": picker.id,
            "view": picker.view(),
            "resolvedProjectId": resolved,
        });
        *guard = Some(picker);
        Ok(answer)
    }

    /// Reference `load_more`: pages until one shows a project this
    /// repository can use, focusing it.
    pub(crate) async fn load_more(&self, host: &Arc<dyn Host>, picker_id: &str) -> Answer {
        host.require_idle()?;
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(picker_id))?;
        let Some(mut cursor) = picker.state.next_cursor.clone() else {
            return Ok(json!({"view": picker.view(), "focusOptionId": null}));
        };
        let mut projects = picker.state.projects.clone();
        let mut next_cursor;
        let mut focus;
        loop {
            let (page, page_cursor) = picker
                .service
                .page(Some(&cursor))
                .await
                .map_err(Refusal::invalid)?;
            next_cursor = page_cursor.clone();
            let visible = page.iter().find(|project| {
                !project.is_read_only
                    && (picker.state.repo_url.is_empty()
                        || is_project_linked_to_repo(project, &picker.state.repo_url))
            });
            focus = visible.map(|project| format!("project:{}", project.project_id));
            projects.extend(page);
            match (&focus, page_cursor) {
                (None, Some(following)) => cursor = following,
                _ => break,
            }
        }
        picker.state.projects = projects;
        picker.state.next_cursor = next_cursor;
        Ok(json!({"view": picker.view(), "focusOptionId": focus}))
    }

    /// Reference `create`: a new project for this repository, listed first
    /// and neither selected nor linked.
    pub(crate) async fn create(
        &self,
        host: &Arc<dyn Host>,
        picker_id: &str,
        name: &str,
        default_branch: &str,
    ) -> Answer {
        host.require_idle()?;
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(picker_id))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(Refusal::invalid("A project name is required."));
        }
        let default_branch = default_branch.trim();
        if default_branch.is_empty() {
            return Err(Refusal::invalid("A default branch is required."));
        }
        let project = picker
            .service
            .client()
            .map_err(Refusal::invalid)?
            .create(name, &picker.git.remote_url, default_branch)
            .await
            .map_err(Refusal::invalid)?;
        let mut projects = vec![project.clone()];
        projects.extend(
            picker
                .state
                .projects
                .iter()
                .filter(|existing| existing.project_id != project.project_id)
                .cloned(),
        );
        picker.state.projects = projects;
        picker.created.insert(project.project_id.clone());
        Ok(json!({"view": picker.view(), "project": project_view(&project)}))
    }

    /// Reference `select`: links the chosen project to the checkout.
    pub(crate) async fn select(
        &self,
        host: &Arc<dyn Host>,
        picker_id: &str,
        project_id: &str,
    ) -> Answer {
        host.require_idle()?;
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(picker_id))?;
        let project = picker
            .state
            .projects
            .iter()
            .find(|candidate| candidate.project_id == project_id)
            .cloned()
            .ok_or_else(|| {
                Refusal::invalid(format!(
                    "Vibe Code project {project_id} is not in the list."
                ))
            })?;
        if project.is_read_only || !is_project_linked_to_repo(&project, &picker.context.repo_url) {
            return Err(Refusal::invalid(
                "That Vibe Code project cannot be used with this repository.",
            ));
        }
        let link = SavedLink {
            repo_root: picker.context.repo_root.clone(),
            repo_url: picker.context.repo_url.clone(),
            project_id: project.project_id.clone(),
            project_name: project.name.clone(),
        };
        host.save_link(&link).map_err(Refusal::invalid)?;
        picker.context.saved_link = Some(link);
        picker.selected = Some(project.project_id.clone());
        let created = picker.created.contains(&project.project_id);
        let source = if created {
            ProjectSelectionSource::CreatedProject
        } else {
            ProjectSelectionSource::SelectedExisting
        };
        let payload = picker.telemetry(source, true);
        if picker.purpose == Purpose::Teleport {
            picker.telemetry = Some(payload);
        } else {
            host.record(&TelemetryRecord::RemoteProjectConfigured {
                outcome: if created {
                    RemoteProjectOutcome::Created
                } else {
                    RemoteProjectOutcome::Configured
                },
                picker: payload,
            });
        }
        Ok(json!({"view": picker.view(), "project": project_view(&project)}))
    }

    /// Reference `unlink`.
    pub(crate) async fn unlink(&self, host: &Arc<dyn Host>, picker_id: &str) -> Answer {
        host.require_idle()?;
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(picker_id))?;
        host.delete_link(&picker.context.repo_root);
        picker.saved_link_cleared = true;
        picker.selected = None;
        picker.context.saved_link = None;
        let payload = picker.telemetry(ProjectSelectionSource::SavedLink, true);
        if picker.purpose == Purpose::Teleport {
            picker_cancelled(host.as_ref(), payload);
            picker.telemetry = None;
        } else {
            host.record(&TelemetryRecord::RemoteProjectConfigured {
                outcome: RemoteProjectOutcome::Unlinked,
                picker: payload,
            });
        }
        Ok(json!({"view": picker.view()}))
    }

    /// Reference `cancel_picker`.
    pub(crate) async fn cancel(&self, host: &Arc<dyn Host>, picker_id: &str) -> Answer {
        host.require_idle()?;
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(picker_id))?;
        let payload = picker.telemetry(ProjectSelectionSource::Cancelled, true);
        if picker.purpose == Purpose::Teleport {
            picker_cancelled(host.as_ref(), payload);
        } else {
            host.record(&TelemetryRecord::RemoteProjectConfigured {
                outcome: RemoteProjectOutcome::Cancelled,
                picker: payload,
            });
        }
        *guard = None;
        Ok(json!({}))
    }

    /// Reference `recover_stale_link`: the link goes, and the first page is
    /// listed again.
    pub(crate) async fn recover(&self, host: &Arc<dyn Host>, picker_id: &str) -> Answer {
        host.require_idle()?;
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(picker_id))?;
        host.delete_link(&picker.context.repo_root);
        picker.saved_link_cleared = true;
        picker.selected = None;
        picker.telemetry = None;
        picker.context.saved_link = None;
        let Ok((projects, next_cursor)) = picker.service.page(None).await else {
            return Ok(json!({"recovered": false, "view": picker.view()}));
        };
        picker.state = State {
            projects,
            next_cursor,
            repo_url: picker.git.remote_url.clone(),
        };
        Ok(json!({"recovered": true, "view": picker.view()}))
    }

    /// Reference `reserve_teleport`: the run is checked against the picker
    /// and takes the session's execution slot before the start is answered.
    pub(crate) async fn reserve(&self, host: &Arc<dyn Host>, params: &StartParams) -> Answer {
        let mut guard = self.picker.lock().await;
        let picker = require_picker(&mut guard, Some(&params.picker_id))?;
        if picker.purpose != Purpose::Teleport {
            return Err(Refusal::conflict(
                "The open project picker was not opened for Teleport.",
            ));
        }
        if picker.selected.as_deref() != Some(params.project_id.as_str()) {
            return Err(Refusal::conflict(
                "The project to teleport to is not the one the picker selected.",
            ));
        }
        let Some(payload) = picker.telemetry else {
            return Err(Refusal::conflict(
                "No project has been chosen for Teleport yet.",
            ));
        };
        host.begin_teleport(&params.operation_id)?;
        lock(&self.runs)
            .reserved
            .insert(params.operation_id.clone(), payload);
        Ok(json!({"operationId": params.operation_id}))
    }

    /// Reference `start_teleport`: the reserved run starts, after its answer
    /// went out.
    pub(crate) fn start(&self, host: Arc<dyn Host>, params: StartParams) {
        let Some(payload) = lock(&self.runs).reserved.remove(&params.operation_id) else {
            return;
        };
        let (cancel, cancelled) = oneshot::channel();
        let operation_id = params.operation_id.clone();
        // The run removes itself under this lock when it ends, so it is
        // registered before it can get that far.
        let mut state = lock(&self.runs);
        let task = tokio::spawn(run_teleport(
            host,
            Arc::clone(&self.runs),
            params,
            payload,
            cancelled,
        ));
        state.running.insert(operation_id, Running { cancel, task });
    }

    /// Reference `respond_to_push`.
    pub(crate) fn respond_to_push(&self, operation_id: &str, approved: bool) -> Answer {
        let sender = lock(&self.runs)
            .push
            .remove(operation_id)
            .ok_or_else(|| Refusal::invalid("No Teleport run is waiting for a push answer."))?;
        let _ = sender.send(approved);
        Ok(json!({}))
    }

    /// Reference `cancel_teleport`: a reserved run is released, a running one
    /// is stopped and waited for, and anything else answers false.
    pub(crate) async fn cancel_teleport(&self, host: &Arc<dyn Host>, operation_id: &str) -> Answer {
        let running = {
            let mut runs = lock(&self.runs);
            if runs.reserved.remove(operation_id).is_some() {
                drop(runs);
                host.finish_teleport(operation_id);
                return Ok(json!({"cancelled": true}));
            }
            let Some(running) = runs.running.remove(operation_id) else {
                return Ok(json!({"cancelled": false}));
            };
            if let Some(push) = runs.push.remove(operation_id) {
                let _ = push.send(false);
            }
            running
        };
        let _ = running.cancel.send(());
        let _ = running.task.await;
        Ok(json!({"cancelled": true}))
    }

    /// Reference `reset`: every push answered no, every run stopped, every
    /// reservation released and the picker dropped.
    pub(crate) async fn reset(&self, host: &dyn Host) {
        let (running, reserved) = {
            let mut runs = lock(&self.runs);
            for (_, push) in std::mem::take(&mut runs.push) {
                let _ = push.send(false);
            }
            (
                std::mem::take(&mut runs.running),
                std::mem::take(&mut runs.reserved),
            )
        };
        let mut tasks = Vec::new();
        for (_, run) in running {
            let _ = run.cancel.send(());
            tasks.push(run.task);
        }
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, futures_util::future::join_all(tasks)).await;
        for operation_id in reserved.keys() {
            host.finish_teleport(operation_id);
        }
        *self.picker.lock().await = None;
    }
}

fn lock(runs: &Mutex<Runs>) -> std::sync::MutexGuard<'_, Runs> {
    runs.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn require_picker<'a>(
    guard: &'a mut Option<Picker>,
    picker_id: Option<&str>,
) -> Result<&'a mut Picker, Refusal> {
    let picker = guard
        .as_mut()
        .ok_or_else(|| Refusal::invalid("No Vibe Code project picker is open."))?;
    if picker_id.is_some_and(|id| id != picker.id) {
        return Err(Refusal::conflict(
            "That Vibe Code project picker was replaced by a newer one.",
        ));
    }
    Ok(picker)
}

/// Reference `_require_teleport_available`: a Mistral model, an account that
/// may teleport, and something to teleport, in that order, each refusal
/// reported as an early failure first.
async fn require_teleport_available(host: &dyn Host, prompt: Option<&str>) -> Result<(), Refusal> {
    let config = host.config();
    let messages = host.messages();
    let count = message_count(&messages);
    let fail_early = |stage, class: &str| host.record(&teleport_early_failure(stage, class, count));
    if !vibe_core::telemetry::is_active_model_mistral(&config) {
        fail_early(TeleportFailureStage::Ineligible, "TeleportIneligibleError");
        return Err(Refusal::forbidden(
            "Teleport only runs on a Mistral model. Switch to one with /model, then retry.",
        ));
    }
    let account = host.read_account().await;
    if account["teleportEligible"] != json!(true) {
        fail_early(TeleportFailureStage::Ineligible, "TeleportIneligibleError");
        let action = &account["teleportAction"];
        let url = action["url"].as_str().map_or_else(
            || {
                format!(
                    "{}/code/extensions?focus=key",
                    config_text(&config, "vibe_base_url", DEFAULT_BASE_URL).trim_end_matches('/')
                )
            },
            ToOwned::to_owned,
        );
        if action["kind"] == json!("switch_api_key") {
            return Err(Refusal::forbidden(format!(
                "Codestral API keys cannot teleport. Use a Vibe or workspace API key instead: {url}"
            )));
        }
        return Err(Refusal::forbidden(format!(
            "Teleport needs a Mistral API key that verifies, and this one did not. Review your \
             Mistral sign-in: {url}"
        )));
    }
    let has_history = messages
        .iter()
        .any(|message| !matches!(message, ModelMessage::System { .. }));
    if prompt.is_some_and(|prompt| !prompt.is_empty()) || has_history {
        return Ok(());
    }
    fail_early(TeleportFailureStage::NoHistory, "TeleportNoHistoryError");
    Err(Refusal::invalid(
        "There is no conversation to teleport yet.",
    ))
}

/// Reference `_make_service`.
fn make_service(host: &dyn Host) -> Result<Service, Refusal> {
    Service::from_config(&host.config(), |variable| host.credential(variable))
        .ok_or_else(|| Refusal::invalid("No Mistral API key is set."))
}

/// Reference `resolve_mistral_api_key`.
fn mistral_api_key(host: &dyn Host, config: &toml::Table) -> Option<String> {
    configured_api_key(config, |variable| host.credential(variable))
}

fn configured_api_key(
    config: &toml::Table,
    credential: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let provider = vibe_core::telemetry::mistral_provider(config)?;
    credential(provider.get("api_key_env_var")?.as_str()?)
}

fn config_text(config: &toml::Table, key: &str, default: &str) -> String {
    config
        .get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or(default)
        .to_owned()
}

/// Reference `_read_git`.
async fn read_git(host: &dyn Host) -> Result<GitRepoInfo, Refusal> {
    let repository = GitRepository::open(&host.cwd())
        .await
        .map_err(|failure| Refusal::invalid(failure.message))?;
    repository
        .info()
        .await
        .map_err(|failure| Refusal::invalid(failure.message))
}

/// Reference `session_message_count`: the transcript without its system
/// message.
fn message_count(messages: &[ModelMessage]) -> u64 {
    let system = usize::from(matches!(
        messages.first(),
        Some(ModelMessage::System { .. })
    ));
    messages.len().saturating_sub(system) as u64
}

fn picker_cancelled(host: &dyn Host, payload: ProjectPicker) {
    host.record(&TelemetryRecord::TeleportFailed(TeleportFailed {
        stage: TeleportFailureStage::Cancelled,
        error_class: "TeleportProjectPickerCancelledError".to_owned(),
        push_required: false,
        nb_session_messages: message_count(&host.messages()),
        context_summary: TeleportContextSummaryStatus::Skipped,
        context_summary_chars: None,
        failure_kind: None,
        http_status_code: None,
        picker: Some(payload),
    }));
}

/// Reference `is_project_linked_to_repo`.
pub(crate) fn is_project_linked_to_repo(project: &Project, repo_url: &str) -> bool {
    let current = normalize_repo_url(repo_url);
    project
        .repositories
        .iter()
        .any(|repository| normalize_repo_url(&repository.repo_url) == current)
}

/// Reference `count_multi_repo_matches`.
fn multi_repo_matches(projects: &[Project], repo_url: &str) -> u64 {
    if repo_url.is_empty() {
        return 0;
    }
    projects
        .iter()
        .filter(|project| {
            project.repositories.len() > 1 && is_project_linked_to_repo(project, repo_url)
        })
        .count() as u64
}

fn project_view(project: &Project) -> Value {
    json!({
        "projectId": project.project_id,
        "name": project.name,
        "repositories": project.repositories.iter().map(|repository| json!({
            "repoUrl": repository.repo_url,
            "defaultBranch": repository.default_branch,
        })).collect::<Vec<_>>(),
        "isReadOnly": project.is_read_only,
    })
}

fn link_view(link: &SavedLink) -> Value {
    json!({
        "repoRoot": link.repo_root,
        "repoUrl": link.repo_url,
        "projectId": link.project_id,
        "projectName": link.project_name,
    })
}

/// Reference `is_saved_project_stale_error`.
fn is_saved_project_stale(message: &str) -> bool {
    let normalized = message.to_lowercase();
    normalized.contains("project not found")
        || (normalized.contains("status 404") && normalized.contains("project"))
        || (normalized.contains("status 403") && normalized.contains("project"))
        || (normalized.contains("forbidden") && normalized.contains("project"))
}

/// Why a run stopped short of completing.
struct RunFailure {
    message: String,
    class: &'static str,
    failure_kind: Option<&'static str>,
    http_status_code: Option<u16>,
}

impl RunFailure {
    fn service(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            class: FailureClass::Service.name(),
            failure_kind: None,
            http_status_code: None,
        }
    }

    fn summary() -> Self {
        Self {
            message: "Summarizing the conversation for Teleport failed.".to_owned(),
            class: FailureClass::Service.name(),
            failure_kind: Some("context_summary_failed"),
            http_status_code: None,
        }
    }
}

impl From<GitFailure> for RunFailure {
    fn from(failure: GitFailure) -> Self {
        Self {
            message: failure.message,
            class: failure.class.name(),
            failure_kind: None,
            http_status_code: None,
        }
    }
}

/// What a run reports and how it waits on the operator.
struct RunContext {
    host: Arc<dyn Host>,
    runs: Arc<Mutex<Runs>>,
    operation_id: String,
    tracker: Mutex<TeleportTracker>,
}

impl RunContext {
    fn track(&self, update: impl FnOnce(&mut TeleportTracker)) {
        let mut tracker = self
            .tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update(&mut tracker);
    }

    fn event(&self, kind: &str, extra: Value) -> Value {
        let mut event = json!({"kind": kind, "operationId": self.operation_id});
        if let (Some(event), Value::Object(extra)) = (event.as_object_mut(), extra) {
            event.extend(extra);
        }
        event
    }

    /// One progress event: recorded, then published.
    fn emit(&self, progress: TeleportProgress, kind: &str, extra: Value) {
        self.track(|tracker| tracker.record_progress(progress));
        self.host.notify(self.event(kind, extra));
    }

    /// Reference: the push question goes out, and the run waits on its
    /// answer.
    async fn ask_push(&self, unpushed_count: u64, branch_not_pushed: bool) -> bool {
        let (sender, receiver) = oneshot::channel();
        lock(&self.runs)
            .push
            .insert(self.operation_id.clone(), sender);
        self.emit(
            TeleportProgress::PushRequired,
            "push_required",
            json!({"unpushedCount": unpushed_count, "branchNotPushed": branch_not_pushed}),
        );
        let approved = receiver.await.unwrap_or(false);
        lock(&self.runs).push.remove(&self.operation_id);
        approved
    }
}

/// Reference `_run_teleport` around `TeleportOrchestrator.execute`: the
/// events go out as the run makes progress, a failure ends it with a `failed`
/// event, and a cancellation ends it with nothing on the wire.
async fn run_teleport(
    host: Arc<dyn Host>,
    runs: Arc<Mutex<Runs>>,
    params: StartParams,
    picker: ProjectPicker,
    cancelled: oneshot::Receiver<()>,
) {
    let messages = host.messages();
    let resolved_prompt = resolve_prompt(params.prompt.as_deref(), &messages);
    let stage = if resolved_prompt.is_empty() {
        TeleportFailureStage::NoHistory
    } else {
        TeleportFailureStage::GitCheck
    };
    let context = RunContext {
        host: Arc::clone(&host),
        runs: Arc::clone(&runs),
        operation_id: params.operation_id.clone(),
        tracker: Mutex::new(TeleportTracker::new(
            message_count(&messages),
            stage,
            Some(picker),
        )),
    };
    let stop = async {
        if cancelled.await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    let outcome = tokio::select! {
        biased;
        () = stop => None,
        outcome = execute(&context, &params, &resolved_prompt, &messages) => Some(outcome),
    };
    match outcome {
        None => context.track(TeleportTracker::record_cancelled),
        Some(Ok(())) => {}
        Some(Err(failure)) => {
            context.track(|tracker| {
                tracker.record_service_error(
                    failure.class,
                    failure.failure_kind.map(ToOwned::to_owned),
                    failure.http_status_code.map(u64::from),
                );
            });
            let code = if is_saved_project_stale(&failure.message) {
                "saved_project_stale"
            } else {
                "teleport_failed"
            };
            host.finish_teleport(&params.operation_id);
            host.notify(context.event(
                "failed",
                json!({"error": {"message": failure.message, "code": code, "details": null}}),
            ));
        }
    }
    let failed = context
        .tracker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .failed();
    if let Some(record) = failed {
        host.record(&record);
    }
    let mut state = lock(&runs);
    state.push.remove(&params.operation_id);
    state.running.remove(&params.operation_id);
    drop(state);
    host.finish_teleport(&params.operation_id);
}

/// Reference `_resolve_teleport_prompt`: the prompt given, else the last
/// message the operator wrote.
fn resolve_prompt(prompt: Option<&str>, messages: &[ModelMessage]) -> String {
    if let Some(prompt) = prompt.filter(|prompt| !prompt.is_empty()) {
        return prompt.to_owned();
    }
    let context = vibe_core::compaction::context::select_model_context(messages);
    last_user_message(&context)
        .and_then(|index| context.get(index))
        .map(|message| message.content().to_owned())
        .unwrap_or_default()
}

/// Reference `_last_user_message_from`: the newest message the operator
/// wrote, by position.
fn last_user_message(messages: &[ModelMessage]) -> Option<usize> {
    messages.iter().rposition(|message| {
        matches!(
            message,
            ModelMessage::User {
                injected: false,
                ..
            }
        )
    })
}

/// Reference `_teleport_context_messages`: the model context, without the
/// message that stands for the prompt when none was given.
fn context_messages(prompt: Option<&str>, messages: &[ModelMessage]) -> Vec<ModelMessage> {
    let mut context = vibe_core::compaction::context::select_model_context(messages);
    if prompt.is_none_or(str::is_empty)
        && let Some(index) = last_user_message(&context)
    {
        context.remove(index);
    }
    context
}

/// Reference `_is_teleport_context_message`.
fn carries_context(message: &ModelMessage) -> bool {
    match message {
        ModelMessage::System { .. } => false,
        ModelMessage::User {
            content,
            attachments,
            ..
        } => !content.is_empty() || !attachments.is_empty(),
        ModelMessage::Assistant {
            content,
            reasoning,
            tool_calls,
            ..
        } => {
            !content.is_empty()
                || reasoning
                    .as_deref()
                    .is_some_and(|reasoning| !reasoning.is_empty())
                || !tool_calls.is_empty()
        }
        ModelMessage::Tool { .. } => true,
    }
}

/// The orchestrator and the run: summary, checkout, push, start.
async fn execute(
    context: &RunContext,
    params: &StartParams,
    resolved_prompt: &str,
    messages: &[ModelMessage],
) -> Result<(), RunFailure> {
    let host = context.host.as_ref();
    let prompt = params.prompt.as_deref();
    let mut message_context = None;
    if !resolved_prompt.is_empty() {
        let source = context_messages(prompt, messages);
        if source.iter().any(carries_context) {
            context.emit(
                TeleportProgress::SummarizingContext,
                "summarizing_context",
                Value::Null,
            );
            let summary = match summarize(host, source, resolved_prompt).await {
                Ok(summary) => summary,
                Err(failure) => {
                    context.track(TeleportTracker::record_context_summary_failed);
                    return Err(failure);
                }
            };
            if summary.chars().count() > SUMMARY_MAX_CHARS {
                context.track(TeleportTracker::record_context_summary_failed);
            } else {
                let chars = summary.chars().count() as u64;
                context.track(|tracker| tracker.record_context_summary_generated(chars));
                let (entrypoint, client_name) = host.launch();
                message_context = Some(http::MessageContext {
                    summary,
                    source: http::MessageContextSource {
                        kind: "teleport",
                        entrypoint,
                        client_name,
                    },
                });
            }
        }
    }
    if resolved_prompt.is_empty() {
        return Err(RunFailure::service(
            "Teleport needs a prompt that is not empty.",
        ));
    }
    let config = host.config();
    let api_key = mistral_api_key(host, &config)
        .ok_or_else(|| RunFailure::service("No Mistral API key is set."))?;
    let project_id = params.project_id.trim();
    if project_id.is_empty() {
        return Err(RunFailure::service(
            "Teleport needs a Vibe Code project id.",
        ));
    }
    let repository = GitRepository::open(&host.cwd()).await?;
    let git = repository.info().await?;
    let Some(branch) = git.branch.clone() else {
        return Err(RunFailure::service(
            "Teleport needs a checked-out branch, not a detached HEAD.",
        ));
    };
    let remote = git.remote_name.clone();
    context.emit(TeleportProgress::CheckingGit, "checking_git", Value::Null);
    repository.fetch(&remote).await;
    let commit_pushed = repository.is_commit_pushed(&git.commit, &remote).await;
    let branch_pushed = repository.is_branch_pushed(&remote).await;
    if !commit_pushed || !branch_pushed {
        let unpushed = repository.unpushed_commit_count(&remote).await?;
        if !context.ask_push(unpushed.max(1), !branch_pushed).await {
            return Err(RunFailure::service(
                "Teleport was canceled: the changes were not pushed.",
            ));
        }
        context.emit(TeleportProgress::Pushing, "pushing", Value::Null);
        if !repository.push_current_branch(&remote).await {
            return Err(RunFailure::service(format!(
                "Pushing the current branch to {remote} failed."
            )));
        }
    }
    context.emit(
        TeleportProgress::StartingWorkflow,
        "starting_workflow",
        Value::Null,
    );
    let diff = compress_diff(&git.diff)?;
    let request = http::StartRequest {
        project_id: project_id.to_owned(),
        source: "vibe_code_cli",
        idempotency_key: vibe_core::session_id::uuid_v4(),
        conversation_id: Some(host.session_id()),
        message: http::StartMessage {
            role: "user",
            parts: vec![http::TextPart {
                kind: "text",
                text: resolved_prompt.to_owned(),
            }],
        },
        context: http::StartContext {
            repositories: vec![http::StartRepository {
                repo_url: git.remote_url.clone(),
                branch: Some(branch),
                commit_sha: Some(git.commit.clone()),
                diff,
            }],
            message_context,
        },
    };
    let base_url = config_text(&config, "vibe_code_sessions_base_url", DEFAULT_BASE_URL);
    let url = match http::start_session(&base_url, &api_key, &request).await {
        Ok(url) => url,
        Err(failure) => {
            if is_saved_project_stale(&failure.message)
                && let Some(root) = &git.repo_root
            {
                host.delete_link(&root.to_string_lossy());
            }
            return Err(RunFailure {
                message: failure.message,
                class: failure.class.name(),
                failure_kind: failure.failure_kind,
                http_status_code: failure.http_status_code,
            });
        }
    };
    context.track(|tracker| tracker.record_progress(TeleportProgress::Complete));
    host.record(
        &context
            .tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .completed(),
    );
    host.finish_teleport(&context.operation_id);
    host.notify(context.event("complete", json!({"url": url})));
    Ok(())
}

/// Reference `_summarize_teleport_context`: one call with no tools over the
/// context and a request naming the prompt, answered by a `<summary>` or by
/// the whole reply.
async fn summarize(
    host: &dyn Host,
    mut messages: Vec<ModelMessage>,
    resolved_prompt: &str,
) -> Result<String, RunFailure> {
    messages.push(ModelMessage::user(summary_request(resolved_prompt)));
    let answer = host
        .summarize(messages)
        .await
        .map_err(|_| RunFailure::summary())?;
    let text = answer.text.trim();
    if !answer.tool_calls.is_empty() || text.is_empty() {
        return Err(RunFailure::summary());
    }
    Ok(vibe_core::compaction::context::extract_summary(text).unwrap_or_else(|| text.to_owned()))
}

/// What the summarization is asked: what the next agent needs, never the
/// prompt again, and the size the start accepts.
fn summary_request(resolved_prompt: &str) -> String {
    let base = vibe_core::compaction::manager::CompactionPrompts::builtin().request;
    format!(
        "{base}\n\n## Handing off to Vibe Code Web\nWrite the context a Vibe Code Web agent \
         needs to pick this task up where it stands: decisions taken, work in progress, and \
         what remains. The prompt below starts that session on its own, so leave it out of the \
         summary. Stay below {SUMMARY_MAX_CHARS} characters.\n\n<teleported_prompt>\n{}\n\
         </teleported_prompt>",
        escape_markup(resolved_prompt)
    )
}

/// Reference `html.escape(..., quote=False)`.
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Reference `_compress_diff`: zstd, then base64, refused past one million
/// encoded bytes.
fn compress_diff(diff: &[u8]) -> Result<Option<http::StartDiff>, RunFailure> {
    use base64::Engine as _;
    if diff.is_empty() {
        return Ok(None);
    }
    let compressed =
        zstd::bulk::compress(diff, 3).map_err(|error| RunFailure::service(error.to_string()))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(compressed);
    if encoded.len() > 1_000_000 {
        return Err(RunFailure::service(
            "The uncommitted changes are too large to teleport. Commit and push them first.",
        ));
    }
    Ok(Some(http::StartDiff {
        format: "git-diff",
        encoding: "base64",
        compression: "zstd",
        content: encoded,
    }))
}

#[cfg(test)]
mod vibe_code_tests;

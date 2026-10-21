//! The Teleport runs that hand a session to Vibe Code Web: the gate a
//! Teleport picker opens behind, the reservation on the session's execution
//! slot, and the run itself (reference the orchestrator,
//! `vibe/core/teleport/orchestrator.py`, and the run,
//! `vibe/core/teleport/teleport.py`, driven by `VibeCodeController` in
//! `vibe/app_server/_vibe_code.py`).
//!
//! A controller holds any number of runs, each reserved on the session's
//! execution slot so nothing else runs beside it. Every lock of the state the
//! runs share goes through [`lock`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::sync::lock;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::oneshot;
use vibe_core::events::ModelMessage;
use vibe_core::telemetry::records::{
    ProjectPicker, TeleportFailureStage, TeleportProgress, TeleportTracker, teleport_early_failure,
};

use super::git::{FailureClass, GitFailure, GitRepository};
use super::http;
use super::picker::require_picker;
use super::{
    Answer, DEFAULT_BASE_URL, Host, Refusal, StartParams, VibeCodeController, config_text,
    configured_api_key, message_count,
};

/// The longest summary a session start carries (reference
/// `TELEPORT_MESSAGE_CONTEXT_MAX_LENGTH`).
const SUMMARY_MAX_CHARS: usize = 8_000;

/// How long tearing a controller down waits for its runs (reference
/// `SHUTDOWN_TIMEOUT_SECONDS`).
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The runs a controller holds, by operation.
#[derive(Default)]
pub(super) struct Runs {
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

impl VibeCodeController {
    /// Reference `reserve_teleport`: the run is checked against the picker
    /// and takes the session's execution slot before the start is answered.
    pub(crate) async fn reserve(&self, host: &Arc<dyn Host>, params: &StartParams) -> Answer {
        let mut guard = self.picker.lock().await;
        let payload = require_picker(&mut guard, Some(&params.picker_id))?
            .teleport_choice(&params.project_id)?;
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

/// Reference `_require_teleport_available`: a Mistral model, an account that
/// may teleport, and something to teleport, in that order, each refusal
/// reported as an early failure first.
pub(super) async fn require_teleport_available(
    host: &dyn Host,
    prompt: Option<&str>,
) -> Result<(), Refusal> {
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

/// Reference `resolve_mistral_api_key`.
fn mistral_api_key(host: &dyn Host, config: &toml::Table) -> Option<String> {
    configured_api_key(config, |variable| host.credential(variable))
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
        update(&mut lock(&self.tracker));
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
    let failed = lock(&context.tracker).failed();
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
    host.record(&lock(&context.tracker).completed());
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
mod teleport_tests;

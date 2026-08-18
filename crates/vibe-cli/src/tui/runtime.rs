//! The interactive session runtime: the live service handle plus the mutable
//! session intent the UI reads and writes.
//!
//! Interactive calls that must not block the frame loop are scheduled from here,
//! and only one may be in flight at a time.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use vibe_app_server::client::{HeadlessService, LiveTurnDriver, PublicDispatch};
use vibe_app_server::release3::Release3Service;
use vibe_core::telemetry::TelemetryRecord;
use vibe_core::telemetry::records::{ProjectPicker, TeleportTracker};

use super::chat_input::Safety;
use super::clipboard_images::{ImageModel, ImageModels};
use super::cloud_workflow::CloudWorkflowState;
use super::interaction::{self, Overlay};
use super::shell::ActiveShell;
use super::state::TuiState;
use super::voice::{SpeechManager, VoiceManager};
use super::{
    CliTelemetryObserver, apply_public_notifications, remote_project_workflow, switching, workflow,
};

#[derive(Debug, Clone)]
pub(super) struct RuntimeSkill {
    pub(super) name: String,
    pub(super) description: String,
}

pub(super) struct InteractiveRuntime {
    pub(super) service: HeadlessService<LiveTurnDriver>,
    /// This session's enrollment, held so the lookup it detached can be
    /// cancelled before the session closes. Reference `AgentLoop` holds its
    /// experiments task for the same reason.
    pub(super) experiments: Option<Arc<vibe_app_server::experiments::SessionExperiments>>,
    /// The configuration service this process already runs on.
    ///
    /// `ConfigReadResponse` publishes a narrow view and declares no room for the
    /// layers, targets and unregistered keys the settings screen renders, so the
    /// screen reads the effective document from the same store the server writes
    /// through rather than from a wire shape that does not carry it.
    pub(super) release3: Release3Service,
    pub(super) session_id: String,
    pub(super) model: String,
    pub(super) image_models: ImageModels,
    pub(super) thinking: String,
    pub(super) mode: String,
    pub(super) agent_name: String,
    pub(super) safety: Safety,
    pub(super) banner: BannerMetrics,
    pub(super) context_tokens: u64,
    pub(super) context_window: u64,
    pub(super) auto_approve: bool,
    pub(super) vibe_code_enabled: bool,
    pub(super) config_target: Option<interaction::ConfigLayerTarget>,
    pub(super) remote_project_overlay: Option<Overlay>,
    pub(super) remote_project_draft: Option<interaction::RemoteProjectDraft>,
    pub(super) ui_operation_sender:
        Option<tokio::sync::mpsc::UnboundedSender<UiOperationCompletion>>,
    pub(super) ui_operation_generation: u64,
    pub(super) active_ui_operation: Option<u64>,
    pub(super) skills: BTreeMap<String, RuntimeSkill>,
    pub(super) shell: Option<ActiveShell>,
    pub(super) cloud: CloudWorkflowState,
    pub(super) pending_switch: Option<switching::SwitchRequest>,
    pub(super) telemetry: Option<Arc<CliTelemetryObserver>>,
    /// What the project picker reported about itself, carried into the
    /// teleport and remote-project events. Reference
    /// `build_project_picker_telemetry`, whose payload is built where the
    /// picker opens and completed where the operator answers it.
    pub(super) project_picker: Option<ProjectPicker>,
    /// The teleport run in flight, and the stage machine it walks. Reference
    /// `TeleportTelemetryTracker`.
    pub(super) teleport_telemetry: Option<TeleportTracker>,
    /// How long `session/new` took, which is one of the three durations
    /// `vibe.startup` reports. Reference
    /// `resources.runtime.session_init_duration_ms`.
    pub(super) session_init_duration_ms: Option<u64>,
    pub(super) voice: VoiceManager,
    /// The read-aloud transport, resolved from the same published view the
    /// transcription session is.
    pub(super) speech: SpeechManager,
}

#[derive(Debug, Clone)]
pub(super) enum UiOperation {
    Mcp(workflow::McpPendingOperation),
    RemoteProject(remote_project_workflow::ProjectPendingOperation),
}

pub(super) struct UiOperationCompletion {
    pub(super) generation: u64,
    pub(super) operation: UiOperation,
    pub(super) result: Result<PublicDispatch, String>,
}

/// The `ConfigView` a session publishes.
///
/// Every screen that renders a preference reads it here, so a caller never
/// re-spells the request or the field the response wraps it in.
pub(super) fn published_config_view(
    service: &mut HeadlessService<LiveTurnDriver>,
    session_id: &str,
) -> Option<Value> {
    service
        .public_call("config/read", json!({"sessionId": session_id}))
        .ok()?
        .get("config")
        .cloned()
}

pub(super) fn schedule_ui_call(
    runtime: &mut InteractiveRuntime,
    method: &str,
    mut params: Value,
    operation: UiOperation,
    state: &mut TuiState,
) -> bool {
    if runtime.active_ui_operation.is_some() {
        state.push_diagnostic("An interactive operation is already in progress");
        return false;
    }
    let Some(sender) = runtime.ui_operation_sender.clone() else {
        state.push_diagnostic("Interactive operation channel is unavailable");
        return false;
    };
    let Some(params) = params.as_object_mut() else {
        state.push_diagnostic("Interactive operation parameters must be an object");
        return false;
    };
    params
        .entry("sessionId")
        .or_insert_with(|| json!(runtime.session_id));
    let pending = match runtime
        .service
        .begin_public_call(method, Value::Object(params.clone()))
    {
        Ok(pending) => pending,
        Err(error) => {
            state.push_diagnostic(error.to_string());
            return false;
        }
    };
    runtime.ui_operation_generation = runtime.ui_operation_generation.saturating_add(1);
    let generation = runtime.ui_operation_generation;
    runtime.active_ui_operation = Some(generation);
    tokio::spawn(async move {
        let result = tokio::time::timeout(Duration::from_secs(30), pending.complete())
            .await
            .map_err(|_| "Interactive operation timed out".to_owned())
            .and_then(|result| result.map_err(|error| error.to_string()));
        let _ = sender.send(UiOperationCompletion {
            generation,
            operation,
            result,
        });
    });
    true
}

pub(super) fn schedule_ui_external<F>(
    runtime: &mut InteractiveRuntime,
    operation: UiOperation,
    work: F,
    state: &mut TuiState,
) -> bool
where
    F: Future<Output = Result<(), String>> + Send + 'static,
{
    if runtime.active_ui_operation.is_some() {
        state.push_diagnostic("An interactive operation is already in progress");
        return false;
    }
    let Some(sender) = runtime.ui_operation_sender.clone() else {
        state.push_diagnostic("Interactive operation channel is unavailable");
        return false;
    };
    runtime.ui_operation_generation = runtime.ui_operation_generation.saturating_add(1);
    let generation = runtime.ui_operation_generation;
    runtime.active_ui_operation = Some(generation);
    tokio::spawn(async move {
        let result = work.await.map(|()| PublicDispatch {
            result: BTreeMap::new(),
            notifications: Vec::new(),
        });
        let _ = sender.send(UiOperationCompletion {
            generation,
            operation,
            result,
        });
    });
    true
}

pub(super) fn apply_ui_operation_completion(
    completion: UiOperationCompletion,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
) {
    let Some(runtime) = runtime.as_mut() else {
        return;
    };
    if runtime.active_ui_operation != Some(completion.generation) {
        return;
    }
    runtime.active_ui_operation = None;
    if let Ok(dispatch) = &completion.result {
        apply_public_notifications(dispatch, runtime, state);
    }
    match completion.operation {
        UiOperation::Mcp(operation) => {
            workflow::apply_pending_operation(operation, completion.result, runtime, state);
        }
        UiOperation::RemoteProject(operation) => {
            remote_project_workflow::apply_pending_operation(
                operation,
                completion.result,
                runtime,
                state,
            );
        }
    }
}

impl InteractiveRuntime {
    /// Sends one telemetry record under this session.
    ///
    /// Telemetry is best effort on both sides, so a session that enabled none
    /// and a delivery that failed are the same thing here: nothing is reported
    /// to the operator, because a census failure is not news about their turn.
    pub(super) fn report(&self, record: &TelemetryRecord) {
        if let Some(telemetry) = self.telemetry.as_ref() {
            let _ = telemetry.enqueue(record, Some(self.session_id.as_str()));
        }
    }

    /// The `ConfigView` this session publishes.
    pub(super) fn published_config(&mut self) -> Option<Value> {
        let session_id = self.session_id.clone();
        published_config_view(&mut self.service, &session_id)
    }

    pub(super) fn image_model(&self) -> ImageModel<'_> {
        self.image_models.get(&self.model)
    }

    pub(super) fn supports_images(&self) -> bool {
        self.image_model().supports_images
    }
}

#[derive(Debug, Clone)]
pub(super) struct BannerMetrics {
    pub(super) models_count: usize,
    pub(super) skills_count: usize,
    pub(super) mcp_servers_enabled: usize,
    pub(super) mcp_servers_total: usize,
    pub(super) connectors_connected: usize,
    pub(super) connectors_total: usize,
    pub(super) hooks_count: usize,
    pub(super) plan: Option<String>,
}

impl Default for BannerMetrics {
    fn default() -> Self {
        Self {
            models_count: 1,
            skills_count: 0,
            mcp_servers_enabled: 0,
            mcp_servers_total: 0,
            connectors_connected: 0,
            connectors_total: 0,
            hooks_count: 0,
            plan: None,
        }
    }
}

pub(super) fn teleport_available(runtime: Option<&InteractiveRuntime>) -> bool {
    runtime.is_some_and(|runtime| runtime.vibe_code_enabled)
}

#[cfg(test)]
pub(in crate::tui) fn interactive_test_runtime(session_id: &str) -> InteractiveRuntime {
    interactive_test_runtime_with_server(session_id, vibe_app_server::server::AppServer::default())
}

/// A runtime wired to an in-process server. Tests that need a specific backend
/// pass their own server rather than reaching into the runtime afterward.
#[cfg(test)]
pub(in crate::tui) fn interactive_test_runtime_with_server(
    session_id: &str,
    server: vibe_app_server::server::AppServer,
) -> InteractiveRuntime {
    interactive_test_runtime_with_trust(session_id, server, true)
}

/// The same runtime with the session's workspace trust chosen by the caller.
///
/// `config/*` is dispatched against the attached session's trust, so a test
/// that needs a project write refused asks for an untrusted session rather than
/// relying on a filesystem the write would fail on for another reason.
#[cfg(test)]
pub(in crate::tui) fn interactive_test_runtime_with_trust(
    session_id: &str,
    server: vibe_app_server::server::AppServer,
    trusted: bool,
) -> InteractiveRuntime {
    use vibe_app_server::client::{LiveDriverConfig, SessionOptions};
    use vibe_core::compaction::manager::CompactionPromptResolution;

    let driver = Arc::new(
        LiveTurnDriver::from_credential(
            LiveDriverConfig {
                style: "mistral".to_owned(),
                endpoint: "http://127.0.0.1:1/v1".to_owned(),
                model: "test-model".to_owned(),
                credential_environment: "TEST_CREDENTIAL".to_owned(),
                system_prompt: "test".to_owned(),
                session_root: None,
                compaction_prompts: CompactionPromptResolution::default(),
                input_price_per_million_micros: 0,
                output_price_per_million_micros: 0,
            },
            "test-credential".to_owned(),
        )
        .expect("test driver"),
    );
    let mut service =
        HeadlessService::new_interactive_shared_with_server(driver, server).expect("test service");
    let session_id = service
        .start_session(&SessionOptions {
            working_directory: "/workspace".to_owned(),
            session_id: Some(session_id.to_owned()),
            add_directories: Vec::new(),
            trusted,
            agent: None,
            tool_filters: Vec::new(),
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            mcp_servers: Vec::new(),
            model: Some("test-model".to_owned()),
            max_turns: None,
            max_tokens: None,
            max_price_micros: None,
            mode: None,
            thinking: false,
            reasoning_effort: None,
            auto_approve: true,
            resume: None,
            continue_session: false,
        })
        .expect("session starts");
    InteractiveRuntime {
        experiments: None,
        service,
        release3: Release3Service::default(),
        session_id,
        model: "test-model".to_owned(),
        image_models: {
            let mut models = ImageModels::default();
            models.insert("test-model", true);
            models
        },
        thinking: "off".to_owned(),
        mode: "code".to_owned(),
        agent_name: "default".to_owned(),
        safety: Safety::Neutral,
        banner: BannerMetrics::default(),
        context_tokens: 0,
        context_window: super::DEFAULT_CONTEXT_WINDOW,
        auto_approve: true,
        vibe_code_enabled: true,
        config_target: None,
        remote_project_overlay: None,
        remote_project_draft: None,
        ui_operation_sender: None,
        ui_operation_generation: 0,
        active_ui_operation: None,
        skills: BTreeMap::new(),
        shell: None,
        cloud: CloudWorkflowState::default(),
        pending_switch: None,
        telemetry: None,
        project_picker: None,
        teleport_telemetry: None,
        session_init_duration_ms: None,
        voice: VoiceManager::production(
            &json!({
                "transcription": {
                    "model": {
                        "name": "test-transcribe-model",
                        "sampleRate": 16_000,
                        "encoding": "pcm_s16le",
                        "targetStreamingDelayMs": 500,
                    },
                    "provider": {"apiBase": "https://provider.invalid", "apiKeyEnvVar": ""},
                },
            }),
            "test-credential",
            std::path::Path::new("/nonexistent-vibe-home"),
            false,
        ),
        speech: SpeechManager::production(
            &json!({
                "speech": {
                    "model": {
                        "name": "test-speech-model",
                        "voice": "test-voice",
                        "responseFormat": "wav",
                    },
                    "provider": {"apiBase": "https://provider.invalid", "apiKeyEnvVar": ""},
                },
            }),
            "test-credential",
            std::path::Path::new("/nonexistent-vibe-home"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[tokio::test]
    async fn ui_operations_are_mutually_exclusive_and_session_scoped() {
        let mut runtime = interactive_test_runtime("ui-operation-session");
        let (operation_sender, mut operation_receiver) = tokio::sync::mpsc::unbounded_channel();
        runtime.ui_operation_sender = Some(operation_sender);
        let mut state = TuiState::new("ui-operation-session");
        let rejected_work_ran = Arc::new(AtomicBool::new(false));

        assert!(schedule_ui_call(
            &mut runtime,
            "config/read",
            json!({}),
            UiOperation::Mcp(workflow::McpPendingOperation::CopyUrl),
            &mut state,
        ));
        let rejected_work = Arc::clone(&rejected_work_ran);
        assert!(!schedule_ui_external(
            &mut runtime,
            UiOperation::Mcp(workflow::McpPendingOperation::CopyUrl),
            async move {
                rejected_work.store(true, Ordering::SeqCst);
                Ok(())
            },
            &mut state,
        ));
        assert_eq!(runtime.ui_operation_generation, 1);
        assert!(
            state
                .diagnostics()
                .any(|message| message.contains("already in progress"))
        );

        let completion = tokio::time::timeout(Duration::from_secs(1), operation_receiver.recv())
            .await
            .expect("config read completes")
            .expect("operation channel remains open");
        assert!(completion.result.is_ok(), "sessionId was injected");
        let mut runtime = Some(runtime);
        apply_ui_operation_completion(completion, &mut runtime, &mut state);
        assert_eq!(
            runtime
                .as_ref()
                .expect("runtime remains mounted")
                .active_ui_operation,
            None
        );
        assert!(!rejected_work_ran.load(Ordering::SeqCst));
    }

    #[test]
    fn teleport_mode_uses_runtime_capability_instead_of_runtime_presence() {
        let mut runtime = interactive_test_runtime("teleport-capability-session");
        assert!(teleport_available(Some(&runtime)));
        runtime.vibe_code_enabled = false;
        assert!(!teleport_available(Some(&runtime)));
        assert!(!teleport_available(None));
    }
}

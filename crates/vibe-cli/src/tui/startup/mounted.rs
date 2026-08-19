use std::path::Path;

use crate::CliError;

use super::super::chat_input::ChatInputState;
use super::super::clipboard_images::ClipboardImageManager;
use super::super::composer::apply_effects as apply_composer_effects;
use super::super::controls::ControlState;
use super::super::prompt::{PromptContext, start_prompt};
use super::super::state::{EntryStatus, TuiState};
use super::super::{ActiveTurn, InteractiveRuntime, push_local_notice, start_teleport};
use super::PostMountAction;

pub(in crate::tui) enum MountedStartup {
    Pending(Option<PostMountAction>),
    Ready,
    FatalPendingRender(CliError),
    FatalAwaitingKey(CliError),
}

impl MountedStartup {
    pub(in crate::tui) const fn new(action: Option<PostMountAction>) -> Self {
        Self::Pending(action)
    }

    pub(in crate::tui) const fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::FatalPendingRender(_) | Self::FatalAwaitingKey(_)
        )
    }

    pub(in crate::tui) const fn is_awaiting_fatal_key(&self) -> bool {
        matches!(self, Self::FatalAwaitingKey(_))
    }

    pub(in crate::tui) const fn needs_fatal_render(&self) -> bool {
        matches!(self, Self::FatalPendingRender(_))
    }

    pub(in crate::tui) fn arm_fatal_acknowledgment(&mut self) {
        let current = std::mem::replace(self, Self::Ready);
        *self = match current {
            Self::FatalPendingRender(error) => Self::FatalAwaitingKey(error),
            current => current,
        };
    }

    pub(in crate::tui) fn into_initialization_error(self) -> Option<CliError> {
        match self {
            Self::FatalPendingRender(error) | Self::FatalAwaitingKey(error) => Some(error),
            Self::Pending(_) | Self::Ready => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::tui) async fn complete_mounted_startup(
    startup: &mut MountedStartup,
    working_directory: &Path,
    runtime: &mut Option<InteractiveRuntime>,
    active: &mut Option<ActiveTurn>,
    state: &mut TuiState,
    controls: &mut ControlState,
    input: &mut ChatInputState,
    clipboard_images: &mut ClipboardImageManager,
) -> Result<(), CliError> {
    let action = match std::mem::replace(startup, MountedStartup::Ready) {
        MountedStartup::Pending(action) => action,
        current @ (MountedStartup::Ready
        | MountedStartup::FatalPendingRender(_)
        | MountedStartup::FatalAwaitingKey(_)) => {
            *startup = current;
            return Ok(());
        }
    };

    let initialization = if let Some(runtime) = runtime.as_mut() {
        let session_id = runtime.session_id.clone();
        runtime
            .service
            .initialize_pending_mcp(&session_id)
            .await
            .map_err(CliError::from)
    } else {
        Ok(Vec::new())
    };
    state.waiting = false;
    if !record_initialization(startup, state, initialization) {
        return Ok(());
    }

    match action {
        Some(PostMountAction::Prompt(prompt)) => {
            dispatch_initial_prompt(
                prompt,
                working_directory,
                runtime,
                active,
                state,
                controls,
                input,
                clipboard_images,
            )
            .await?;
        }
        Some(PostMountAction::Teleport(prompt)) => {
            if let Some(runtime) = runtime.as_mut() {
                if runtime.vibe_code_enabled {
                    start_teleport(prompt.as_deref(), working_directory, runtime, state);
                } else {
                    state.push_diagnostic(
                        "Startup Teleport is unavailable in the active configuration",
                    );
                }
            } else {
                state.push_diagnostic(
                    "Startup Teleport could not start because setup is incomplete",
                );
            }
        }
        None => {}
    }
    Ok(())
}

fn record_initialization(
    startup: &mut MountedStartup,
    state: &mut TuiState,
    initialization: Result<Vec<String>, CliError>,
) -> bool {
    match initialization {
        Ok(diagnostics) => {
            for diagnostic in diagnostics {
                push_local_notice(
                    state,
                    &format!("MCP server failed to connect: {diagnostic}"),
                    EntryStatus::Failed,
                );
            }
            true
        }
        Err(error) => {
            push_local_notice(
                state,
                &format!("Background initialization failed: {error}"),
                EntryStatus::Failed,
            );
            push_local_notice(state, "Press any key to exit", EntryStatus::Completed);
            *startup = MountedStartup::FatalPendingRender(error);
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_initial_prompt(
    prompt: String,
    working_directory: &Path,
    runtime: &mut Option<InteractiveRuntime>,
    active: &mut Option<ActiveTurn>,
    state: &mut TuiState,
    controls: &mut ControlState,
    input: &mut ChatInputState,
    clipboard_images: &mut ClipboardImageManager,
) -> Result<(), CliError> {
    if prompt.trim().is_empty() {
        state.push_diagnostic("Initial prompt is empty; no turn was submitted");
        return Ok(());
    }
    if runtime.is_none() {
        state.push_diagnostic("Initial prompt could not start because setup is incomplete");
        return Ok(());
    }
    let draft = clipboard_images.draft(working_directory, prompt);
    if !start_prompt(
        PromptContext::new(
            working_directory,
            runtime,
            active,
            state,
            controls,
            clipboard_images,
        ),
        &draft,
    )
    .await?
    {
        input.replace_text(draft.into_text());
        let effects = input.refresh_after_adapter_mutation();
        apply_composer_effects(input, effects, working_directory, state);
        state.push_diagnostic("Initial prompt submission failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use vibe_app_server::projects::{
        CloudError, GitProbe, GitSnapshot, Project, ProjectCloud, ProjectGitSnapshot, ProjectPage,
        ProjectRepository, ProjectsService, TeleportCloud, TeleportStartFailure,
        TeleportStartRequest,
    };
    use vibe_app_server::server::AppServer;

    use super::super::super::chat_input::ChatInputState;
    use super::super::super::clipboard_images::ClipboardImageManager;
    use super::super::super::controls::ControlState;
    use super::super::super::runtime::{
        apply_ui_operation_completion, interactive_test_runtime,
        interactive_test_runtime_with_server,
    };
    use super::super::super::state::TuiState;
    use super::super::PostMountAction;
    use super::*;

    #[test]
    fn fatal_initialization_remains_typed_after_visible_failure() {
        let mut startup = MountedStartup::new(None);
        let mut state = TuiState::new("fatal-startup");
        assert!(!record_initialization(
            &mut startup,
            &mut state,
            Err(CliError::Terminal("host initialization failed".to_owned())),
        ));
        assert!(startup.is_fatal());
        assert!(!startup.is_awaiting_fatal_key());
        assert!(state.entries.iter().any(|entry| {
            entry.text.contains("Background initialization failed")
                && entry.status == EntryStatus::Failed
        }));
        assert!(
            state
                .entries
                .iter()
                .any(|entry| entry.text == "Press any key to exit")
        );
        startup.arm_fatal_acknowledgment();
        assert!(startup.is_awaiting_fatal_key());
        assert!(matches!(
            startup.into_initialization_error(),
            Some(CliError::Terminal(_))
        ));
    }

    struct StartupProjects;

    impl ProjectCloud for StartupProjects {
        fn create(
            &self,
            name: &str,
            repo_url: &str,
            default_branch: &str,
        ) -> Result<Project, CloudError> {
            Ok(Project {
                project_id: "startup-project".to_owned(),
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
                    project_id: "startup-project".to_owned(),
                    name: "startup".to_owned(),
                    repositories: vec![ProjectRepository {
                        repo_url: "https://github.com/example/startup.git".to_owned(),
                        default_branch: Some("main".to_owned()),
                    }],
                    is_read_only: false,
                }],
                next_cursor: None,
            })
        }
    }

    struct StartupGit;

    impl GitProbe for StartupGit {
        fn inspect(&self, _working_directory: &Path) -> Result<GitSnapshot, CloudError> {
            Ok(GitSnapshot {
                repository: "https://github.com/example/startup.git".to_owned(),
                dirty: false,
                unpushed: false,
            })
        }

        fn inspect_project(
            &self,
            working_directory: &Path,
        ) -> Result<ProjectGitSnapshot, CloudError> {
            Ok(ProjectGitSnapshot {
                snapshot: self.inspect(working_directory)?,
                repo_root: working_directory.to_string_lossy().into_owned(),
                remote_name: "origin".to_owned(),
                branch: Some("main".to_owned()),
            })
        }

        fn push(&self, _working_directory: &Path) -> Result<(), CloudError> {
            Ok(())
        }
    }

    struct CapturingStartupTeleport {
        requests: Arc<Mutex<Vec<TeleportStartRequest>>>,
    }

    impl TeleportCloud for CapturingStartupTeleport {
        fn start(&self, request: &TeleportStartRequest) -> Result<String, TeleportStartFailure> {
            self.requests
                .lock()
                .map_err(|_| {
                    TeleportStartFailure::from(CloudError::Unavailable(
                        "fixture lock was poisoned".to_owned(),
                    ))
                })?
                .push(request.clone());
            Ok("https://cloud.example/teleport/startup".to_owned())
        }
    }

    #[tokio::test]
    async fn mounted_startup_teleport_executes_once_without_an_agent_turn() {
        for prompt in [None, Some("deployment context".to_owned())] {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let projects = ProjectsService::with_backends(
                Arc::new(StartupProjects),
                Arc::new(CapturingStartupTeleport {
                    requests: requests.clone(),
                }),
                Arc::new(StartupGit),
            );
            let server = AppServer::with_projects_service(projects);
            let mut runtime = Some(interactive_test_runtime_with_server(
                "startup-teleport",
                server,
            ));
            let (operation_sender, mut operation_receiver) = tokio::sync::mpsc::unbounded_channel();
            runtime.as_mut().expect("runtime").ui_operation_sender = operation_sender;
            let mut mounted = MountedStartup::new(Some(PostMountAction::Teleport(prompt.clone())));
            let mut active = None;
            let mut state = TuiState::new("startup-teleport");
            let mut controls = ControlState::new("startup-teleport");
            let mut input = ChatInputState::default();
            let mut clipboard_images = ClipboardImageManager::default();

            complete_mounted_startup(
                &mut mounted,
                Path::new("/workspace"),
                &mut runtime,
                &mut active,
                &mut state,
                &mut controls,
                &mut input,
                &mut clipboard_images,
            )
            .await
            .expect("mounted Teleport startup");
            for _ in 0..2 {
                let completion =
                    tokio::time::timeout(Duration::from_secs(1), operation_receiver.recv())
                        .await
                        .expect("startup operation completes")
                        .expect("startup operation channel");
                apply_ui_operation_completion(completion, &mut runtime, &mut state);
            }
            complete_mounted_startup(
                &mut mounted,
                Path::new("/workspace"),
                &mut runtime,
                &mut active,
                &mut state,
                &mut controls,
                &mut input,
                &mut clipboard_images,
            )
            .await
            .expect("mounted startup is consumed once");

            assert!(
                active.is_none(),
                "Teleport startup must not start an agent turn"
            );
            let requests = requests.lock().expect("captured Teleport requests");
            assert_eq!(
                requests.len(),
                1,
                "workflow: {:?}; diagnostics: {:?}; entries: {:?}",
                runtime.as_ref().map(|runtime| &runtime.cloud),
                state.diagnostics().collect::<Vec<_>>(),
                state
                    .entries
                    .iter()
                    .map(|entry| entry.text.as_str())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                requests[0].summary,
                prompt.unwrap_or_else(|| "Continue this session in Vibe Code".to_owned())
            );
        }
    }

    #[tokio::test]
    async fn unavailable_startup_teleport_stays_visible_and_has_no_remote_effect() {
        let mut runtime = interactive_test_runtime("startup-teleport-unavailable");
        runtime.vibe_code_enabled = false;
        let mut runtime = Some(runtime);
        let mut mounted = MountedStartup::new(Some(PostMountAction::Teleport(Some(
            "do not submit".to_owned(),
        ))));
        let mut active = None;
        let mut state = TuiState::new("startup-teleport-unavailable");
        let mut controls = ControlState::new("startup-teleport-unavailable");
        let mut input = ChatInputState::default();
        let mut clipboard_images = ClipboardImageManager::default();

        complete_mounted_startup(
            &mut mounted,
            Path::new("/workspace"),
            &mut runtime,
            &mut active,
            &mut state,
            &mut controls,
            &mut input,
            &mut clipboard_images,
        )
        .await
        .expect("unavailable startup Teleport is recoverable");

        assert!(active.is_none());
        assert!(
            state
                .diagnostics()
                .any(|diagnostic| { diagnostic.contains("Startup Teleport is unavailable") })
        );
    }
}

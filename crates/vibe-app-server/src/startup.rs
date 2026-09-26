use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use thiserror::Error;
use vibe_core::events::ModelMessage;
use vibe_core::storage::{SessionStore, StorageError};
use vibe_core::trust::{self, TrustError, TrustStatus, TrustStore};

use crate::projects::{ProjectsService, ProjectsServiceError};
use crate::session_lifecycle::{DeleteSessionError, delete_session_transactionally};
use crate::workspace::{WorkspacePaths, WorkspaceService, WorkspaceServiceError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTrustDecision {
    TrustRepository,
    TrustDirectory,
    Decline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceTrustPrompt {
    pub cwd: PathBuf,
    pub repo_root: Option<PathBuf>,
    pub detected_files: Vec<String>,
    pub repo_detected_files: Vec<String>,
    pub repo_explicitly_untrusted: bool,
    pub settings_path: PathBuf,
    pub decisions: Vec<WorkspaceTrustDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceTrustInspection {
    pub trusted: bool,
    pub prompt: Option<WorkspaceTrustPrompt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSessionSummary {
    pub id: String,
    pub end_time: String,
    pub preview: String,
}

#[derive(Clone)]
pub struct StartupHost {
    paths: WorkspacePaths,
}

impl StartupHost {
    #[must_use]
    pub const fn new(paths: WorkspacePaths) -> Self {
        Self { paths }
    }

    fn trust_store(&self) -> TrustStore {
        TrustStore::for_vibe_home(&self.paths.vibe_home)
    }

    /// The store sessions are saved in: where `session_logging` says, which a
    /// trusted project's configuration may move (reference `VibeConfig`).
    fn session_store(&self) -> SessionStore {
        let trusted = self
            .trust_store()
            .trust_status(&trust::resolve(&self.paths.working_directory))
            != TrustStatus::Untrusted;
        WorkspaceService::new(self.paths.clone(), trusted).map_or_else(
            |_| SessionStore::new(&self.paths.session_root),
            |service| service.session_store(),
        )
    }

    /// Whether the working directory is trusted, and otherwise the prompt a
    /// client shows about it. Reference `_resolve_workspace_trust` over
    /// `read_workspace_trust`: a session grant counts as trust, and a folder
    /// declined earlier is asked about again rather than refused.
    pub fn inspect_workspace_trust(&self) -> Result<WorkspaceTrustInspection, StartupHostError> {
        let store = self.trust_store();
        let cwd = trust::resolve(&self.paths.working_directory);
        if store.trust_status(&cwd) != TrustStatus::Untrusted {
            return Ok(WorkspaceTrustInspection {
                trusted: true,
                prompt: None,
            });
        }
        let settings_path = store.settings_path();
        Ok(WorkspaceTrustInspection {
            trusted: false,
            prompt: trust::build_trust_prompt(&cwd, true, &store)
                .map(|prompt| WorkspaceTrustPrompt::from_core(prompt, settings_path)),
        })
    }

    /// Records `decision` and answers whether the working directory is
    /// trusted afterward.
    pub fn decide_workspace_trust(
        &self,
        prompt: &WorkspaceTrustPrompt,
        decision: WorkspaceTrustDecision,
    ) -> Result<bool, StartupHostError> {
        if !prompt.decisions.contains(&decision) {
            return Err(StartupHostError::InvalidTrustDecision(
                "trust decision was not offered by the startup host".to_owned(),
            ));
        }
        let store = self.trust_store();
        let core = trust::build_trust_prompt(&prompt.cwd, true, &store).ok_or_else(|| {
            StartupHostError::InvalidTrustDecision(
                "this workspace has no trust decision to make".to_owned(),
            )
        })?;
        trust::apply_decision(&core, decision.core(), &store).map_err(|error| match error {
            TrustError::Io(source) => startup_io(&store.settings_path(), source),
            TrustError::Unsupported(_) => StartupHostError::InvalidTrustDecision(error.to_string()),
        })?;
        Ok(store.is_trusted(&prompt.cwd) == Some(true))
    }

    pub fn saved_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<SavedSessionSummary>, StartupHostError> {
        let store = self.session_store();
        store.migrate_legacy()?;
        let cwd = self.paths.working_directory.to_string_lossy();
        let sessions = store.sessions(Some(&cwd))?;
        Ok(sessions
            .into_iter()
            .take(limit)
            .map(|session| SavedSessionSummary {
                preview: store.first_user_message(&session.session_id),
                id: session.session_id,
                end_time: session.end_time.unwrap_or_else(|| "unknown".to_owned()),
            })
            .collect())
    }

    pub fn delete_session(&self, session_id: &str) -> Result<(), StartupHostError> {
        let store = self.session_store();
        let projects = ProjectsService::default()
            .with_loop_store(self.paths.vibe_home.join("scheduled-loops.json"))?;
        match delete_session_transactionally(&projects, session_id, || {
            match store.delete(session_id) {
                Ok(_) | Err(StorageError::SessionNotFound(_)) => Ok(()),
                Err(error) => Err(error),
            }
        }) {
            Ok(()) => Ok(()),
            Err(DeleteSessionError::Prepare(error)) => Err(error.into()),
            Err(DeleteSessionError::Delete(error)) => Err(error.into()),
            Err(DeleteSessionError::Rollback { delete, rollback }) => {
                Err(StartupHostError::DeleteRollback { delete, rollback })
            }
        }
    }

    /// Builds the session service for an interactive start, bringing the
    /// configuration files forward first.
    ///
    /// This is the startup step the reference runs before its orchestrator
    /// composes anything. A migration that cannot write is not fatal: its
    /// warning rides on every later configuration snapshot.
    pub fn into_workspace(
        self,
        project_trusted: bool,
    ) -> Result<WorkspaceService, StartupHostError> {
        let service = WorkspaceService::new(self.paths, project_trusted)
            .map(WorkspaceService::with_runtime_session_persistence)
            .map_err(StartupHostError::Workspace)?;
        service
            .migrate_configuration()
            .map_err(StartupHostError::Workspace)?;
        Ok(service)
    }
}

impl WorkspaceTrustDecision {
    /// The wire name of a decision. Reference `WorkspaceTrustDecision`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.core().as_str()
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        [Self::TrustRepository, Self::TrustDirectory, Self::Decline]
            .into_iter()
            .find(|decision| decision.as_str() == value)
    }

    const fn core(self) -> trust::WorkspaceTrustDecision {
        match self {
            Self::TrustRepository => trust::WorkspaceTrustDecision::TrustRepository,
            Self::TrustDirectory => trust::WorkspaceTrustDecision::TrustDirectory,
            Self::Decline => trust::WorkspaceTrustDecision::Decline,
        }
    }

    const fn from_core(decision: trust::WorkspaceTrustDecision) -> Option<Self> {
        match decision {
            trust::WorkspaceTrustDecision::TrustRepository => Some(Self::TrustRepository),
            trust::WorkspaceTrustDecision::TrustDirectory => Some(Self::TrustDirectory),
            trust::WorkspaceTrustDecision::Decline => Some(Self::Decline),
            trust::WorkspaceTrustDecision::TrustSession => None,
        }
    }
}

impl WorkspaceTrustPrompt {
    /// The dialog's view of a prompt: the files it names, where decisions
    /// persist, and the choices a client offers, which never include a session
    /// grant.
    fn from_core(prompt: trust::WorkspaceTrustPrompt, settings_path: PathBuf) -> Self {
        let decisions = trust::available_decisions(&prompt, false)
            .into_iter()
            .filter_map(WorkspaceTrustDecision::from_core)
            .collect();
        Self {
            cwd: trust::resolve(&prompt.cwd),
            repo_root: prompt.repo_root,
            detected_files: prompt.detected_files,
            repo_detected_files: prompt.repo_detected_files,
            repo_explicitly_untrusted: prompt.repo_explicitly_untrusted,
            settings_path,
            decisions,
        }
    }

    fn details(&self) -> Value {
        json!({
            "cwd": self.cwd,
            "repoRoot": self.repo_root,
            "detectedFiles": self.detected_files,
            "repoDetectedFiles": self.repo_detected_files,
            "repoExplicitlyUntrusted": self.repo_explicitly_untrusted,
            "settingsPath": self.settings_path,
            "availableDecisions": self
                .decisions
                .iter()
                .map(|decision| decision.as_str())
                .collect::<Vec<_>>(),
        })
    }
}

/// Why a workspace trust request was refused, which the app server answers as
/// `invalid_params` with no further detail, as upstream's `WorkspaceTrustError`.
#[derive(Debug, Error)]
pub enum WorkspaceTrustError {
    #[error("There is no workspace trust decision to make here")]
    NothingToDecide,
    #[error("The trust prompt for this workspace does not offer `{0}`")]
    Unsupported(String),
    #[error("A trust decision about a session must name the session's own working directory")]
    OutsideSession,
    #[error("The trust file could not be written: {0}")]
    Io(std::io::Error),
}

/// Reference `read_workspace_trust`: the trust `cwd` resolves to, and the
/// prompt a client would show about it while it is untrusted. The details
/// are `None` when there is nothing to ask, which is a workspace that holds
/// no file a trust decision would unlock.
#[must_use]
pub fn read_workspace_trust(store: &TrustStore, cwd: &Path) -> serde_json::Map<String, Value> {
    let resolved = trust::resolve(cwd);
    let status = store.trust_status(&resolved);
    let details = if status == TrustStatus::Untrusted {
        trust::build_trust_prompt(&resolved, true, store).map_or(Value::Null, |prompt| {
            WorkspaceTrustPrompt::from_core(prompt, store.settings_path()).details()
        })
    } else {
        Value::Null
    };
    let mut answer = serde_json::Map::new();
    answer.insert("status".to_owned(), json!(status.as_str()));
    answer.insert("details".to_owned(), details);
    answer
}

/// Reference `decide_workspace_trust`: a decision the current prompt offers is
/// recorded, and the trust it leaves is read back.
///
/// # Errors
///
/// When nothing is undecided about `cwd`, when its prompt does not offer
/// `decision`, or when the trust file cannot be created.
pub fn decide_workspace_trust(
    store: &TrustStore,
    cwd: &Path,
    decision: WorkspaceTrustDecision,
) -> Result<serde_json::Map<String, Value>, WorkspaceTrustError> {
    let resolved = trust::resolve(cwd);
    let prompt = trust::build_trust_prompt(&resolved, true, store)
        .ok_or(WorkspaceTrustError::NothingToDecide)?;
    if !trust::available_decisions(&prompt, false).contains(&decision.core()) {
        return Err(WorkspaceTrustError::Unsupported(
            decision.as_str().to_owned(),
        ));
    }
    trust::apply_decision(&prompt, decision.core(), store).map_err(|error| match error {
        TrustError::Io(source) => WorkspaceTrustError::Io(source),
        TrustError::Unsupported(name) => WorkspaceTrustError::Unsupported(name.to_owned()),
    })?;
    Ok(read_workspace_trust(store, &resolved))
}

/// Reference `read_untrusted_config_dirs`: the configuration directories a
/// trusted `cwd` holds that are declined on their own, and where to undo it.
#[must_use]
pub fn read_untrusted_config_dirs(
    store: &TrustStore,
    cwd: &Path,
) -> serde_json::Map<String, Value> {
    let dirs = trust::find_untrusted_config_dirs(cwd, store)
        .into_iter()
        .map(|directory| directory.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut answer = serde_json::Map::new();
    answer.insert("dirs".to_owned(), json!(dirs));
    answer.insert("settingsPath".to_owned(), json!(store.settings_path()));
    answer
}

#[derive(Debug, Error)]
pub enum StartupHostError {
    #[error("startup I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid workspace trust decision: {0}")]
    InvalidTrustDecision(String),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Workspace(WorkspaceServiceError),
    #[error(transparent)]
    Projects(#[from] ProjectsServiceError),
    #[error("session deletion failed ({delete}); scheduled-loop rollback failed ({rollback})")]
    DeleteRollback {
        delete: StorageError,
        rollback: ProjectsServiceError,
    },
}

/// Reference `message_preview` over a saved session: its first user message
/// that was not injected, cut at 160 characters, or `None` for a session that
/// holds none.
#[must_use]
pub fn saved_session_preview(session_root: &Path, session_id: &str) -> Option<String> {
    SessionStore::new(session_root)
        .load(session_id)
        .ok()?
        .messages
        .into_iter()
        .find_map(|message| match message {
            ModelMessage::User {
                content,
                injected: false,
                ..
            } if !content.is_empty() => Some(content.chars().take(160).collect()),
            _ => None,
        })
}

fn startup_io(path: &Path, source: std::io::Error) -> StartupHostError {
    StartupHostError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn paths(root: &Path) -> WorkspacePaths {
        WorkspacePaths {
            vibe_home: root.join("vibe-home"),
            working_directory: root.to_path_buf(),
            session_root: root.join("vibe-home/sessions"),
        }
    }

    #[test]
    fn trust_inspection_and_persistence_have_one_canonical_owner() {
        let root = tempfile::tempdir().expect("workspace");
        fs::create_dir_all(root.path().join(".vibe/prompts")).expect("project config");
        let host = StartupHost::new(paths(root.path()));
        let inspection = host.inspect_workspace_trust().expect("trust inspection");
        let prompt = inspection.prompt.expect("trust prompt");
        assert_eq!(prompt.detected_files, vec![".vibe/"]);
        assert!(
            host.decide_workspace_trust(&prompt, WorkspaceTrustDecision::TrustDirectory)
                .expect("trust saved")
        );
        let persisted = host.inspect_workspace_trust().expect("persisted trust");
        assert!(persisted.trusted);
        assert!(persisted.prompt.is_none());
    }

    #[test]
    fn explicit_untrust_can_be_replaced_by_one_trusted_scope() {
        let root = tempfile::tempdir().expect("workspace");
        fs::create_dir_all(root.path().join(".vibe/prompts")).expect("project config");
        let host = StartupHost::new(paths(root.path()));
        let prompt = host
            .inspect_workspace_trust()
            .expect("trust inspection")
            .prompt
            .expect("trust prompt");
        assert!(
            !host
                .decide_workspace_trust(&prompt, WorkspaceTrustDecision::Decline)
                .expect("decline saved")
        );
        let prompt = host
            .inspect_workspace_trust()
            .expect("untrusted workspace remains actionable")
            .prompt
            .expect("trust prompt remains available");
        assert!(prompt.repo_explicitly_untrusted || prompt.repo_root.is_none());
        assert!(
            host.decide_workspace_trust(&prompt, WorkspaceTrustDecision::TrustDirectory)
                .expect("trust replaces decline")
        );

        let written = fs::read_to_string(&prompt.settings_path).expect("persisted settings");
        let cwd = fs::canonicalize(root.path()).expect("canonical workspace");
        assert_eq!(
            written,
            format!(
                "trusted = [\n    \"{}\",\n]\nuntrusted = []\n",
                cwd.display()
            )
        );
    }

    #[test]
    fn a_malformed_trust_file_is_reset_and_an_unwritable_one_holds_for_the_process() {
        let malformed_root = tempfile::tempdir().expect("malformed workspace");
        fs::create_dir_all(malformed_root.path().join(".vibe/prompts")).expect("project config");
        let malformed_paths = paths(malformed_root.path());
        fs::create_dir_all(&malformed_paths.vibe_home).expect("vibe home");
        let settings = malformed_paths.vibe_home.join("trusted_folders.toml");
        fs::write(&settings, "trusted = [\n").expect("malformed settings");
        let inspection = StartupHost::new(malformed_paths)
            .inspect_workspace_trust()
            .expect("a malformed file reads as empty");
        assert!(inspection.prompt.is_some());
        assert_eq!(
            fs::read_to_string(&settings).expect("rewritten"),
            "trusted = []\nuntrusted = []\n"
        );

        let unwritable_root = tempfile::tempdir().expect("unwritable workspace");
        fs::create_dir_all(unwritable_root.path().join(".vibe/prompts")).expect("project config");
        let unwritable_paths = paths(unwritable_root.path());
        fs::create_dir_all(
            unwritable_paths
                .vibe_home
                .join("trusted_folders.toml/occupied"),
        )
        .expect("conflicting settings directory");
        let host = StartupHost::new(unwritable_paths);
        let prompt = host
            .inspect_workspace_trust()
            .expect("trust inspection")
            .prompt
            .expect("trust prompt");
        assert!(
            host.decide_workspace_trust(&prompt, WorkspaceTrustDecision::TrustDirectory)
                .expect("the decision holds in memory")
        );
        assert!(
            host.inspect_workspace_trust()
                .expect("reinspection")
                .trusted
        );
    }

    #[test]
    fn saved_session_summaries_include_the_first_user_message() {
        let root = tempfile::tempdir().expect("workspace");
        let paths = paths(root.path());
        let store = StartupHost::new(paths.clone()).session_store();
        let mut metadata = store
            .create("preview", &root.path().to_string_lossy(), None, 1)
            .expect("session");
        store
            .append_message(
                &mut metadata,
                &ModelMessage::Assistant {
                    message_id: None,
                    reasoning_message_id: None,
                    content: "assistant preface".to_owned(),
                    reasoning: None,
                    reasoning_payloads: Vec::new(),
                    tool_calls: Vec::new(),
                },
                2,
            )
            .expect("assistant message");
        store
            .append_message(
                &mut metadata,
                &ModelMessage::user("first request".to_owned()),
                3,
            )
            .expect("user message");
        let sessions = StartupHost::new(paths)
            .saved_sessions(100)
            .expect("saved sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].preview, "first request");
    }

    #[test]
    fn startup_deletion_removes_owned_scheduled_loops() {
        let root = tempfile::tempdir().expect("workspace");
        let paths = paths(root.path());
        let store = StartupHost::new(paths.clone()).session_store();
        let metadata = store
            .create("delete", &root.path().to_string_lossy(), None, 1)
            .expect("session");
        let loop_path = paths.vibe_home.join("scheduled-loops.json");
        let projects = ProjectsService::default()
            .with_loop_store(loop_path.clone())
            .expect("loop store");
        projects
            .dispatch(
                "loops/create",
                &serde_json::from_value(serde_json::json!({
                    "sessionId": metadata.id,
                    "prompt": "review",
                    "interval": "30s",
                    "nowSeconds": 10
                }))
                .expect("loop params"),
            )
            .expect("create loop");

        StartupHost::new(paths)
            .delete_session(&metadata.id)
            .expect("delete session");

        assert!(matches!(
            store.load(&metadata.id),
            Err(StorageError::SessionNotFound(_))
        ));
        let projects = ProjectsService::default()
            .with_loop_store(loop_path)
            .expect("reload loop store");
        let listed = projects
            .dispatch(
                "loops/list",
                &serde_json::from_value(serde_json::json!({
                    "sessionId": metadata.id
                }))
                .expect("list params"),
            )
            .expect("list loops");
        assert_eq!(listed.result["loops"].as_array().map(Vec::len), Some(0));
    }
}

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use vibe_core::events::ModelMessage;
use vibe_core::storage::{SessionStore, StorageError};

use crate::release3::{Release3Error, Release3Paths, Release3Service};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    paths: Release3Paths,
}

impl StartupHost {
    #[must_use]
    pub const fn new(paths: Release3Paths) -> Self {
        Self { paths }
    }

    pub fn inspect_workspace_trust(&self) -> Result<WorkspaceTrustInspection, StartupHostError> {
        let settings_path = self.paths.vibe_home.join("trusted_folders.toml");
        let settings = TrustSettings::load(&settings_path)?;
        let cwd = canonical_directory(&self.paths.working_directory)?;
        if settings.closest(&cwd) == Some(true) {
            return Ok(WorkspaceTrustInspection {
                trusted: true,
                prompt: None,
            });
        }
        Ok(WorkspaceTrustInspection {
            trusted: false,
            prompt: build_trust_prompt(&cwd, &settings, settings_path),
        })
    }

    pub fn decide_workspace_trust(
        &self,
        prompt: &WorkspaceTrustPrompt,
        decision: WorkspaceTrustDecision,
    ) -> Result<bool, StartupHostError> {
        let mut settings = TrustSettings::load(&prompt.settings_path)?;
        let (scope, trusted) = match decision {
            WorkspaceTrustDecision::TrustRepository => (
                prompt.repo_root.as_deref().ok_or_else(|| {
                    StartupHostError::InvalidTrustDecision(
                        "repository trust requires a repository root".to_owned(),
                    )
                })?,
                true,
            ),
            WorkspaceTrustDecision::TrustDirectory => (prompt.cwd.as_path(), true),
            WorkspaceTrustDecision::Decline => (prompt.cwd.as_path(), false),
        };
        if !prompt.decisions.contains(&decision) {
            return Err(StartupHostError::InvalidTrustDecision(
                "trust decision was not offered by the startup host".to_owned(),
            ));
        }
        settings.apply(scope, trusted);
        settings.save(&prompt.settings_path)?;
        Ok(trusted)
    }

    pub fn saved_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<SavedSessionSummary>, StartupHostError> {
        let store = SessionStore::new(&self.paths.session_root);
        store.migrate_legacy()?;
        let cwd = self.paths.working_directory.to_string_lossy();
        let sessions = store.list(Some(&cwd), 0, limit)?.sessions;
        Ok(sessions
            .into_iter()
            .map(|session| SavedSessionSummary {
                preview: session_preview(&store, &session.id),
                id: session.id,
                end_time: session.end_time.unwrap_or_else(|| "unknown".to_owned()),
            })
            .collect())
    }

    pub fn into_release3(self, project_trusted: bool) -> Result<Release3Service, StartupHostError> {
        Release3Service::new(self.paths, toml::Table::new(), project_trusted)
            .map(Release3Service::with_runtime_session_persistence)
            .map_err(StartupHostError::Release3)
    }
}

#[derive(Debug, Error)]
pub enum StartupHostError {
    #[error("startup I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid trusted folder settings at `{path}`: {message}")]
    TrustSettings { path: PathBuf, message: String },
    #[error("invalid workspace trust decision: {0}")]
    InvalidTrustDecision(String),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Release3(Release3Error),
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct TrustSettings {
    trusted: Vec<String>,
    untrusted: Vec<String>,
}

impl TrustSettings {
    fn load(path: &Path) -> Result<Self, StartupHostError> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|error| StartupHostError::TrustSettings {
                path: path.to_path_buf(),
                message: error.to_string(),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(startup_io(path, source)),
        }
    }

    fn save(&self, path: &Path) -> Result<(), StartupHostError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| startup_io(parent, source))?;
        }
        let encoded = toml::to_string(self).map_err(|error| StartupHostError::TrustSettings {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = path.with_file_name(format!(
            ".{}.tmp-{}-{sequence}",
            path.file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("trusted_folders.toml"),
            std::process::id()
        ));
        fs::write(&temporary, encoded).map_err(|source| startup_io(&temporary, source))?;
        fs::rename(&temporary, path).map_err(|source| {
            let _ = fs::remove_file(&temporary);
            startup_io(path, source)
        })
    }

    fn closest(&self, path: &Path) -> Option<bool> {
        path.ancestors().find_map(|candidate| {
            let candidate = candidate.to_string_lossy();
            if self.trusted.iter().any(|path| path == candidate.as_ref()) {
                Some(true)
            } else if self.untrusted.iter().any(|path| path == candidate.as_ref()) {
                Some(false)
            } else {
                None
            }
        })
    }

    fn apply(&mut self, path: &Path, trusted: bool) {
        let path = path.to_string_lossy().into_owned();
        self.trusted.retain(|candidate| candidate != &path);
        self.untrusted.retain(|candidate| candidate != &path);
        if trusted {
            self.trusted.push(path);
        } else {
            self.untrusted.push(path);
        }
    }
}

fn build_trust_prompt(
    cwd: &Path,
    settings: &TrustSettings,
    settings_path: PathBuf,
) -> Option<WorkspaceTrustPrompt> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|path| fs::canonicalize(path).ok());
    if home.as_deref() == Some(cwd) {
        return None;
    }
    let repo_root = find_repository_root(cwd, home.as_deref());
    let detected_files = trustable_files(cwd);
    let repo_detected_files = repo_root
        .as_deref()
        .map_or_else(Vec::new, |root| repository_trustable_files(cwd, root));
    if detected_files.is_empty() && repo_detected_files.is_empty() {
        return None;
    }
    let repo_explicitly_untrusted = repo_root
        .as_deref()
        .is_some_and(|root| settings.closest(root) == Some(false));
    let offer_repo = repo_root
        .as_deref()
        .is_some_and(|root| root != cwd && settings.closest(root) != Some(true));
    let mut decisions = Vec::new();
    if offer_repo {
        decisions.push(WorkspaceTrustDecision::TrustRepository);
    }
    decisions.extend([
        WorkspaceTrustDecision::TrustDirectory,
        WorkspaceTrustDecision::Decline,
    ]);
    Some(WorkspaceTrustPrompt {
        cwd: cwd.to_path_buf(),
        repo_root,
        detected_files,
        repo_detected_files,
        repo_explicitly_untrusted,
        settings_path,
        decisions,
    })
}

fn trustable_files(path: &Path) -> Vec<String> {
    let mut found = BTreeSet::new();
    if path.join("AGENTS.md").is_file() {
        found.insert("AGENTS.md".to_owned());
    }
    let vibe = path.join(".vibe");
    let vibe_has_content = ["tools", "skills", "agents", "prompts"]
        .iter()
        .any(|name| vibe.join(name).is_dir())
        || vibe.join("config.toml").is_file();
    if vibe.is_dir() && vibe_has_content {
        found.insert(".vibe/".to_owned());
    }
    if path.join(".agents/skills").is_dir() {
        found.insert(".agents/".to_owned());
    }
    found.into_iter().collect()
}

fn repository_trustable_files(cwd: &Path, repo_root: &Path) -> Vec<String> {
    if cwd == repo_root || !cwd.starts_with(repo_root) {
        return Vec::new();
    }
    let mut found = trustable_files(repo_root)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut current = cwd.parent();
    while let Some(directory) = current {
        if directory == repo_root {
            break;
        }
        let agents = directory.join("AGENTS.md");
        if agents.is_file()
            && let Ok(relative) = agents.strip_prefix(repo_root)
        {
            found.insert(relative.to_string_lossy().replace('\\', "/"));
        }
        current = directory.parent();
    }
    found.into_iter().collect()
}

fn find_repository_root(cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    cwd.ancestors()
        .take_while(|candidate| Some(*candidate) != home && candidate.parent().is_some())
        .find(|candidate| {
            let git = candidate.join(".git");
            git.is_file() || (git.is_dir() && git.join("HEAD").is_file())
        })
        .map(Path::to_path_buf)
}

fn session_preview(store: &SessionStore, session_id: &str) -> String {
    store
        .load(session_id)
        .ok()
        .and_then(|session| {
            session
                .messages
                .into_iter()
                .find_map(|message| match message {
                    ModelMessage::User { content } if !content.is_empty() => {
                        Some(content.chars().take(160).collect())
                    }
                    _ => None,
                })
        })
        .unwrap_or_else(|| "(empty session)".to_owned())
}

fn canonical_directory(path: &Path) -> Result<PathBuf, StartupHostError> {
    let canonical = fs::canonicalize(path).map_err(|source| startup_io(path, source))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(StartupHostError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "path is not a directory",
            ),
        })
    }
}

fn startup_io(path: &Path, source: std::io::Error) -> StartupHostError {
    StartupHostError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use vibe_core::storage::SessionStore;

    use super::*;

    fn paths(root: &Path) -> Release3Paths {
        Release3Paths {
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

        let settings = TrustSettings::load(&prompt.settings_path).expect("persisted settings");
        let cwd = fs::canonicalize(root.path()).expect("canonical workspace");
        let cwd = cwd.to_string_lossy();
        assert_eq!(
            settings
                .trusted
                .iter()
                .filter(|candidate| candidate.as_str() == cwd)
                .count(),
            1
        );
        assert!(!settings.untrusted.iter().any(|candidate| candidate == &cwd));
    }

    #[test]
    fn malformed_or_unwritable_trust_settings_fail_closed() {
        let malformed_root = tempfile::tempdir().expect("malformed workspace");
        fs::create_dir_all(malformed_root.path().join(".vibe/prompts")).expect("project config");
        let malformed_paths = paths(malformed_root.path());
        fs::create_dir_all(&malformed_paths.vibe_home).expect("vibe home");
        fs::write(
            malformed_paths.vibe_home.join("trusted_folders.toml"),
            "unexpected = true\n",
        )
        .expect("malformed settings");
        assert!(matches!(
            StartupHost::new(malformed_paths).inspect_workspace_trust(),
            Err(StartupHostError::TrustSettings { .. })
        ));

        let unwritable_root = tempfile::tempdir().expect("unwritable workspace");
        fs::create_dir_all(unwritable_root.path().join(".vibe/prompts")).expect("project config");
        let host = StartupHost::new(paths(unwritable_root.path()));
        let prompt = host
            .inspect_workspace_trust()
            .expect("trust inspection")
            .prompt
            .expect("trust prompt");
        fs::create_dir_all(&prompt.settings_path).expect("conflicting settings directory");
        assert!(matches!(
            host.decide_workspace_trust(&prompt, WorkspaceTrustDecision::TrustDirectory),
            Err(StartupHostError::Io { .. })
        ));
    }

    #[test]
    fn linked_worktree_metadata_is_recognized_as_a_repository_boundary() {
        let root = tempfile::tempdir().expect("linked worktree");
        fs::write(
            root.path().join(".git"),
            "gitdir: /tmp/repository/worktrees/test\n",
        )
        .expect("linked worktree metadata");
        assert_eq!(
            find_repository_root(root.path(), None).as_deref(),
            Some(root.path())
        );
    }

    #[test]
    fn saved_session_summaries_include_the_first_user_message() {
        let root = tempfile::tempdir().expect("workspace");
        let paths = paths(root.path());
        let store = SessionStore::new(&paths.session_root);
        let mut metadata = store
            .create("preview", &root.path().to_string_lossy(), None, 1)
            .expect("session");
        store
            .append_message(
                &mut metadata,
                &ModelMessage::Assistant {
                    content: "assistant preface".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                },
                2,
            )
            .expect("assistant message");
        store
            .append_message(
                &mut metadata,
                &ModelMessage::User {
                    content: "first request".to_owned(),
                },
                3,
            )
            .expect("user message");
        let sessions = StartupHost::new(paths)
            .saved_sessions(100)
            .expect("saved sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].preview, "first request");
    }
}

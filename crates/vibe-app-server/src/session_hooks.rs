//! The hooks a session loaded.
//!
//! Reference `AgentLoop` reads its hook files once, when it is built, and again
//! only when something reloads its runtime with `reload_hooks=True`: a
//! configuration write or reload that asks for one, a trust grant, a skill
//! mutation, a relocation. A `hooks.toml` written in between is not run until
//! then, which is why the loaded set lives on the session rather than being
//! read per turn. The count and the issues it publishes are the same cached
//! reading (`project_diagnostics`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use vibe_core::engine::TurnHooks;
use vibe_core::hooks::{HookConfigResult, HooksManager, load_hooks_from_fs};

/// One reading of a session's hook files.
#[derive(Debug, Clone, Default)]
pub struct SessionHooks {
    config: Arc<HookConfigResult>,
    /// Shared by every turn of the session, which is what carries the
    /// post-agent retry count across the cycles of one turn.
    manager: Option<Arc<HooksManager>>,
}

impl SessionHooks {
    /// Reads `files` in order, for hooks running in `cwd`.
    #[must_use]
    pub fn load(files: &[PathBuf], cwd: &Path) -> Self {
        Self::from_config(load_hooks_from_fs(files), cwd)
    }

    #[must_use]
    pub fn from_config(config: HookConfigResult, cwd: &Path) -> Self {
        let manager = (!config.hooks.is_empty())
            .then(|| Arc::new(HooksManager::new(config.hooks.clone(), cwd.to_path_buf())));
        Self {
            config: Arc::new(config),
            manager,
        }
    }

    /// How many hooks loaded. Reference `AgentLoop.hooks_count`.
    #[must_use]
    pub fn count(&self) -> usize {
        self.config.hooks.len()
    }

    /// Why the other entries did not load, as `ConfigIssue` values. Reference
    /// `_project_issue` over `hook_config_issues`.
    #[must_use]
    pub fn issues(&self) -> Vec<Value> {
        self.config
            .issues
            .iter()
            .map(|issue| {
                json!({
                    "file": issue.file.to_string_lossy(),
                    "message": issue.message,
                })
            })
            .collect()
    }

    /// The loaded hooks, for a child session that builds its own manager from
    /// them as the reference's subagent loop does.
    #[must_use]
    pub fn config(&self) -> &HookConfigResult {
        &self.config
    }

    /// What a turn needs to run these hooks, or `None` when there are none.
    #[must_use]
    pub fn turn_hooks(
        &self,
        transcript_path: String,
        cwd: String,
        parent_session_id: Option<String>,
    ) -> Option<TurnHooks> {
        self.manager.as_ref().map(|manager| TurnHooks {
            manager: Arc::clone(manager),
            transcript_path,
            cwd,
            parent_session_id,
        })
    }
}

impl PartialEq for SessionHooks {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.config, &other.config) || self.config == other.config
    }
}

impl Eq for SessionHooks {}

/// Reference `session_logger.messages_filepath.resolve()`: the session's
/// `messages.jsonl`, absolute and with its directory's links resolved, whether
/// or not the file exists yet.
#[must_use]
pub fn transcript_path(session_dir: &Path) -> String {
    let directory = session_dir
        .canonicalize()
        .unwrap_or_else(|_| session_dir.to_path_buf());
    directory
        .join("messages.jsonl")
        .to_string_lossy()
        .into_owned()
}

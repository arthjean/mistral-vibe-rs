//! What a session learns about a workspace before it acts on it: the
//! instruction files along a path, and the state of its git checkout.

use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use super::{FileRead, Workspace, WorkspaceError};

/// The per-directory instruction file both implementations read.
pub(super) const INSTRUCTION_FILE: &str = "AGENTS.md";

pub type GitInspectorFuture<'a> =
    Pin<Box<dyn Future<Output = Result<GitState, WorkspaceError>> + Send + 'a>>;

pub trait GitInspector: Send + Sync {
    fn inspect<'a>(&'a self, root: &'a Path) -> GitInspectorFuture<'a>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitState {
    pub branch: Option<String>,
    pub head: Option<String>,
    pub changed_paths: Vec<String>,
}

impl GitState {
    /// Parses `git status --porcelain=v1 --branch` output.
    ///
    /// The first line carries the branch header; every other line is a status
    /// code followed by a path at a fixed offset.
    #[must_use]
    pub fn from_porcelain(output: &str) -> Self {
        let mut lines = output.lines();
        let branch = lines
            .next()
            .unwrap_or_default()
            .strip_prefix("## ")
            .and_then(|value| value.split(['.', ' ']).next())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        Self {
            branch,
            head: None,
            changed_paths: lines
                .filter_map(|line| line.get(3..))
                .map(str::to_owned)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectContext {
    pub instruction_files: Vec<FileRead>,
    pub git: Option<GitState>,
}

impl Workspace {
    pub async fn project_context(
        &self,
        target: impl AsRef<Path>,
        git: Option<&dyn GitInspector>,
    ) -> Result<ProjectContext, WorkspaceError> {
        let relative = self.confined(target.as_ref(), true)?;
        let directory = if self
            .directory
            .metadata(&relative)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
        {
            relative
        } else {
            relative.parent().unwrap_or(Path::new(".")).to_path_buf()
        };
        let mut candidates = vec![PathBuf::from("AGENTS.md")];
        let mut current = PathBuf::new();
        for component in directory.components() {
            if matches!(component, Component::CurDir) {
                continue;
            }
            current.push(component.as_os_str());
            candidates.push(current.join("AGENTS.md"));
        }
        let instruction_files = {
            let mut instruction_files = Vec::new();
            let mut injected =
                self.injected_instructions
                    .lock()
                    .map_err(|_| WorkspaceError::LockPoisoned {
                        surface: "instruction injection",
                    })?;
            for candidate in candidates {
                if injected.contains(&candidate) || self.directory.metadata(&candidate).is_err() {
                    continue;
                }
                let read = self.read(&candidate, 1, Some(self.max_lines))?;
                injected.insert(candidate);
                instruction_files.push(read);
            }
            instruction_files
        };
        let git = match git {
            Some(inspector) => Some(inspector.inspect(&self.canonical_root).await?),
            None => None,
        };
        Ok(ProjectContext {
            instruction_files,
            git,
        })
    }

    /// The `AGENTS.md` files between a read file and the workspace root that
    /// this session has not injected yet, outermost first.
    ///
    /// Mirrors reference `find_subdirectory_agents_md`
    /// (`vibe/core/config/harness_files/_harness_manager.py:215`): the walk
    /// starts at the file's own directory and stops *before* the root, whose
    /// own instructions the session prompt already carries. Recording each
    /// directory as it is returned is what makes the injection happen once per
    /// directory per session.
    pub fn undiscovered_instructions(
        &self,
        file_path: &str,
    ) -> Result<Vec<(String, String)>, WorkspaceError> {
        let relative = match self.confined(Path::new(file_path), false) {
            Ok(relative) => relative,
            // A path this workspace cannot resolve carries no instructions,
            // which is the answer the reference gives for the same case.
            Err(_) => return Ok(Vec::new()),
        };
        let start = if self.is_directory(&relative) {
            relative.clone()
        } else {
            relative.parent().unwrap_or(Path::new("")).to_path_buf()
        };
        let mut injected =
            self.injected_instructions
                .lock()
                .map_err(|_| WorkspaceError::LockPoisoned {
                    surface: "instruction injection",
                })?;
        let mut discovered = Vec::new();
        let mut current = start;
        while !current.as_os_str().is_empty() && current != Path::new(".") {
            let candidate = current.join(INSTRUCTION_FILE);
            if !injected.contains(&candidate)
                && let Ok(read) =
                    self.read_lines_bounded(&candidate, 1, self.max_lines, self.max_read_bytes)
            {
                let content = read.lines.join("\n").trim().to_owned();
                if !content.is_empty() {
                    injected.insert(candidate);
                    discovered.push((self.absolute_display(&current), content));
                }
            }
            current = current.parent().unwrap_or(Path::new("")).to_path_buf();
        }
        discovered.reverse();
        Ok(discovered)
    }
}

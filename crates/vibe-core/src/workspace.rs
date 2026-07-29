use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::policy::{ApprovalAgent, PermissionRequirement, PermissionStore, PolicyGuardedTool};
use crate::tools::{
    OwnedToolHandlerFuture, RegistrationOutcome, ToolAvailability, ToolError, ToolExecutionOutput,
    ToolHandler, ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource,
    ToolSpec, object_schema,
};

pub const DEFAULT_MAX_READ_BYTES: usize = 1_048_576;
pub const DEFAULT_MAX_LINES: usize = 2_000;
pub const DEFAULT_MAX_DISCOVERED_FILES: usize = 10_000;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRead {
    pub path: String,
    pub content: String,
    pub numbered_content: String,
    pub start_line: usize,
    pub end_line: usize,
    pub bytes_read: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub path: String,
    pub is_directory: bool,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectContext {
    pub instruction_files: Vec<FileRead>,
    pub git: Option<GitState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MutationResult {
    pub path: String,
    pub bytes_written: usize,
    pub files_changed: usize,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditOperation {
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewHunk {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub turn_id: String,
    pub hunks: Vec<ReviewHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewView {
    pub active_turn: Option<String>,
    pub checkpoints: Vec<Checkpoint>,
    pub pending_hunks: Vec<ReviewHunk>,
}

pub struct Workspace {
    canonical_root: PathBuf,
    directory: Arc<Dir>,
    max_read_bytes: usize,
    max_lines: usize,
    max_discovered_files: usize,
    injected_instructions: Mutex<BTreeSet<PathBuf>>,
    next_temporary: AtomicU64,
}

impl Workspace {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let canonical_root =
            std::fs::canonicalize(path.as_ref()).map_err(|source| WorkspaceError::Io {
                path: path.as_ref().to_path_buf(),
                source,
            })?;
        let directory =
            Dir::open_ambient_dir(&canonical_root, ambient_authority()).map_err(|source| {
                WorkspaceError::Io {
                    path: canonical_root.clone(),
                    source,
                }
            })?;
        Ok(Self {
            canonical_root,
            directory: Arc::new(directory),
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            max_discovered_files: DEFAULT_MAX_DISCOVERED_FILES,
            injected_instructions: Mutex::new(BTreeSet::new()),
            next_temporary: AtomicU64::new(1),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.canonical_root
    }

    pub fn read(
        &self,
        path: impl AsRef<Path>,
        start_line: usize,
        max_lines: Option<usize>,
    ) -> Result<FileRead, WorkspaceError> {
        let relative = self.confined(path.as_ref(), true)?;
        let mut file = self
            .directory
            .open(&relative)
            .map_err(|source| WorkspaceError::Io {
                path: relative.clone(),
                source,
            })?;
        let before = file.metadata().map_err(|source| WorkspaceError::Io {
            path: relative.clone(),
            source,
        })?;
        let read_limit = u64::try_from(self.max_read_bytes.saturating_add(1))
            .map_err(|_| WorkspaceError::LimitOverflow)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|source| WorkspaceError::Io {
                path: relative.clone(),
                source,
            })?;
        let after = file.metadata().map_err(|source| WorkspaceError::Io {
            path: relative.clone(),
            source,
        })?;
        if before.len() != after.len() {
            return Err(WorkspaceError::ChangedDuringRead(relative));
        }
        let byte_truncated = bytes.len() > self.max_read_bytes;
        bytes.truncate(self.max_read_bytes);
        if bytes.contains(&0) {
            return Err(WorkspaceError::Binary(relative));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| WorkspaceError::InvalidEncoding(relative.clone()))?;
        let start = start_line.max(1);
        let line_limit = max_lines.unwrap_or(self.max_lines).min(self.max_lines);
        let all_lines = content.lines().collect::<Vec<_>>();
        let selected = all_lines
            .iter()
            .skip(start.saturating_sub(1))
            .take(line_limit)
            .copied()
            .collect::<Vec<_>>();
        let line_truncated =
            start.saturating_sub(1).saturating_add(selected.len()) < all_lines.len();
        let end_line = start.saturating_add(selected.len().saturating_sub(1));
        let selected_content = selected.join("\n");
        let numbered_content = selected
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{}|{line}", start.saturating_add(index)))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(FileRead {
            path: path_display(&relative),
            bytes_read: selected_content.len(),
            content: selected_content,
            numbered_content,
            start_line: start,
            end_line,
            truncated: byte_truncated || line_truncated,
        })
    }

    pub fn list(&self, path: impl AsRef<Path>) -> Result<Vec<FileEntry>, WorkspaceError> {
        let relative = self.confined(path.as_ref(), true)?;
        let mut entries = self
            .directory
            .read_dir(&relative)
            .map_err(|source| WorkspaceError::Io {
                path: relative.clone(),
                source,
            })?
            .map(|entry| {
                let entry = entry.map_err(|source| WorkspaceError::Io {
                    path: relative.clone(),
                    source,
                })?;
                let child = relative.join(entry.file_name());
                let metadata = entry.metadata().map_err(|source| WorkspaceError::Io {
                    path: child.clone(),
                    source,
                })?;
                Ok(FileEntry {
                    path: path_display(&child),
                    is_directory: metadata.is_dir(),
                    bytes: metadata.len(),
                })
            })
            .collect::<Result<Vec<_>, WorkspaceError>>()?;
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }

    pub fn discover(&self) -> Result<Vec<FileEntry>, WorkspaceError> {
        let ignores = self.load_ignores();
        let mut pending = vec![PathBuf::from(".")];
        let mut output = Vec::new();
        while let Some(directory) = pending.pop() {
            let mut entries = self.list(&directory)?;
            entries.sort_by(|left, right| right.path.cmp(&left.path));
            for entry in entries {
                if is_ignored(&entry.path, &ignores) {
                    continue;
                }
                if output.len() >= self.max_discovered_files {
                    return Err(WorkspaceError::DiscoveryLimit(self.max_discovered_files));
                }
                if entry.is_directory {
                    pending.push(PathBuf::from(&entry.path));
                }
                output.push(entry);
            }
        }
        output.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(output)
    }

    pub fn search(
        &self,
        pattern: &str,
        regex: bool,
        limit: usize,
    ) -> Result<Vec<SearchMatch>, WorkspaceError> {
        let compiled = regex
            .then(|| Regex::new(pattern))
            .transpose()
            .map_err(|error| WorkspaceError::InvalidPattern(error.to_string()))?;
        let mut matches = Vec::new();
        for entry in self
            .discover()?
            .into_iter()
            .filter(|entry| !entry.is_directory)
        {
            let read = match self.read(&entry.path, 1, Some(self.max_lines)) {
                Ok(read) => read,
                Err(WorkspaceError::Binary(_) | WorkspaceError::InvalidEncoding(_)) => continue,
                Err(error) => return Err(error),
            };
            for (index, line) in read.content.lines().enumerate() {
                let is_match = compiled.as_ref().map_or_else(
                    || line.contains(pattern),
                    |compiled| compiled.is_match(line),
                );
                if is_match {
                    matches.push(SearchMatch {
                        path: entry.path.clone(),
                        line: index.saturating_add(1),
                        text: line.to_owned(),
                    });
                    if matches.len() >= limit {
                        return Ok(matches);
                    }
                }
            }
        }
        Ok(matches)
    }

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

    fn write_new(&self, path: &Path, content: &[u8]) -> Result<MutationResult, WorkspaceError> {
        let relative = self.confined(path, false)?;
        if content.len() > self.max_read_bytes {
            return Err(WorkspaceError::WriteLimit {
                actual: content.len(),
                limit: self.max_read_bytes,
            });
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = self
            .directory
            .open_with(&relative, &options)
            .map_err(|source| WorkspaceError::Io {
                path: relative.clone(),
                source,
            })?;
        file.write_all(content)
            .and_then(|()| file.sync_all())
            .map_err(|source| WorkspaceError::Io {
                path: relative.clone(),
                source,
            })?;
        let text = String::from_utf8_lossy(content);
        Ok(MutationResult {
            path: path_display(&relative),
            bytes_written: content.len(),
            files_changed: 1,
            diff: unified_diff("", &text),
        })
    }

    fn atomic_replace(&self, relative: &Path, content: &[u8]) -> Result<(), WorkspaceError> {
        let sequence = self.next_temporary.fetch_add(1, Ordering::Relaxed);
        let parent = relative.parent().unwrap_or(Path::new("."));
        let temporary = parent.join(format!(".vibe-{sequence}.tmp"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = self
            .directory
            .open_with(&temporary, &options)
            .map_err(|source| WorkspaceError::Io {
                path: temporary.clone(),
                source,
            })?;
        if let Err(source) = file.write_all(content).and_then(|()| file.sync_all()) {
            let _ = self.directory.remove_file(&temporary);
            return Err(WorkspaceError::Io {
                path: temporary,
                source,
            });
        }
        self.directory
            .rename(&temporary, &self.directory, relative)
            .map_err(|source| WorkspaceError::Io {
                path: relative.to_path_buf(),
                source,
            })
    }

    fn remove(&self, relative: &Path) -> Result<(), WorkspaceError> {
        self.directory
            .remove_file(relative)
            .map_err(|source| WorkspaceError::Io {
                path: relative.to_path_buf(),
                source,
            })
    }

    fn read_raw(&self, relative: &Path) -> Result<Vec<u8>, WorkspaceError> {
        self.directory
            .read(relative)
            .map_err(|source| WorkspaceError::Io {
                path: relative.to_path_buf(),
                source,
            })
    }

    fn exists(&self, relative: &Path) -> bool {
        self.directory.metadata(relative).is_ok()
    }

    fn confined(&self, requested: &Path, must_exist: bool) -> Result<PathBuf, WorkspaceError> {
        let lexical = if requested.is_absolute() {
            requested
                .strip_prefix(&self.canonical_root)
                .map_err(|_| WorkspaceError::OutsideRoot(requested.to_path_buf()))?
                .to_path_buf()
        } else {
            requested.to_path_buf()
        };
        if lexical.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(WorkspaceError::OutsideRoot(requested.to_path_buf()));
        }
        let relative = if must_exist || self.directory.metadata(&lexical).is_ok() {
            self.directory
                .canonicalize(&lexical)
                .map_err(|source| WorkspaceError::Io {
                    path: lexical.clone(),
                    source,
                })?
        } else {
            let parent = lexical.parent().unwrap_or(Path::new("."));
            let canonical_parent =
                self.directory
                    .canonicalize(parent)
                    .map_err(|source| WorkspaceError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
            let file_name = lexical
                .file_name()
                .ok_or_else(|| WorkspaceError::OutsideRoot(requested.to_path_buf()))?;
            canonical_parent.join(file_name)
        };
        if relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(WorkspaceError::OutsideRoot(requested.to_path_buf()));
        }
        Ok(relative)
    }

    fn load_ignores(&self) -> Vec<String> {
        let mut ignores = vec![".git".to_owned(), "target".to_owned()];
        if let Ok(read) = self.read(".gitignore", 1, Some(self.max_lines)) {
            ignores.extend(
                read.content
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && !line.starts_with('#'))
                    .map(|line| {
                        line.trim_start_matches('/')
                            .trim_end_matches('/')
                            .to_owned()
                    }),
            );
        }
        ignores
    }
}

#[derive(Default)]
struct ReviewState {
    active_turn: Option<String>,
    baseline: BTreeMap<PathBuf, Option<Vec<u8>>>,
    checkpoints: Vec<Checkpoint>,
}

pub struct ReviewManager {
    workspace: Arc<Workspace>,
    state: Mutex<ReviewState>,
}

impl ReviewManager {
    #[must_use]
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self {
            workspace,
            state: Mutex::new(ReviewState::default()),
        }
    }

    pub fn begin_turn(&self, turn_id: impl Into<String>) -> Result<(), WorkspaceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| WorkspaceError::LockPoisoned { surface: "review" })?;
        if state.active_turn.is_some() {
            return Err(WorkspaceError::ReviewBusy);
        }
        state.active_turn = Some(turn_id.into());
        Ok(())
    }

    pub fn write(
        &self,
        path: impl AsRef<Path>,
        content: impl AsRef<[u8]>,
    ) -> Result<MutationResult, WorkspaceError> {
        let relative = self.workspace.confined(path.as_ref(), false)?;
        self.capture_baseline(&relative)?;
        self.workspace.write_new(&relative, content.as_ref())
    }

    pub fn edit(
        &self,
        path: impl AsRef<Path>,
        operations: &[EditOperation],
    ) -> Result<MutationResult, WorkspaceError> {
        let relative = self.workspace.confined(path.as_ref(), true)?;
        self.capture_baseline(&relative)?;
        let original = self.workspace.read_raw(&relative)?;
        let original_text = String::from_utf8(original.clone())
            .map_err(|_| WorkspaceError::InvalidEncoding(relative.clone()))?;
        let mut updated = original_text.clone();
        for operation in operations {
            let matches = updated.matches(&operation.old_text).count();
            if matches == 0 {
                return Err(WorkspaceError::StaleEdit {
                    path: relative.clone(),
                    needle: operation.old_text.clone(),
                });
            }
            if matches > 1 && !operation.replace_all {
                return Err(WorkspaceError::AmbiguousEdit {
                    path: relative.clone(),
                    matches,
                });
            }
            updated = if operation.replace_all {
                updated.replace(&operation.old_text, &operation.new_text)
            } else {
                updated.replacen(&operation.old_text, &operation.new_text, 1)
            };
        }
        self.workspace
            .atomic_replace(&relative, updated.as_bytes())?;
        Ok(MutationResult {
            path: path_display(&relative),
            bytes_written: updated.len(),
            files_changed: 1,
            diff: unified_diff(&original_text, &updated),
        })
    }

    pub fn seal_turn(&self) -> Result<Checkpoint, WorkspaceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| WorkspaceError::LockPoisoned { surface: "review" })?;
        let turn_id = state
            .active_turn
            .take()
            .ok_or(WorkspaceError::NoActiveTurn)?;
        let hunks = reconcile_hunks(&self.workspace, &state.baseline)?;
        let checkpoint = Checkpoint { turn_id, hunks };
        state.checkpoints.push(checkpoint.clone());
        Ok(checkpoint)
    }

    pub fn view(&self) -> Result<ReviewView, WorkspaceError> {
        let state = self
            .state
            .lock()
            .map_err(|_| WorkspaceError::LockPoisoned { surface: "review" })?;
        Ok(ReviewView {
            active_turn: state.active_turn.clone(),
            checkpoints: state.checkpoints.clone(),
            pending_hunks: reconcile_hunks(&self.workspace, &state.baseline)?,
        })
    }

    pub fn approve(&self) -> Result<ReviewView, WorkspaceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| WorkspaceError::LockPoisoned { surface: "review" })?;
        if state.active_turn.is_some() {
            return Err(WorkspaceError::ReviewBusy);
        }
        state.baseline.clear();
        state.checkpoints.clear();
        drop(state);
        self.view()
    }

    pub fn revert(&self) -> Result<ReviewView, WorkspaceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| WorkspaceError::LockPoisoned { surface: "review" })?;
        if state.active_turn.is_some() {
            return Err(WorkspaceError::ReviewBusy);
        }
        let current = state
            .baseline
            .keys()
            .map(|path| {
                let value = self
                    .workspace
                    .exists(path)
                    .then(|| self.workspace.read_raw(path))
                    .transpose()?;
                Ok((path.clone(), value))
            })
            .collect::<Result<BTreeMap<_, _>, WorkspaceError>>()?;
        let mut restored: Vec<PathBuf> = Vec::new();
        for (path, baseline) in &state.baseline {
            let result = match baseline {
                Some(bytes) => self.workspace.atomic_replace(path, bytes),
                None if self.workspace.exists(path) => self.workspace.remove(path),
                None => Ok(()),
            };
            if let Err(error) = result {
                for restored_path in restored.into_iter().rev() {
                    if let Some(previous) = current.get(&restored_path) {
                        match previous {
                            Some(bytes) => {
                                let _ = self.workspace.atomic_replace(&restored_path, bytes);
                            }
                            None if self.workspace.exists(&restored_path) => {
                                let _ = self.workspace.remove(&restored_path);
                            }
                            None => {}
                        }
                    }
                }
                return Err(error);
            }
            restored.push(path.clone());
        }
        state.baseline.clear();
        state.checkpoints.clear();
        drop(state);
        self.view()
    }

    fn capture_baseline(&self, relative: &Path) -> Result<(), WorkspaceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| WorkspaceError::LockPoisoned { surface: "review" })?;
        if state.active_turn.is_none() {
            return Err(WorkspaceError::NoActiveTurn);
        }
        if !state.baseline.contains_key(relative) {
            let baseline = self
                .workspace
                .exists(relative)
                .then(|| self.workspace.read_raw(relative))
                .transpose()?;
            state.baseline.insert(relative.to_path_buf(), baseline);
        }
        Ok(())
    }
}

pub struct WorkspaceTools {
    workspace: Arc<Workspace>,
    review: Arc<ReviewManager>,
}

impl WorkspaceTools {
    #[must_use]
    pub fn new(workspace: Arc<Workspace>, review: Arc<ReviewManager>) -> Self {
        Self { workspace, review }
    }

    pub fn register(
        &self,
        registry: &ToolRegistry,
        policy: PermissionStore,
        approval: Arc<dyn ApprovalAgent>,
    ) -> Result<Vec<RegistrationOutcome>, ToolError> {
        let read_workspace = self.workspace.clone();
        let read: Arc<dyn ToolHandler> = Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let workspace = read_workspace.clone();
                let path = invocation.arguments["path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let start_line = invocation.arguments["startLine"].as_u64().unwrap_or(1);
                Box::pin(async move {
                    let start_line = usize::try_from(start_line).unwrap_or(usize::MAX);
                    let result = workspace
                        .read(path, start_line, None)
                        .map_err(|error| ToolError::Execution(error.to_string()))?;
                    Ok(ToolExecutionOutput {
                        typed_result: serde_json::to_value(&result)
                            .map_err(|error| ToolError::InvalidResult(error.to_string()))?,
                        model_text: result.numbered_content,
                        display: json!({"kind": "read", "path": result.path}),
                        chunks: Vec::new(),
                    })
                })
            },
        );
        let search_workspace = self.workspace.clone();
        let search: Arc<dyn ToolHandler> = Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let workspace = search_workspace.clone();
                let pattern = invocation.arguments["pattern"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let regex = invocation.arguments["regex"].as_bool().unwrap_or(false);
                Box::pin(async move {
                    let result = workspace
                        .search(&pattern, regex, 500)
                        .map_err(|error| ToolError::Execution(error.to_string()))?;
                    Ok(ToolExecutionOutput {
                        typed_result: serde_json::to_value(&result)
                            .map_err(|error| ToolError::InvalidResult(error.to_string()))?,
                        model_text: result
                            .iter()
                            .map(|entry| format!("{}:{}:{}", entry.path, entry.line, entry.text))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        display: json!({"kind": "search", "matches": result.len()}),
                        chunks: Vec::new(),
                    })
                })
            },
        );
        let edit_review = self.review.clone();
        let edit: Arc<dyn ToolHandler> = Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let review = edit_review.clone();
                let path = invocation.arguments["path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let old_text = invocation.arguments["oldText"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let new_text = invocation.arguments["newText"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let replace_all = invocation.arguments["replaceAll"]
                    .as_bool()
                    .unwrap_or(false);
                Box::pin(async move {
                    let result = review
                        .edit(
                            path,
                            &[EditOperation {
                                old_text,
                                new_text,
                                replace_all,
                            }],
                        )
                        .map_err(|error| ToolError::Execution(error.to_string()))?;
                    Ok(ToolExecutionOutput {
                        typed_result: serde_json::to_value(&result)
                            .map_err(|error| ToolError::InvalidResult(error.to_string()))?,
                        model_text: result.diff.clone(),
                        display: json!({"kind": "diff", "path": result.path}),
                        chunks: Vec::new(),
                    })
                })
            },
        );
        let read_root = self.workspace.root().to_path_buf();
        let guarded_read = Arc::new(PolicyGuardedTool::new(
            "read",
            policy.clone(),
            approval.clone(),
            Arc::new(move |invocation| {
                let path = invocation.arguments["path"]
                    .as_str()
                    .ok_or_else(|| ToolError::Execution("read path is missing".to_owned()))?;
                Ok(vec![PermissionRequirement::Read {
                    path: read_root.join(path),
                }])
            }),
            read,
        ));
        let search_root = self.workspace.root().to_path_buf();
        let guarded_search = Arc::new(PolicyGuardedTool::new(
            "search",
            policy.clone(),
            approval.clone(),
            Arc::new(move |_invocation| {
                Ok(vec![PermissionRequirement::Read {
                    path: search_root.clone(),
                }])
            }),
            search,
        ));
        let edit_root = self.workspace.root().to_path_buf();
        let guarded_edit = Arc::new(PolicyGuardedTool::new(
            "edit",
            policy,
            approval,
            Arc::new(move |invocation| {
                let path = invocation.arguments["path"]
                    .as_str()
                    .ok_or_else(|| ToolError::Execution("edit path is missing".to_owned()))?;
                Ok(vec![PermissionRequirement::Write {
                    path: edit_root.join(path),
                }])
            }),
            edit,
        ));
        Ok(vec![
            registry.register(read_spec(), guarded_read)?,
            registry.register(search_spec(), guarded_search)?,
            registry.register(edit_spec(), guarded_edit)?,
        ])
    }
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace path `{0}` escapes the authorized root")]
    OutsideRoot(PathBuf),
    #[error("filesystem error at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("binary file `{0}` cannot be injected as text")]
    Binary(PathBuf),
    #[error("file `{0}` is not valid UTF-8")]
    InvalidEncoding(PathBuf),
    #[error("file `{0}` changed while it was being read")]
    ChangedDuringRead(PathBuf),
    #[error("discovery exceeded the {0}-entry limit")]
    DiscoveryLimit(usize),
    #[error("write is {actual} bytes, exceeding the {limit}-byte limit")]
    WriteLimit { actual: usize, limit: usize },
    #[error("invalid search pattern: {0}")]
    InvalidPattern(String),
    #[error("edit is stale in `{path}` because `{needle}` was not found")]
    StaleEdit { path: PathBuf, needle: String },
    #[error("edit in `{path}` matches {matches} locations; set replace_all explicitly")]
    AmbiguousEdit { path: PathBuf, matches: usize },
    #[error("a review mutation is unavailable while a turn is active")]
    ReviewBusy,
    #[error("mutation requires an active turn")]
    NoActiveTurn,
    #[error("{surface} lock is poisoned")]
    LockPoisoned { surface: &'static str },
    #[error("numeric limit cannot be represented on this platform")]
    LimitOverflow,
}

fn read_spec() -> ToolSpec {
    ToolSpec {
        name: "read".to_owned(),
        description: "Read a bounded UTF-8 workspace file".to_owned(),
        input_schema: object_schema(
            [
                ("path", json!({"type": "string"})),
                ("startLine", json!({"type": "integer"})),
            ],
            ["path"],
        ),
        output_schema: None,
        config: json!({"maxBytes": DEFAULT_MAX_READ_BYTES, "maxLines": DEFAULT_MAX_LINES}),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Read,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

fn search_spec() -> ToolSpec {
    ToolSpec {
        name: "search".to_owned(),
        description: "Search bounded workspace text".to_owned(),
        input_schema: object_schema(
            [
                ("pattern", json!({"type": "string"})),
                ("regex", json!({"type": "boolean"})),
            ],
            ["pattern"],
        ),
        output_schema: None,
        config: json!({"maxMatches": 500}),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Search,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

fn edit_spec() -> ToolSpec {
    ToolSpec {
        name: "edit".to_owned(),
        description: "Atomically edit one uniquely matched text span".to_owned(),
        input_schema: object_schema(
            [
                ("path", json!({"type": "string"})),
                ("oldText", json!({"type": "string"})),
                ("newText", json!({"type": "string"})),
                ("replaceAll", json!({"type": "boolean"})),
            ],
            ["path", "oldText", "newText"],
        ),
        output_schema: None,
        config: Value::Null,
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Diff,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

fn reconcile_hunks(
    workspace: &Workspace,
    baseline: &BTreeMap<PathBuf, Option<Vec<u8>>>,
) -> Result<Vec<ReviewHunk>, WorkspaceError> {
    baseline
        .iter()
        .filter_map(|(path, before)| {
            let after = workspace
                .exists(path)
                .then(|| workspace.read_raw(path))
                .transpose();
            match after {
                Err(error) => Some(Err(error)),
                Ok(after) if &after == before => None,
                Ok(after) => {
                    let before = before
                        .as_deref()
                        .map(String::from_utf8_lossy)
                        .unwrap_or_default();
                    let after = after
                        .as_deref()
                        .map(String::from_utf8_lossy)
                        .unwrap_or_default();
                    let removed = before.lines().count();
                    let added = after.lines().count();
                    Some(Ok(ReviewHunk {
                        path: path_display(path),
                        added,
                        removed,
                        diff: unified_diff(&before, &after),
                    }))
                }
            }
        })
        .collect()
}

fn unified_diff(before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }
    let mut lines = vec!["--- before".to_owned(), "+++ after".to_owned()];
    lines.extend(before.lines().map(|line| format!("-{line}")));
    lines.extend(after.lines().map(|line| format!("+{line}")));
    lines.join("\n")
}

fn is_ignored(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        path.split('/')
            .any(|component| wildcard_match(pattern, component))
            || wildcard_match(pattern, path)
    })
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    let parts = pattern.split('*').collect::<Vec<_>>();
    let mut remainder = value;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let Some(position) = remainder.find(part) else {
            return false;
        };
        if index == 0 && !pattern.starts_with('*') && position != 0 {
            return false;
        }
        remainder = &remainder[position.saturating_add(part.len())..];
    }
    pattern.ends_with('*') || remainder.is_empty()
}

fn path_display(path: &Path) -> String {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .map(Component::as_os_str)
        .collect::<PathBuf>()
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{
        ApprovalDecision, ApprovalFuture, ApprovalRequest, TrustDecision, TrustRootKind,
    };
    use tempfile::tempdir;

    struct RejectApproval;

    impl ApprovalAgent for RejectApproval {
        fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
            Box::pin(async { Ok(ApprovalDecision::Deny) })
        }
    }

    #[test]
    fn discovery_reads_searches_and_honors_ignore_order() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join(".gitignore"), "ignored.txt\n").expect("ignore");
        std::fs::write(directory.path().join("visible.txt"), "alpha\nbeta\n").expect("visible");
        std::fs::write(directory.path().join("ignored.txt"), "alpha\n").expect("ignored");
        let workspace = Workspace::open(directory.path()).expect("workspace");
        let discovered = workspace.discover().expect("discover");
        assert!(discovered.iter().any(|entry| entry.path == "visible.txt"));
        assert!(discovered.iter().all(|entry| entry.path != "ignored.txt"));
        let read = workspace.read("visible.txt", 2, Some(1)).expect("read");
        assert_eq!(read.numbered_content, "2|beta");
        let matches = workspace.search("alpha", false, 10).expect("search");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "visible.txt");
    }

    #[test]
    fn binary_invalid_encoding_and_traversal_fail_closed() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("binary"), [0, 1, 2]).expect("binary");
        std::fs::write(directory.path().join("invalid"), [0xff, 0xfe]).expect("invalid");
        let workspace = Workspace::open(directory.path()).expect("workspace");
        assert!(matches!(
            workspace.read("binary", 1, None),
            Err(WorkspaceError::Binary(_))
        ));
        assert!(matches!(
            workspace.read("invalid", 1, None),
            Err(WorkspaceError::InvalidEncoding(_))
        ));
        assert!(matches!(
            workspace.read("../secret", 1, None),
            Err(WorkspaceError::OutsideRoot(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn capability_directory_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("tempdir");
        let outside = tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), "secret").expect("secret");
        symlink(outside.path(), directory.path().join("escape")).expect("symlink");
        let workspace = Workspace::open(directory.path()).expect("workspace");
        assert!(workspace.read("escape/secret", 1, None).is_err());
    }

    #[test]
    fn edit_requires_unique_content_and_review_reverts_atomically() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("file.txt"), "one\ntwo\n").expect("file");
        let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
        let review = ReviewManager::new(workspace.clone());
        review.begin_turn("turn-1").expect("turn");
        let result = review
            .edit(
                "file.txt",
                &[EditOperation {
                    old_text: "two".to_owned(),
                    new_text: "three".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("edit");
        assert!(result.diff.contains("+three"));
        let checkpoint = review.seal_turn().expect("seal");
        assert_eq!(checkpoint.hunks.len(), 1);
        review.revert().expect("revert");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("file.txt")).expect("read"),
            "one\ntwo\n"
        );
    }

    #[test]
    fn manual_drift_is_reconciled_before_review() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("file.txt"), "before\n").expect("file");
        let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
        let review = ReviewManager::new(workspace);
        review.begin_turn("turn-1").expect("turn");
        review
            .edit(
                "file.txt",
                &[EditOperation {
                    old_text: "before".to_owned(),
                    new_text: "tool".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("edit");
        std::fs::write(directory.path().join("file.txt"), "manual\n").expect("manual drift");
        review.seal_turn().expect("seal");
        let view = review.view().expect("view");
        assert!(view.pending_hunks[0].diff.contains("+manual"));
    }

    #[tokio::test]
    async fn agents_hierarchy_is_injected_once() {
        let directory = tempdir().expect("tempdir");
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).expect("nested");
        std::fs::write(directory.path().join("AGENTS.md"), "root").expect("root agents");
        std::fs::write(nested.join("AGENTS.md"), "nested").expect("nested agents");
        let workspace = Workspace::open(directory.path()).expect("workspace");
        let first = workspace
            .project_context("nested", None)
            .await
            .expect("context");
        assert_eq!(first.instruction_files.len(), 2);
        let second = workspace
            .project_context("nested", None)
            .await
            .expect("context");
        assert!(second.instruction_files.is_empty());
    }

    #[tokio::test]
    async fn registered_workspace_tools_revalidate_trust_before_filesystem_access() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("visible.txt"), "safe\n").expect("file");
        let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
        let review = Arc::new(ReviewManager::new(workspace.clone()));
        let policy = PermissionStore::default();
        policy
            .set_trust(
                directory.path(),
                TrustDecision::Trusted,
                TrustRootKind::Workspace,
            )
            .await
            .expect("trust");
        let registry = ToolRegistry::default();
        WorkspaceTools::new(workspace, review)
            .register(&registry, policy.clone(), Arc::new(RejectApproval))
            .expect("register");
        let result = registry
            .invoke(
                "read",
                ToolInvocation {
                    call_id: "read-1".to_owned(),
                    arguments: json!({"path": "visible.txt"}),
                },
            )
            .await
            .expect("trusted read");
        assert_eq!(result.model_text, "1|safe");

        policy.revoke_trust(directory.path()).await.expect("revoke");
        let denied = registry
            .invoke(
                "read",
                ToolInvocation {
                    call_id: "read-2".to_owned(),
                    arguments: json!({"path": "visible.txt"}),
                },
            )
            .await
            .expect_err("revoked trust");
        assert!(denied.to_string().contains("permission denied"));
    }
}

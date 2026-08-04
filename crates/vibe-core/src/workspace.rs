use std::collections::BTreeSet;
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
use crate::schema::{ObjectSchema, Property};
use crate::text::matches_wildcard;
use crate::tools::{
    OwnedToolHandlerFuture, RegistrationOutcome, ToolAvailability, ToolError, ToolExecutionOutput,
    ToolHandler, ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource,
    ToolSpec,
};

mod review;

pub use review::{RestoreTransaction, ReviewManager};

pub const DEFAULT_MAX_READ_BYTES: usize = 1_048_576;
pub const DEFAULT_MAX_LINES: usize = 2_000;
pub const DEFAULT_MAX_DISCOVERED_FILES: usize = 10_000;
const DIFF_CONTEXT_LINES: usize = 3;

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
pub struct FileRead {
    pub path: String,
    pub content: String,
    pub numbered_content: String,
    pub start_line: usize,
    pub end_line: usize,
    /// Byte length of `content`, which is the selected line range rather than
    /// the whole file.
    pub content_bytes: usize,
    /// Lines the file holds, which is what tells an empty selection caused by
    /// an out-of-range offset apart from an empty file.
    pub total_lines: usize,
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

    /// Reads a whole workspace file as text, bounded by the byte budget.
    ///
    /// The returned flag reports whether the byte budget cut the file short.
    fn read_text(&self, relative: &Path) -> Result<(String, bool), WorkspaceError> {
        let mut file = self
            .directory
            .open(relative)
            .map_err(|source| WorkspaceError::Io {
                path: relative.to_path_buf(),
                source,
            })?;
        let before = file.metadata().map_err(|source| WorkspaceError::Io {
            path: relative.to_path_buf(),
            source,
        })?;
        let read_limit = u64::try_from(self.max_read_bytes.saturating_add(1))
            .map_err(|_| WorkspaceError::LimitOverflow)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|source| WorkspaceError::Io {
                path: relative.to_path_buf(),
                source,
            })?;
        let after = file.metadata().map_err(|source| WorkspaceError::Io {
            path: relative.to_path_buf(),
            source,
        })?;
        if before.len() != after.len() {
            return Err(WorkspaceError::ChangedDuringRead(relative.to_path_buf()));
        }
        let truncated = bytes.len() > self.max_read_bytes;
        bytes.truncate(self.max_read_bytes);
        if bytes.contains(&0) {
            return Err(WorkspaceError::Binary(relative.to_path_buf()));
        }
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            // The byte budget can land inside a character. Only the tail may be
            // split that way; an earlier failure is a genuine encoding error.
            Err(error)
                if truncated
                    && error.utf8_error().valid_up_to()
                        >= self.max_read_bytes.saturating_sub(3) =>
            {
                let valid_up_to = error.utf8_error().valid_up_to();
                let mut bytes = error.into_bytes();
                bytes.truncate(valid_up_to);
                String::from_utf8(bytes)
                    .map_err(|_| WorkspaceError::InvalidEncoding(relative.to_path_buf()))?
            }
            Err(_) => return Err(WorkspaceError::InvalidEncoding(relative.to_path_buf())),
        };
        Ok((content, truncated))
    }

    pub fn read(
        &self,
        path: impl AsRef<Path>,
        start_line: usize,
        max_lines: Option<usize>,
    ) -> Result<FileRead, WorkspaceError> {
        let relative = self.confined(path.as_ref(), true)?;
        let (content, byte_truncated) = self.read_text(&relative)?;
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
            content_bytes: selected_content.len(),
            content: selected_content,
            numbered_content,
            start_line: start,
            end_line,
            total_lines: all_lines.len(),
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
        self.discover_under(Path::new("."), &self.load_ignores())
    }

    /// Walks `root`, skipping every entry matching `ignores`.
    ///
    /// `grep` narrows the walk to its `path` argument and chooses its own
    /// ignore set, which is what `use_default_ignore` selects.
    fn discover_under(
        &self,
        root: &Path,
        ignores: &[String],
    ) -> Result<Vec<FileEntry>, WorkspaceError> {
        let mut pending = vec![root.to_path_buf()];
        let mut output = Vec::new();
        while let Some(directory) = pending.pop() {
            let mut entries = self.list(&directory)?;
            entries.sort_by(|left, right| right.path.cmp(&left.path));
            for entry in entries {
                if is_ignored(&entry.path, ignores) {
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

    /// Regex search over `path`, the contract the reference `grep` publishes.
    ///
    /// The pattern is always a regular expression, `path` narrows the walk to
    /// one file or one subtree, and `use_default_ignore` selects whether the
    /// `.gitignore` entries join the always-excluded directories.
    pub fn search(
        &self,
        pattern: &str,
        path: impl AsRef<Path>,
        limit: usize,
        use_default_ignore: bool,
    ) -> Result<Vec<SearchMatch>, WorkspaceError> {
        let compiled = Regex::new(pattern)
            .map_err(|error| WorkspaceError::InvalidPattern(error.to_string()))?;
        let relative = self.confined(path.as_ref(), true)?;
        let is_directory = self
            .directory
            .metadata(&relative)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        let targets = if is_directory {
            // `use_default_ignore` governs the `.gitignore` entries only: the
            // reference keeps its own exclusion list applied either way.
            let ignores = if use_default_ignore {
                self.load_ignores()
            } else {
                vec![".git".to_owned(), "target".to_owned()]
            };
            self.discover_under(&relative, &ignores)?
                .into_iter()
                .filter(|entry| !entry.is_directory)
                .map(|entry| entry.path)
                .collect::<Vec<_>>()
        } else {
            vec![path_display(&relative)]
        };
        let mut matches = Vec::new();
        for target in targets {
            let relative = self.confined(Path::new(&target), true)?;
            // Search the whole file, not just the first page a reader would get.
            let content = match self.read_text(&relative) {
                Ok((content, _)) => content,
                Err(WorkspaceError::Binary(_) | WorkspaceError::InvalidEncoding(_)) => continue,
                Err(error) => return Err(error),
            };
            for (index, line) in content.lines().enumerate() {
                if compiled.is_match(line) {
                    matches.push(SearchMatch {
                        path: target.clone(),
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

    fn read_raw_bounded(
        &self,
        relative: &Path,
        max_bytes: usize,
    ) -> Result<Vec<u8>, WorkspaceError> {
        let metadata = self
            .directory
            .metadata(relative)
            .map_err(|source| WorkspaceError::Io {
                path: relative.to_path_buf(),
                source,
            })?;
        let actual = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if actual > max_bytes {
            return Err(WorkspaceError::ReviewSnapshotLimit {
                actual,
                limit: max_bytes,
            });
        }
        let bytes = self.read_raw(relative)?;
        if bytes.len() > max_bytes {
            return Err(WorkspaceError::ReviewSnapshotLimit {
                actual: bytes.len(),
                limit: max_bytes,
            });
        }
        Ok(bytes)
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
                let path = invocation.arguments["file_path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                // `offset` is nullable and defaults to null, so an absent or
                // explicitly null offset both mean "start at line one".
                let start_line = invocation.arguments["offset"].as_u64().unwrap_or(1);
                let limit = invocation.arguments["limit"]
                    .as_u64()
                    .unwrap_or(DEFAULT_READ_LINE_LIMIT);
                Box::pin(async move {
                    let start_line = usize::try_from(start_line).unwrap_or(usize::MAX);
                    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
                    let result = workspace
                        .read(path, start_line, Some(limit))
                        .map_err(|error| ToolError::Execution(error.to_string()))?;
                    let model_text = read_model_text(&result);
                    Ok(ToolExecutionOutput {
                        typed_result: serde_json::to_value(&result)
                            .map_err(|error| ToolError::InvalidResult(error.to_string()))?,
                        model_text,
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
                let path = invocation.arguments["path"]
                    .as_str()
                    .unwrap_or(".")
                    .to_owned();
                // The reference reads `max_matches or default`, so a null and a
                // zero both fall back to the configured cap.
                let max_matches = invocation.arguments["max_matches"]
                    .as_u64()
                    .filter(|limit| *limit > 0)
                    .and_then(|limit| usize::try_from(limit).ok())
                    .unwrap_or(DEFAULT_GREP_MAX_MATCHES);
                let use_default_ignore = invocation.arguments["use_default_ignore"]
                    .as_bool()
                    .unwrap_or(true);
                Box::pin(async move {
                    let result = workspace
                        .search(&pattern, path, max_matches, use_default_ignore)
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
                let path = invocation.arguments["file_path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let old_text = invocation.arguments["old_string"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let new_text = invocation.arguments["new_string"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let replace_all = invocation.arguments["replace_all"]
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
            "read_file",
            policy.clone(),
            approval.clone(),
            Arc::new(move |invocation| {
                let path = invocation.arguments["file_path"].as_str().ok_or_else(|| {
                    ToolError::Execution("read_file file_path is missing".to_owned())
                })?;
                Ok(vec![PermissionRequirement::Read {
                    path: read_root.join(path),
                }])
            }),
            read,
        ));
        let search_root = self.workspace.root().to_path_buf();
        let guarded_search = Arc::new(PolicyGuardedTool::new(
            "grep",
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
                let path = invocation.arguments["file_path"]
                    .as_str()
                    .ok_or_else(|| ToolError::Execution("edit file_path is missing".to_owned()))?;
                Ok(vec![PermissionRequirement::Write {
                    path: edit_root.join(path),
                }])
            }),
            edit,
        ));
        Ok(vec![
            registry.register(read_file_spec(), guarded_read)?,
            registry.register(grep_spec(), guarded_search)?,
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
    #[error("git inspection failed: {0}")]
    GitInspection(String),
    #[error("edit is stale in `{path}` because `{needle}` was not found")]
    StaleEdit { path: PathBuf, needle: String },
    #[error("edit in `{path}` matches {matches} locations; set replace_all explicitly")]
    AmbiguousEdit { path: PathBuf, matches: usize },
    #[error("a review mutation is unavailable while a turn is active")]
    ReviewBusy,
    #[error("mutation requires an active turn")]
    NoActiveTurn,
    #[error("workspace restoration failed: {cause}; rollback: {rollback}")]
    RestoreRollback { cause: String, rollback: String },
    #[error("review snapshots are {actual} bytes, exceeding the {limit}-byte limit")]
    ReviewSnapshotLimit { actual: usize, limit: usize },
    #[error("{surface} lock is poisoned")]
    LockPoisoned { surface: &'static str },
    #[error("numeric limit cannot be represented on this platform")]
    LimitOverflow,
}

/// What the model reads back from a `read_file` call.
///
/// An empty selection is not an empty success: the reference distinguishes an
/// empty file from an offset past the last line, and says so in the text the
/// model receives rather than returning nothing. It branches on whether any
/// line was selected, not on whether those lines carry text, so a file holding
/// one blank line reads back as that blank line.
fn read_model_text(result: &FileRead) -> String {
    if result.total_lines == 0 {
        return format!(
            "<warning>`{}` exists but holds no content.</warning>",
            result.path
        );
    }
    if result.start_line > result.total_lines {
        return format!(
            "<warning>`{}` holds {} lines, which stops short of the requested offset {}.</warning>",
            result.path, result.total_lines, result.start_line
        );
    }
    result.numbered_content.clone()
}

/// Reference `ReadFileArgs.limit` default.
const DEFAULT_READ_LINE_LIMIT: u64 = 2000;
/// Reference `GrepToolConfig.default_max_matches`, applied when `max_matches`
/// stays null.
const DEFAULT_GREP_MAX_MATCHES: usize = 100;

fn read_file_spec() -> ToolSpec {
    ToolSpec {
        name: "read_file".to_owned(),
        description: "Read one file from an absolute path. Page through a long file with \
                      `offset` and `limit` instead of reading it whole, reach for `grep` when \
                      looking for specific content, and leave binary and model-weight files \
                      (.bin, .safetensors, .pt, .gguf) alone."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "file_path",
                Property::string().described("The absolute path of the file to read"),
            )
            .optional(
                "offset",
                Property::integer()
                    .constrained("minimum", 1)
                    .described("The 1-indexed line the read starts at")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "limit",
                Property::integer()
                    .constrained("exclusiveMinimum", 0)
                    .described("How many lines to read at most")
                    .with_default(DEFAULT_READ_LINE_LIMIT),
            )
            .build(),
        output_schema: None,
        config: json!({"maxBytes": DEFAULT_MAX_READ_BYTES, "maxLines": DEFAULT_MAX_LINES}),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Read,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

fn grep_spec() -> ToolSpec {
    ToolSpec {
        name: "grep".to_owned(),
        description: "Search file contents against a regular expression, reporting the path, \
                      the line number and the matching line. Narrow the walk with `path`."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "pattern",
                Property::string().described("The regular expression matched against file contents"),
            )
            .optional(
                "path",
                Property::string()
                    .described(
                        "The file or directory the search walks. Defaults to the working directory.",
                    )
                    .with_default("."),
            )
            .optional(
                "max_matches",
                Property::integer()
                    .described("Raises or lowers the default cap on returned matches.")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "use_default_ignore",
                Property::boolean()
                    .described("Whether .gitignore and .ignore entries are honoured.")
                    .with_default(true),
            )
            .build(),
        output_schema: None,
        config: json!({"maxMatches": DEFAULT_GREP_MAX_MATCHES}),
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
        description: "Replace an exact string in a file. Call `read_file` first, and never carry \
                      any part of its line-number prefix into `old_string` or `new_string`. When \
                      `old_string` is absent or matches more than once, add surrounding context \
                      until it is unique or set `replace_all`. Re-read the file before retrying a \
                      failed edit."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "file_path",
                Property::string().described("The absolute path of the file to modify"),
            )
            .required(
                "old_string",
                Property::string().described("The text being replaced"),
            )
            .required(
                "new_string",
                Property::string()
                    .described("The replacement text, which must differ from old_string"),
            )
            .optional(
                "replace_all",
                Property::boolean()
                    .described("Replace every occurrence of old_string instead of a single one")
                    .with_default(false),
            )
            .build(),
        output_schema: None,
        config: Value::Null,
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Diff,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

/// Renders a single unified-diff hunk covering the changed span.
///
/// Only the lines that actually differ, plus [`DIFF_CONTEXT_LINES`] of context,
/// reach the model. Emitting whole files would make an one-line edit cost as
/// much as the file itself.
fn unified_diff(before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }
    let before_lines = before.lines().collect::<Vec<_>>();
    let after_lines = after.lines().collect::<Vec<_>>();
    let shortest = before_lines.len().min(after_lines.len());
    let prefix = before_lines
        .iter()
        .zip(&after_lines)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = before_lines
        .iter()
        .rev()
        .zip(after_lines.iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
        .min(shortest.saturating_sub(prefix));
    let context_start = prefix.saturating_sub(DIFF_CONTEXT_LINES);
    let before_span = context_start
        ..before_lines
            .len()
            .saturating_sub(suffix)
            .saturating_add(DIFF_CONTEXT_LINES)
            .min(before_lines.len());
    let after_span = context_start
        ..after_lines
            .len()
            .saturating_sub(suffix)
            .saturating_add(DIFF_CONTEXT_LINES)
            .min(after_lines.len());

    let mut lines = vec![
        "--- before".to_owned(),
        "+++ after".to_owned(),
        format!(
            "@@ -{},{} +{},{} @@",
            hunk_start(before_span.start, before_span.len()),
            before_span.len(),
            hunk_start(after_span.start, after_span.len()),
            after_span.len(),
        ),
    ];
    lines.extend(
        before_lines
            .get(context_start..prefix)
            .unwrap_or_default()
            .iter()
            .map(|line| format!(" {line}")),
    );
    lines.extend(
        before_lines
            .get(prefix..before_lines.len().saturating_sub(suffix))
            .unwrap_or_default()
            .iter()
            .map(|line| format!("-{line}")),
    );
    lines.extend(
        after_lines
            .get(prefix..after_lines.len().saturating_sub(suffix))
            .unwrap_or_default()
            .iter()
            .map(|line| format!("+{line}")),
    );
    lines.extend(
        after_lines
            .get(after_lines.len().saturating_sub(suffix)..after_span.end)
            .unwrap_or_default()
            .iter()
            .map(|line| format!(" {line}")),
    );
    lines.join("\n")
}

const fn hunk_start(start: usize, length: usize) -> usize {
    if length == 0 {
        start
    } else {
        start.saturating_add(1)
    }
}

fn is_ignored(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        path.split('/')
            .any(|component| matches_wildcard(pattern, component))
            || matches_wildcard(pattern, path)
    })
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
        let matches = workspace.search("alpha", ".", 10, true).expect("search");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "visible.txt");

        // `use_default_ignore: false` drops the .gitignore entries and keeps
        // the always-excluded directories, matching the reference split.
        let unfiltered = workspace.search("alpha", ".", 10, false).expect("search");
        assert_eq!(unfiltered.len(), 2);
    }

    #[test]
    fn search_reaches_past_the_reader_page_limit() {
        let directory = tempdir().expect("tempdir");
        let mut content = "filler\n".repeat(DEFAULT_MAX_LINES + 10);
        content.push_str("needle\n");
        std::fs::write(directory.path().join("long.txt"), content).expect("long file");
        let workspace = Workspace::open(directory.path()).expect("workspace");

        let matches = workspace.search("needle", ".", 10, true).expect("search");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line, DEFAULT_MAX_LINES + 11);
    }

    #[test]
    fn edit_diff_reports_only_the_changed_span() {
        let directory = tempdir().expect("tempdir");
        let before = (0..500)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(directory.path().join("wide.txt"), &before).expect("seed file");
        let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
        let review = ReviewManager::new(workspace);
        review.begin_turn("turn-1").expect("turn");

        let result = review
            .edit(
                "wide.txt",
                &[EditOperation {
                    old_text: "line 250".to_owned(),
                    new_text: "line 250 edited".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("edit");

        assert!(result.diff.contains("@@ -248,7 +248,7 @@"));
        assert!(result.diff.contains("-line 250"));
        assert!(result.diff.contains("+line 250 edited"));
        assert_eq!(
            result
                .diff
                .lines()
                .filter(|line| line.starts_with('-'))
                .count(),
            2,
            "only the changed line and the `--- before` header start with a dash"
        );
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

    #[test]
    fn rewind_restoration_is_target_specific_transactional_and_forkable() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("main.txt"), "zero\n").expect("main fixture");
        std::fs::create_dir(directory.path().join("generated")).expect("generated directory");
        let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
        let review = ReviewManager::new(workspace);

        review.begin_turn_at("turn-1", 2).expect("first turn");
        review
            .write("generated/first.txt", b"first\n")
            .expect("first write");
        review.seal_turn().expect("first checkpoint");
        review.begin_turn_at("turn-2", 4).expect("second turn");
        review
            .edit(
                "main.txt",
                &[EditOperation {
                    old_text: "zero".to_owned(),
                    new_text: "two".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("second edit");
        review
            .write("generated/later.txt", b"later\n")
            .expect("later write");
        review.seal_turn().expect("second checkpoint");

        assert_eq!(
            review.restorable_paths_at(4).expect("latest paths"),
            vec!["generated/later.txt", "main.txt"]
        );
        assert_eq!(
            review.restorable_paths_at(2).expect("earlier paths"),
            vec!["generated/first.txt", "generated/later.txt", "main.txt"]
        );

        let staged = review
            .stage_restore_to_message(4)
            .expect("staged restoration");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("main.txt")).expect("staged main"),
            "zero\n"
        );
        assert!(!directory.path().join("generated/later.txt").exists());
        assert!(directory.path().join("generated/first.txt").exists());
        staged.rollback().expect("explicit rollback");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("main.txt")).expect("rolled back main"),
            "two\n"
        );
        assert!(directory.path().join("generated/later.txt").exists());

        let fork = review.fork_at(4).expect("checkpoint fork");
        assert_eq!(fork.view().expect("fork view").checkpoints.len(), 1);
        let restored = review
            .stage_restore_to_message(2)
            .expect("earlier restoration")
            .commit();
        assert_eq!(
            restored,
            vec!["generated/first.txt", "generated/later.txt", "main.txt"]
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("main.txt")).expect("restored main"),
            "zero\n"
        );
        assert!(!directory.path().join("generated/first.txt").exists());
        assert!(!directory.path().join("generated/later.txt").exists());
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
                "read_file",
                ToolInvocation {
                    call_id: "read-1".to_owned(),
                    arguments: json!({"file_path": "visible.txt"}),
                },
            )
            .await
            .expect("trusted read");
        assert_eq!(result.model_text, "1|safe");

        policy.revoke_trust(directory.path()).await.expect("revoke");
        let denied = registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: "read-2".to_owned(),
                    arguments: json!({"file_path": "visible.txt"}),
                },
            )
            .await
            .expect_err("revoked trust");
        assert!(denied.to_string().contains("permission denied"));
    }

    /// The three registered names and the argument keys each one reads, which
    /// is the contract a model prompted for reference behaviour relies on.
    #[tokio::test]
    async fn the_file_tools_publish_the_reference_names_and_argument_keys() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("visible.txt"), "alpha\nbeta\n").expect("file");
        let registry = registered_workspace_tools(directory.path()).await;

        let names = registry
            .list()
            .expect("list")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["edit", "grep", "read_file"]);

        let read = registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: "read-1".to_owned(),
                    arguments: json!({"file_path": "visible.txt", "offset": 2}),
                },
            )
            .await
            .expect("read_file accepts the reference keys");
        assert_eq!(read.model_text, "2|beta");

        let grep = registry
            .invoke(
                "grep",
                ToolInvocation {
                    call_id: "grep-1".to_owned(),
                    arguments: json!({"pattern": "al.ha"}),
                },
            )
            .await
            .expect("grep treats its pattern as a regular expression");
        assert_eq!(grep.model_text, "visible.txt:1:alpha");
    }

    /// An offset past the last line is an explicit out-of-range answer, never
    /// an empty success the model would read as an empty file.
    #[tokio::test]
    async fn read_file_reports_an_offset_past_the_end_of_the_file() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("short.txt"), "only\n").expect("file");
        let registry = registered_workspace_tools(directory.path()).await;

        let beyond = registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: "read-1".to_owned(),
                    arguments: json!({"file_path": "short.txt", "offset": 9}),
                },
            )
            .await
            .expect("an out-of-range offset still answers");

        assert!(
            beyond.model_text.contains("1 lines") && beyond.model_text.contains("offset 9"),
            "the answer must name the file length and the offset: {}",
            beyond.model_text
        );
    }

    /// The out-of-range answer keys on the selected line count, not on whether
    /// those lines carry text, so a file holding one blank line reads back as
    /// that blank line rather than as a file shorter than the offset.
    #[tokio::test]
    async fn read_file_returns_a_blank_line_rather_than_an_out_of_range_warning() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("blank.txt"), "\n").expect("file");
        let registry = registered_workspace_tools(directory.path()).await;

        let blank = registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: "read-1".to_owned(),
                    arguments: json!({"file_path": "blank.txt"}),
                },
            )
            .await
            .expect("a blank line is content");

        assert_eq!(blank.model_text, "1|");
    }

    /// The reference resolves `max_matches` with `or`, so a zero is not a cap
    /// of zero: it falls back to the configured default like an absent value.
    #[tokio::test]
    async fn grep_treats_a_zero_max_matches_as_the_default_cap() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("visible.txt"), "a\na\na\n").expect("file");
        let registry = registered_workspace_tools(directory.path()).await;

        let matched = registry
            .invoke(
                "grep",
                ToolInvocation {
                    call_id: "grep-1".to_owned(),
                    arguments: json!({"pattern": "a", "max_matches": 0}),
                },
            )
            .await
            .expect("grep answers");

        assert_eq!(matched.model_text.lines().count(), 3);
    }

    #[tokio::test]
    async fn grep_reports_an_invalid_pattern_instead_of_searching() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("visible.txt"), "alpha\n").expect("file");
        let registry = registered_workspace_tools(directory.path()).await;

        let error = registry
            .invoke(
                "grep",
                ToolInvocation {
                    call_id: "grep-1".to_owned(),
                    arguments: json!({"pattern": "alpha("}),
                },
            )
            .await
            .expect_err("an unparsable pattern cannot search");

        assert!(
            error.to_string().contains("invalid search pattern"),
            "the failure must name the pattern error: {error}"
        );
    }

    /// The camelCase keys the port used to publish are gone: a call written
    /// against them fails naming the reference key it left out.
    #[tokio::test]
    async fn edit_rejects_the_previous_camel_case_keys() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("file.txt"), "before\n").expect("file");
        let registry = registered_workspace_tools(directory.path()).await;

        let error = registry
            .invoke(
                "edit",
                ToolInvocation {
                    call_id: "edit-1".to_owned(),
                    arguments: json!({
                        "path": "file.txt",
                        "oldText": "before",
                        "newText": "after",
                    }),
                },
            )
            .await
            .expect_err("the camelCase keys are no longer the contract");

        assert!(
            error.to_string().contains("$.file_path")
                && error.to_string().contains("required property is missing"),
            "the failure must name the missing reference key: {error}"
        );
    }

    /// The stale-edit and ambiguity failures survive the key rename, and the
    /// write permission is still derived from the renamed path argument.
    #[tokio::test]
    async fn edit_keeps_its_failure_modes_and_its_permission_scope_under_the_new_keys() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("file.txt"), "one\none\n").expect("file");
        let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
        let review = Arc::new(ReviewManager::new(workspace.clone()));
        review.begin_turn("turn-1").expect("turn");
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
            .register(&registry, policy, Arc::new(RejectApproval))
            .expect("register");

        let ambiguous = registry
            .invoke(
                "edit",
                ToolInvocation {
                    call_id: "edit-1".to_owned(),
                    arguments: json!({
                        "file_path": "file.txt",
                        "old_string": "one",
                        "new_string": "two",
                    }),
                },
            )
            .await
            .expect_err("two matches without replace_all stay ambiguous");
        assert!(
            ambiguous.to_string().contains("matches 2 locations"),
            "{ambiguous}"
        );

        let stale = registry
            .invoke(
                "edit",
                ToolInvocation {
                    call_id: "edit-2".to_owned(),
                    arguments: json!({
                        "file_path": "file.txt",
                        "old_string": "absent",
                        "new_string": "two",
                    }),
                },
            )
            .await
            .expect_err("a needle that is not there is a stale edit");
        assert!(stale.to_string().contains("is stale"), "{stale}");

        let replaced = registry
            .invoke(
                "edit",
                ToolInvocation {
                    call_id: "edit-3".to_owned(),
                    arguments: json!({
                        "file_path": "file.txt",
                        "old_string": "one",
                        "new_string": "two",
                        "replace_all": true,
                    }),
                },
            )
            .await
            .expect("replace_all resolves the ambiguity");
        assert!(replaced.model_text.contains("+two"));
    }

    /// A trusted workspace with the three tools registered and every approval
    /// refused, so anything reaching the approval path fails loudly.
    async fn registered_workspace_tools(root: &Path) -> ToolRegistry {
        let workspace = Arc::new(Workspace::open(root).expect("workspace"));
        let review = Arc::new(ReviewManager::new(workspace.clone()));
        let policy = PermissionStore::default();
        policy
            .set_trust(root, TrustDecision::Trusted, TrustRootKind::Workspace)
            .await
            .expect("trust");
        let registry = ToolRegistry::default();
        WorkspaceTools::new(workspace, review)
            .register(&registry, policy, Arc::new(RejectApproval))
            .expect("register");
        registry
    }
}

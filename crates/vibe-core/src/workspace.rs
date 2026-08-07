use std::collections::BTreeSet;
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::policy::{
    PolicyGuardedTool, RequirementResolver, ToolGuard, resolve_file_tool_permission,
};
use crate::process::ClientToolIo;
use crate::schema::{ObjectSchema, Property};
use crate::text::{matches_wildcard, truncate_utf8};
use crate::tools::config::SharedToolConfig;
use crate::tools::config::{
    GrepConfig, ReadFileConfig, ToolConfigResolver, WriteFileConfig, declared_document,
};
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

/// What one search reads off the `grep` configuration.
///
/// Reference `GrepConfig` carries all five: how many matches a call returns
/// when it names none, which globs never enter the walk, which ignore file adds
/// to them, and how long the search may take. [`Self::from_config`] is where
/// they arrive, so a handler never spells a limit itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOptions {
    pub limit: usize,
    pub use_default_ignore: bool,
    pub exclude_patterns: Vec<String>,
    pub codeignore_file: String,
    /// [`None`] runs without a deadline, which is what a caller with no
    /// configured timeout asks for.
    pub timeout: Option<Duration>,
}

impl SearchOptions {
    /// The options a resolved `grep` configuration composes, with `limit`
    /// taken from the call when it named one.
    ///
    /// The reference reads `max_matches or default_max_matches`, so a null and
    /// a zero both fall back to the configured cap.
    #[must_use]
    pub fn from_config(
        config: &GrepConfig,
        requested_limit: Option<usize>,
        use_default_ignore: bool,
    ) -> Self {
        Self {
            limit: requested_limit
                .filter(|limit| *limit > 0)
                .unwrap_or(config.default_max_matches),
            use_default_ignore,
            exclude_patterns: config.exclude_patterns.clone(),
            codeignore_file: config.codeignore_file.clone(),
            timeout: (config.default_timeout > 0)
                .then(|| Duration::from_secs(config.default_timeout)),
        }
    }
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self::from_config(&ToolConfigResolver::new().view("grep"), None, true)
    }
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
    /// one file or one subtree, and [`SearchOptions`] carries what the operator
    /// configured: how many matches to return, which globs to exclude, which
    /// ignore file to read and how long the walk may take.
    pub fn search(
        &self,
        pattern: &str,
        path: impl AsRef<Path>,
        options: &SearchOptions,
    ) -> Result<Vec<SearchMatch>, WorkspaceError> {
        let compiled = Regex::new(pattern)
            .map_err(|error| WorkspaceError::InvalidPattern(error.to_string()))?;
        let relative = self.confined(path.as_ref(), true)?;
        let is_directory = self
            .directory
            .metadata(&relative)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        let deadline = options.timeout.map(|timeout| Instant::now() + timeout);
        let targets = if is_directory {
            // `use_default_ignore` governs the `.gitignore` entries only: the
            // reference keeps its own exclusion list applied either way, and
            // adds whatever the codeignore file names.
            let mut ignores = self.configured_ignores(options);
            if options.use_default_ignore {
                ignores.extend(self.load_ignores());
            } else {
                ignores.extend([".git".to_owned(), "target".to_owned()]);
            }
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
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                // A partial answer is discarded rather than returned as a whole
                // one: a search that ran out of time answers nothing.
                return Err(WorkspaceError::SearchTimeout {
                    seconds: options.timeout.unwrap_or_default().as_secs(),
                });
            }
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
                    if matches.len() >= options.limit {
                        return Ok(matches);
                    }
                }
            }
        }
        Ok(matches)
    }

    /// The exclusion set the configuration contributes: the configured globs,
    /// plus every non-comment line of the configured codeignore file.
    fn configured_ignores(&self, options: &SearchOptions) -> Vec<String> {
        let mut ignores = options
            .exclude_patterns
            .iter()
            .map(|pattern| pattern.trim_end_matches('/').to_owned())
            .filter(|pattern| !pattern.is_empty())
            .collect::<Vec<_>>();
        if options.codeignore_file.is_empty() {
            return ignores;
        }
        if let Ok(read) = self.read(&options.codeignore_file, 1, Some(self.max_lines)) {
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
        // The reference `WriteFileConfig.create_parent_dirs` defaults to true,
        // so a write into a directory that does not exist yet creates it rather
        // than failing.
        if let Some(parent) = relative
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            self.directory
                .create_dir_all(parent)
                .map_err(|source| WorkspaceError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = self
            .directory
            .open_with(&relative, &options)
            .map_err(|source| {
                // `create_new` is what refuses an overwrite, and the reference
                // answers that case by naming `edit` rather than reporting a
                // raw filesystem error.
                if source.kind() == io::ErrorKind::AlreadyExists {
                    WorkspaceError::AlreadyExists(relative.clone())
                } else {
                    WorkspaceError::Io {
                        path: relative.clone(),
                        source,
                    }
                }
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
            // A write may target a path whose parent directories do not exist
            // yet, so the canonicalization anchors at the deepest ancestor
            // that does and rejoins the components below it. The escape check
            // below still sees every rejoined component.
            let mut remainder = Vec::new();
            let mut cursor = lexical.as_path();
            loop {
                let parent = match cursor.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => parent,
                    _ => Path::new("."),
                };
                let file_name = cursor
                    .file_name()
                    .ok_or_else(|| WorkspaceError::OutsideRoot(requested.to_path_buf()))?;
                remainder.push(file_name.to_os_string());
                if parent == Path::new(".") || self.directory.metadata(parent).is_ok() {
                    let canonical_parent =
                        self.directory.canonicalize(parent).map_err(|source| {
                            WorkspaceError::Io {
                                path: parent.to_path_buf(),
                                source,
                            }
                        })?;
                    break remainder
                        .iter()
                        .rev()
                        .fold(canonical_parent, |resolved, component| {
                            resolved.join(component)
                        });
                }
                cursor = parent;
            }
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
    client_io: Option<ClientToolIo>,
}

impl WorkspaceTools {
    #[must_use]
    pub fn new(workspace: Arc<Workspace>, review: Arc<ReviewManager>) -> Self {
        Self {
            workspace,
            review,
            client_io: None,
        }
    }

    /// Routes file access through the connected client when it declared the
    /// matching capability.
    ///
    /// A client that hosts the editor holds buffers the workspace cannot see, so
    /// the file the agent reads is the one the user is looking at rather than
    /// the one last saved to disk. A client that declared nothing leaves every
    /// tool on the workspace's own filesystem access.
    #[must_use]
    pub fn with_client_io(mut self, client_io: Option<ClientToolIo>) -> Self {
        self.client_io = client_io;
        self
    }

    pub fn register(
        &self,
        registry: &ToolRegistry,
        guard: &ToolGuard,
    ) -> Result<Vec<RegistrationOutcome>, ToolError> {
        let ToolGuard {
            policy,
            approval,
            config,
            scratchpad: _,
        } = guard;
        let read_workspace = self.workspace.clone();
        let read_client = self.client_io.clone();
        let read_config = config.clone();
        let read: Arc<dyn ToolHandler> = Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let workspace = read_workspace.clone();
                let client = read_client.clone();
                // The budget is read per call, so an operator who raises it
                // between two turns is obeyed on the second one.
                let settings: ReadFileConfig = read_config.view("read_file");
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
                    let result =
                        match delegated_read(&workspace, client.as_ref(), &path, start_line, limit)
                            .await?
                        {
                            Some(result) => result,
                            None => workspace
                                .read(path, start_line, Some(limit))
                                .map_err(|error| ToolError::Execution(error.to_string()))?,
                        };
                    let model_text = read_model_text(&result);
                    // Reference `read_file` raises rather than truncating when
                    // the rendered output passes the budget, because a silently
                    // clipped file reads as a complete one.
                    if model_text.len() > settings.max_read_bytes {
                        return Err(ToolError::Execution(format!(
                            "the rendered output is {} bytes, over the {}-byte budget; narrow it \
                             with offset and limit",
                            model_text.len(),
                            settings.max_read_bytes
                        )));
                    }
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
        let search_config = config.clone();
        let search: Arc<dyn ToolHandler> = Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let workspace = search_workspace.clone();
                let settings: GrepConfig = search_config.view("grep");
                let pattern = invocation.arguments["pattern"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let path = invocation.arguments["path"]
                    .as_str()
                    .unwrap_or(".")
                    .to_owned();
                let requested = invocation.arguments["max_matches"]
                    .as_u64()
                    .and_then(|limit| usize::try_from(limit).ok());
                let use_default_ignore = invocation.arguments["use_default_ignore"]
                    .as_bool()
                    .unwrap_or(true);
                Box::pin(async move {
                    let options =
                        SearchOptions::from_config(&settings, requested, use_default_ignore);
                    let result = workspace
                        .search(&pattern, path, &options)
                        .map_err(|error| ToolError::Execution(error.to_string()))?;
                    let rendered = result
                        .iter()
                        .map(|entry| format!("{}:{}:{}", entry.path, entry.line, entry.text))
                        .collect::<Vec<_>>()
                        .join("\n");
                    // The byte budget clips the model-facing text rather than
                    // failing the call, which is what the reference does with
                    // its own output cap.
                    let model_text = truncate_utf8(&rendered, settings.max_output_bytes).to_owned();
                    Ok(ToolExecutionOutput {
                        typed_result: serde_json::to_value(&result)
                            .map_err(|error| ToolError::InvalidResult(error.to_string()))?,
                        model_text,
                        display: json!({"kind": "search", "matches": result.len()}),
                        chunks: Vec::new(),
                    })
                })
            },
        );
        let edit_review = self.review.clone();
        let edit_client = self.client_io.clone();
        let edit_workspace = self.workspace.clone();
        let edit: Arc<dyn ToolHandler> = Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let review = edit_review.clone();
                let client = edit_client.clone();
                let workspace = edit_workspace.clone();
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
                    let operations = [EditOperation {
                        old_text,
                        new_text,
                        replace_all,
                    }];
                    let result =
                        match delegated_edit(&workspace, client.as_ref(), &path, &operations)
                            .await?
                        {
                            Some(result) => result,
                            None => review
                                .edit(path, &operations)
                                .map_err(|error| ToolError::Execution(error.to_string()))?,
                        };
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
        let write_review = self.review.clone();
        let write_client = self.client_io.clone();
        let write_workspace = self.workspace.clone();
        let write_config = config.clone();
        let write: Arc<dyn ToolHandler> = Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let review = write_review.clone();
                let client = write_client.clone();
                let workspace = write_workspace.clone();
                let settings: WriteFileConfig = write_config.view("write_file");
                let path = invocation.arguments["file_path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let content = invocation.arguments["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                Box::pin(async move {
                    if path.trim().is_empty() {
                        return Err(ToolError::Execution(
                            "write_file file_path cannot be empty".to_owned(),
                        ));
                    }
                    // Both checks run before anything touches the filesystem,
                    // so a refused write leaves no directory behind.
                    if content.len() > settings.max_write_bytes {
                        return Err(ToolError::Execution(format!(
                            "the content is {} bytes, exceeding the {}-byte write budget",
                            content.len(),
                            settings.max_write_bytes
                        )));
                    }
                    if !settings.create_parent_dirs
                        && let Some(missing) = missing_parent(&workspace, &path)
                    {
                        return Err(ToolError::Execution(format!(
                            "the parent directory `{missing}` does not exist and \
                             create_parent_dirs is off"
                        )));
                    }
                    let result = match delegated_write(&workspace, client.as_ref(), &path, &content)
                        .await?
                    {
                        Some(result) => result,
                        None => review
                            .write(&path, content.as_bytes())
                            .map_err(|error| ToolError::Execution(error.to_string()))?,
                    };
                    Ok(ToolExecutionOutput {
                        model_text: format!(
                            "Wrote {} bytes to {}",
                            result.bytes_written, result.path
                        ),
                        display: json!({"kind": "write", "path": result.path}),
                        typed_result: serde_json::to_value(&result)
                            .map_err(|error| ToolError::InvalidResult(error.to_string()))?,
                        chunks: Vec::new(),
                    })
                })
            },
        );
        let root = self.workspace.root().to_path_buf();
        let guarded_read = Arc::new(PolicyGuardedTool::new(
            "read_file",
            policy.clone(),
            approval.clone(),
            file_tool_permission("read_file", "file_path", &root, guard),
            read,
        ));
        let guarded_search = Arc::new(PolicyGuardedTool::new(
            "grep",
            policy.clone(),
            approval.clone(),
            // Reference `GrepTool.resolve_permission` runs the same chain over
            // `args.path`, which defaults to the working directory when the call
            // names none.
            file_tool_permission("grep", "path", &root, guard),
            search,
        ));
        let guarded_edit = Arc::new(PolicyGuardedTool::new(
            "edit",
            policy.clone(),
            approval.clone(),
            file_tool_permission("edit", "file_path", &root, guard),
            edit,
        ));
        let guarded_write = Arc::new(PolicyGuardedTool::new(
            "write_file",
            policy.clone(),
            approval.clone(),
            file_tool_permission("write_file", "file_path", &root, guard),
            write,
        ));
        Ok(vec![
            registry.register(read_file_spec(), guarded_read)?,
            registry.register(grep_spec(), guarded_search)?,
            registry.register(edit_spec(), guarded_edit)?,
            registry.register(write_file_spec(), guarded_write)?,
        ])
    }
}

/// The permission resolver a file tool is published behind.
///
/// Reference `ReadFileTool`, `GrepTool`, `EditTool` and `WriteFileTool` all
/// answer `resolve_permission` by handing their own name, configuration and
/// path argument to one shared chain. This is that chain's entry point: the
/// argument is resolved against the workspace root, the configuration is read
/// at every call so an operator's edit lands without a re-registration, and the
/// call that names no path at all falls back to the root, which is what
/// `GrepArgs.path` defaults to.
fn file_tool_permission(
    tool: &'static str,
    argument: &'static str,
    root: &Path,
    guard: &ToolGuard,
) -> Arc<RequirementResolver> {
    let root = root.to_path_buf();
    let config = guard.config.clone();
    let scratchpad = guard.scratchpad.clone();
    Arc::new(move |invocation: &ToolInvocation| {
        let requested = invocation.arguments[argument].as_str().unwrap_or_default();
        if requested.is_empty() && argument != "path" {
            return Err(ToolError::Execution(format!(
                "{tool} {argument} is missing"
            )));
        }
        let path = if requested.is_empty() {
            root.clone()
        } else {
            root.join(requested)
        };
        let settings: SharedToolConfig = config.view(tool);
        Ok(resolve_file_tool_permission(
            &path,
            tool,
            &settings,
            scratchpad.as_deref(),
        ))
    })
}

/// The parent directory `path` needs and does not have, or [`None`] when it
/// has one.
///
/// A path the workspace refuses outright is not reported here: the confinement
/// check that follows names it, and answering "the parent is missing" for a
/// path that escapes the root would name the wrong problem.
fn missing_parent(workspace: &Workspace, path: &str) -> Option<String> {
    let relative = workspace.confined(Path::new(path), false).ok()?;
    let parent = relative
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())?;
    if workspace.root().join(parent).is_dir() {
        return None;
    }
    Some(path_display(parent))
}

/// The path a delegated request carries, or the error the local path would
/// have raised for it.
///
/// Hosting the filesystem is not a way around the workspace boundary: the same
/// confinement the local tools apply runs first, so a path that escapes the
/// root is refused here rather than handed to an editor that would happily open
/// it. What travels is the confined absolute path, which is the only form a
/// client can resolve: the request carries no working directory.
fn delegated_path(
    workspace: &Workspace,
    path: &str,
    must_exist: bool,
) -> Result<(PathBuf, String), ToolError> {
    let relative = workspace
        .confined(Path::new(path), must_exist)
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let display = path_display(&relative);
    Ok((workspace.root().join(relative), display))
}

/// Reads one file through the client, or `None` when it hosts no filesystem.
///
/// The client answers with a window rather than a file, so the line count it
/// implies is what it returned: the reference reports zero only for an empty
/// answer at the top of the file, and this keeps that distinction so an offset
/// past the end still reads as one rather than as an empty file.
async fn delegated_read(
    workspace: &Workspace,
    client: Option<&ClientToolIo>,
    path: &str,
    start_line: usize,
    limit: usize,
) -> Result<Option<FileRead>, ToolError> {
    let Some(client) = client.filter(|client| client.supports_read()) else {
        return Ok(None);
    };
    let (absolute, display) = delegated_path(workspace, path, true)?;
    let start = start_line.max(1);
    let line_limit = limit.min(workspace.max_lines);
    let content = client
        .read_text_file(
            &absolute.to_string_lossy(),
            u64::try_from(start).ok(),
            // One line past the budget, which is how the answer reports that it
            // stopped short without a second round trip.
            u64::try_from(line_limit.saturating_add(1)).ok(),
        )
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let mut cut = workspace.max_read_bytes.min(content.len());
    while cut > 0 && !content.is_char_boundary(cut) {
        cut = cut.saturating_sub(1);
    }
    let byte_truncated = cut < content.len();
    let all_lines = content[..cut].lines().collect::<Vec<_>>();
    let selected = all_lines
        .iter()
        .take(line_limit)
        .copied()
        .collect::<Vec<_>>();
    let selected_content = selected.join("\n");
    let numbered_content = selected
        .iter()
        .enumerate()
        .map(|(index, line)| format!("{}|{line}", start.saturating_add(index)))
        .collect::<Vec<_>>()
        .join("\n");
    let total_lines = if selected.is_empty() && start == 1 {
        0
    } else {
        start.saturating_sub(1).saturating_add(selected.len())
    };
    Ok(Some(FileRead {
        path: display,
        content_bytes: selected_content.len(),
        content: selected_content,
        numbered_content,
        start_line: start,
        end_line: start.saturating_add(selected.len().saturating_sub(1)),
        total_lines,
        truncated: byte_truncated || all_lines.len() > line_limit,
    }))
}

/// Writes one file through the client, or `None` when it hosts no filesystem.
///
/// Nothing on disk changes, so nothing is checkpointed: the buffer the client
/// now holds is its own to save, and a rewind that restored the untouched file
/// would claim an edit the workspace never made.
async fn delegated_write(
    workspace: &Workspace,
    client: Option<&ClientToolIo>,
    path: &str,
    content: &str,
) -> Result<Option<MutationResult>, ToolError> {
    let Some(client) = client.filter(|client| client.supports_write()) else {
        return Ok(None);
    };
    // A write may target a file that does not exist yet, on the client's side
    // as much as on ours.
    let (absolute, display) = delegated_path(workspace, path, false)?;
    client
        .write_text_file(&absolute.to_string_lossy(), content)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    Ok(Some(MutationResult {
        path: display,
        bytes_written: content.len(),
        files_changed: 1,
        diff: unified_diff("", content),
    }))
}

/// Edits one file through the client, or `None` when it hosts no filesystem.
///
/// The read and the write are both delegated, so the text the operations run
/// against is the buffer's rather than the file's. A client that hosts reads
/// but not writes leaves the whole edit local: applying an edit to a buffer and
/// saving it to disk would write over the very content it was not read from.
async fn delegated_edit(
    workspace: &Workspace,
    client: Option<&ClientToolIo>,
    path: &str,
    operations: &[EditOperation],
) -> Result<Option<MutationResult>, ToolError> {
    let Some(client) = client.filter(|client| client.supports_read() && client.supports_write())
    else {
        return Ok(None);
    };
    let (absolute, display) = delegated_path(workspace, path, true)?;
    let absolute = absolute.to_string_lossy().into_owned();
    let original = client
        .read_text_file(&absolute, None, None)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let updated = review::apply_edit_operations(Path::new(&display), &original, operations)
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    if updated != original {
        client
            .write_text_file(&absolute, &updated)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
    }
    Ok(Some(MutationResult {
        path: display,
        bytes_written: updated.len(),
        files_changed: 1,
        diff: unified_diff(&original, &updated),
    }))
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
    #[error("`{0}` already exists; use edit to modify it")]
    AlreadyExists(PathBuf),
    #[error("invalid search pattern: {0}")]
    InvalidPattern(String),
    #[error("the search did not finish inside the {seconds}-second budget")]
    SearchTimeout { seconds: u64 },
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
        config: declared_document("read_file"),
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
                    .described("Whether .gitignore and .ignore entries are honored.")
                    .with_default(true),
            )
            .build(),
        output_schema: None,
        config: declared_document("grep"),
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
        config: declared_document("edit"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Diff,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

/// Directive coverage for `write_file`, whose reference description this port
/// must cover without reproducing (`NOTICE`).
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The path is absolute | "absolute path" in the `file_path` description |
/// | An existing file is not overwritten; `edit` modifies it | "never overwrites an existing file: reach for `edit`" |
/// | Missing parent directories are created | "Missing parent directories are created" |
/// | The content replaces the whole file | "the whole file content" in the `content` description |
fn write_file_spec() -> ToolSpec {
    ToolSpec {
        name: "write_file".to_owned(),
        description: "Create a file at an absolute path and write it whole. It never overwrites \
                      an existing file: reach for `edit` to change one. Missing parent \
                      directories are created."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "file_path",
                Property::string()
                    .described("The absolute path of the file to write, which must not exist yet"),
            )
            .required(
                "content",
                Property::string().described("The whole file content to write"),
            )
            .build(),
        output_schema: None,
        config: declared_document("write_file"),
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
        ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionMode,
        PermissionStore, TrustDecision, TrustRootKind,
    };
    use tempfile::tempdir;

    /// The declared `grep` options with the call's own limit and ignore
    /// choice, which is what the handler composes.
    fn probe_options(limit: usize, use_default_ignore: bool) -> SearchOptions {
        SearchOptions {
            limit,
            use_default_ignore,
            ..SearchOptions::default()
        }
    }

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
        let matches = workspace
            .search("alpha", ".", &probe_options(10, true))
            .expect("search");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "visible.txt");

        // `use_default_ignore: false` drops the .gitignore entries and keeps
        // the always-excluded directories, matching the reference split.
        let unfiltered = workspace
            .search("alpha", ".", &probe_options(10, false))
            .expect("search");
        assert_eq!(unfiltered.len(), 2);
    }

    #[test]
    fn search_reaches_past_the_reader_page_limit() {
        let directory = tempdir().expect("tempdir");
        let mut content = "filler\n".repeat(DEFAULT_MAX_LINES + 10);
        content.push_str("needle\n");
        std::fs::write(directory.path().join("long.txt"), content).expect("long file");
        let workspace = Workspace::open(directory.path()).expect("workspace");

        let matches = workspace
            .search("needle", ".", &probe_options(10, true))
            .expect("search");

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
            .register(
                &registry,
                &ToolGuard::new(policy.clone(), Arc::new(RejectApproval)),
            )
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
    /// is the contract a model prompted for reference behavior relies on.
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
        assert_eq!(names, ["edit", "grep", "read_file", "write_file"]);

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
        // The reference declares `edit` and `write_file` as `ask`, so a harness
        // exercising their bodies grants them the way an operator's "allow
        // always" does rather than answering a prompt per call.
        for tool in ["edit", "write_file"] {
            policy.set_tool_permission(tool, PermissionMode::Always);
        }
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
            .register(
                &registry,
                &ToolGuard::new(policy.clone(), Arc::new(RejectApproval)),
            )
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

    /// A trusted workspace with the file tools registered and every approval
    /// refused, so anything reaching the approval path fails loudly.
    async fn registered_workspace_tools(root: &Path) -> ToolRegistry {
        registered_with_settings(root, "").await.0
    }

    /// US-108: a registered file tool passes the guard for the session
    /// scratchpad without an approval, even for a name its own
    /// `sensitive_patterns` would otherwise stop, and the same name inside the
    /// workspace still asks.
    ///
    /// What stops the scratchpad read here is the workspace confinement, one
    /// layer past the permission: this port serves the file tools through a
    /// `cap-std` root while the reference serves any absolute path. That
    /// boundary is `read_file`'s own contract and moves with US-113, so the
    /// assertion below names the confinement rather than an approval, which is
    /// what proves the permission chain granted the path.
    #[tokio::test]
    async fn a_registered_file_tool_reaches_the_scratchpad_without_asking() {
        let root = tempdir().expect("workspace");
        let scratchpad =
            crate::scratchpad::init_scratchpad("workspace-scratchpad-probe").expect("scratchpad");
        std::fs::write(scratchpad.join(".env"), "SECRET=1\n").expect("scratchpad file");
        std::fs::write(root.path().join(".env"), "SECRET=2\n").expect("workspace file");

        let workspace = Arc::new(Workspace::open(root.path()).expect("workspace"));
        let review = Arc::new(ReviewManager::new(workspace.clone()));
        let policy = PermissionStore::default();
        policy
            .set_trust(
                root.path(),
                TrustDecision::Trusted,
                TrustRootKind::Workspace,
            )
            .await
            .expect("trust");
        let registry = ToolRegistry::default();
        WorkspaceTools::new(workspace, review)
            .register(
                &registry,
                &ToolGuard::new(policy, Arc::new(RejectApproval))
                    .with_scratchpad(Some(scratchpad.clone())),
            )
            .expect("register");

        let granted = registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: "scratchpad-read".to_owned(),
                    arguments: json!({"file_path": scratchpad.join(".env").to_string_lossy()}),
                },
            )
            .await
            .expect_err("the workspace confinement stops it one layer past the guard");
        let granted = granted.to_string();
        assert!(
            granted.contains("escapes the authorized root"),
            "the scratchpad passed the permission chain: {granted}"
        );

        let asked = registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: "workspace-read".to_owned(),
                    arguments: json!({"file_path": ".env"}),
                },
            )
            .await
            .expect_err("a sensitive name inside the workspace asks");
        let asked = asked.to_string();
        assert!(
            asked.contains("approval denied"),
            "the same name inside the workspace reaches the operator: {asked}"
        );

        crate::scratchpad::cleanup_scratchpad(Some(&scratchpad));
    }

    /// The file tools of a trusted workspace, published against a resolver the
    /// caller can move afterward.
    ///
    /// The resolver is returned rather than consumed, which is what proves the
    /// registration hands the families a resolver and not a snapshot: a test
    /// changes `settings` after the surface is published and the next call
    /// obeys it.
    async fn registered_with_settings(
        root: &Path,
        settings: &str,
    ) -> (ToolRegistry, ToolConfigResolver) {
        let workspace = Arc::new(Workspace::open(root).expect("workspace"));
        let review = Arc::new(ReviewManager::new(workspace.clone()));
        // A mutating tool snapshots what it is about to change, which this port
        // only allows inside a turn, so the helper opens one.
        review.begin_turn("turn-1").expect("a turn opens");
        let policy = PermissionStore::default();
        // The reference declares `edit` and `write_file` as `ask`, so a harness
        // exercising their bodies grants them the way an operator's "allow
        // always" does rather than answering a prompt per call.
        for tool in ["edit", "write_file"] {
            policy.set_tool_permission(tool, PermissionMode::Always);
        }
        policy
            .set_trust(root, TrustDecision::Trusted, TrustRootKind::Workspace)
            .await
            .expect("trust");
        let config = policy.tool_config();
        config.update(settings.parse::<toml::Table>().expect("settings parse"));
        let registry = ToolRegistry::default();
        WorkspaceTools::new(workspace, review)
            .register(
                &registry,
                &ToolGuard {
                    policy,
                    approval: Arc::new(RejectApproval),
                    config: config.clone(),
                    scratchpad: None,
                },
            )
            .expect("register");
        (registry, config)
    }

    /// US-103: `read_file` reads its budget from the configuration, and a
    /// render past it fails naming both sizes rather than truncating.
    #[tokio::test]
    async fn a_configured_read_budget_refuses_the_render_naming_both_sizes() {
        let directory = tempdir().expect("tempdir");
        let content = (1..=200)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(directory.path().join("long.txt"), &content).expect("file");
        let (registry, config) =
            registered_with_settings(directory.path(), "[read_file]\nmax_read_bytes = 120\n").await;

        let refused = registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: "read-1".to_owned(),
                    arguments: json!({"file_path": "long.txt"}),
                },
            )
            .await
            .expect_err("a render past the budget is refused");
        let message = refused.to_string();
        assert!(message.contains("120-byte budget"), "{message}");
        assert!(message.contains("offset and limit"), "{message}");

        // The surface was published against the resolver, not a snapshot of it,
        // so raising the budget between two calls is obeyed on the second.
        config.update(
            "[read_file]\nmax_read_bytes = 51200\n"
                .parse::<toml::Table>()
                .expect("settings parse"),
        );
        let read = registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: "read-2".to_owned(),
                    arguments: json!({"file_path": "long.txt"}),
                },
            )
            .await
            .expect("the raised budget carries the same file");
        assert_eq!(read.typed_result["totalLines"], json!(200));
    }

    /// US-103: `grep` reads its cap, its exclusion globs and its codeignore file
    /// from the configuration.
    #[tokio::test]
    async fn grep_reads_its_cap_and_its_exclusions_from_the_configuration() {
        let directory = tempdir().expect("tempdir");
        std::fs::create_dir_all(directory.path().join("build")).expect("build");
        std::fs::create_dir_all(directory.path().join("generated")).expect("generated");
        std::fs::write(
            directory.path().join("kept.txt"),
            "needle\nneedle\nneedle\n",
        )
        .expect("kept");
        std::fs::write(directory.path().join("build/out.txt"), "needle\n").expect("built");
        std::fs::write(directory.path().join("generated/out.txt"), "needle\n").expect("generated");
        std::fs::write(
            directory.path().join(".vibeignore"),
            "# comment\n\ngenerated/\n",
        )
        .expect("codeignore");
        let (registry, config) = registered_with_settings(directory.path(), "").await;

        let matched = registry
            .invoke(
                "grep",
                ToolInvocation {
                    call_id: "grep-1".to_owned(),
                    arguments: json!({"pattern": "needle"}),
                },
            )
            .await
            .expect("search");
        let paths = matched.typed_result["matches"]
            .as_array()
            .map(|entries| entries.len())
            .unwrap_or_else(|| {
                matched
                    .typed_result
                    .as_array()
                    .map(Vec::len)
                    .expect("an array of matches")
            });
        assert_eq!(
            paths, 3,
            "the declared exclusions drop `build/` and the codeignore file drops `generated/`: {}",
            matched.model_text
        );

        // The cap is the operator's: two matches out of the three.
        config.update(
            "[grep]\ndefault_max_matches = 2\n"
                .parse::<toml::Table>()
                .expect("settings parse"),
        );
        let capped = registry
            .invoke(
                "grep",
                ToolInvocation {
                    call_id: "grep-2".to_owned(),
                    arguments: json!({"pattern": "needle"}),
                },
            )
            .await
            .expect("search");
        assert_eq!(
            capped.model_text.lines().count(),
            2,
            "{}",
            capped.model_text
        );

        // And so is the byte budget the model-facing text is clipped at.
        config.update(
            "[grep]\nmax_output_bytes = 12\n"
                .parse::<toml::Table>()
                .expect("settings parse"),
        );
        let clipped = registry
            .invoke(
                "grep",
                ToolInvocation {
                    call_id: "grep-3".to_owned(),
                    arguments: json!({"pattern": "needle"}),
                },
            )
            .await
            .expect("search");
        assert!(clipped.model_text.len() <= 12, "{}", clipped.model_text);
    }

    /// US-103: `write_file` reads its byte budget and its parent-creation flag
    /// from the configuration, and refuses before touching the filesystem.
    #[tokio::test]
    async fn write_file_reads_its_budget_and_parent_creation_from_the_configuration() {
        let directory = tempdir().expect("tempdir");
        let (registry, config) = registered_with_settings(
            directory.path(),
            "[write_file]\nmax_write_bytes = 16\ncreate_parent_dirs = false\n",
        )
        .await;
        let oversized = registry
            .invoke(
                "write_file",
                ToolInvocation {
                    call_id: "write-1".to_owned(),
                    arguments: json!({"file_path": "big.txt", "content": "x".repeat(64)}),
                },
            )
            .await
            .expect_err("a write past the configured budget is refused");
        assert!(
            oversized.to_string().contains("16-byte write budget"),
            "{oversized}"
        );
        assert!(!directory.path().join("big.txt").exists());

        let missing = registry
            .invoke(
                "write_file",
                ToolInvocation {
                    call_id: "write-2".to_owned(),
                    arguments: json!({"file_path": "nested/child.txt", "content": "hi"}),
                },
            )
            .await
            .expect_err("a missing parent is refused when the flag is off");
        assert!(missing.to_string().contains("nested"), "{missing}");
        assert!(!directory.path().join("nested").exists());

        // The reference default creates the directory instead.
        config.update(
            "[write_file]\ncreate_parent_dirs = true\n"
                .parse::<toml::Table>()
                .expect("settings parse"),
        );
        registry
            .invoke(
                "write_file",
                ToolInvocation {
                    call_id: "write-3".to_owned(),
                    arguments: json!({"file_path": "nested/child.txt", "content": "hi"}),
                },
            )
            .await
            .expect("the parent is created");
        assert!(directory.path().join("nested/child.txt").exists());
    }

    /// A client hosting an editor: it answers reads from a buffer that differs
    /// from what is on disk, so a delegated read is distinguishable from a local
    /// one by its content alone.
    #[derive(Default)]
    struct EditorClient {
        requests: Mutex<Vec<crate::process::ClientToolRequest>>,
        buffer: Mutex<String>,
        capabilities: Mutex<BTreeSet<crate::process::ClientToolCapability>>,
    }

    impl EditorClient {
        fn hosting(
            buffer: &str,
            capabilities: impl IntoIterator<Item = crate::process::ClientToolCapability>,
        ) -> Arc<Self> {
            let client = Self::default();
            *client.buffer.lock().expect("buffer") = buffer.to_owned();
            *client.capabilities.lock().expect("capabilities") = capabilities.into_iter().collect();
            Arc::new(client)
        }

        fn methods(&self) -> Vec<&'static str> {
            self.requests
                .lock()
                .expect("requests")
                .iter()
                .map(crate::process::ClientToolRequest::method)
                .collect()
        }
    }

    impl crate::process::ClientToolPort for EditorClient {
        fn request<'a>(
            &'a self,
            request: crate::process::ClientToolRequest,
        ) -> crate::process::ToolIoFuture<'a> {
            Box::pin(async move {
                self.requests
                    .lock()
                    .map_err(|_| {
                        crate::process::ToolIoError::Request("client lock poisoned".to_owned())
                    })?
                    .push(request.clone());
                match request {
                    crate::process::ClientToolRequest::ReadTextFile { .. } => {
                        Ok(json!({"content": self.buffer.lock().expect("buffer").clone()}))
                    }
                    crate::process::ClientToolRequest::WriteTextFile { content, .. } => {
                        *self.buffer.lock().expect("buffer") = content;
                        Ok(json!({}))
                    }
                    _ => Ok(json!({})),
                }
            })
        }

        fn supports(&self, capability: crate::process::ClientToolCapability) -> bool {
            self.capabilities
                .lock()
                .expect("capabilities")
                .contains(&capability)
        }
    }

    async fn registered_workspace_tools_with_client(
        root: &Path,
        client: Option<ClientToolIo>,
    ) -> ToolRegistry {
        let workspace = Arc::new(Workspace::open(root).expect("workspace"));
        let review = Arc::new(ReviewManager::new(workspace.clone()));
        review.begin_turn("turn-1").expect("turn");
        let policy = PermissionStore::default();
        // The reference declares `edit` and `write_file` as `ask`, so a harness
        // exercising their bodies grants them the way an operator's "allow
        // always" does rather than answering a prompt per call.
        for tool in ["edit", "write_file"] {
            policy.set_tool_permission(tool, PermissionMode::Always);
        }
        policy
            .set_trust(root, TrustDecision::Trusted, TrustRootKind::Workspace)
            .await
            .expect("trust");
        let registry = ToolRegistry::default();
        WorkspaceTools::new(workspace, review)
            .with_client_io(client)
            .register(
                &registry,
                &ToolGuard::new(policy.clone(), Arc::new(RejectApproval)),
            )
            .expect("register");
        registry
    }

    /// The agent reads the buffer the user is looking at rather than the file
    /// the workspace last saw.
    #[tokio::test]
    async fn a_client_hosting_reads_answers_read_file_from_its_buffer() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("main.rs"), "on disk\n").expect("file");
        let client = EditorClient::hosting(
            "unsaved one\nunsaved two\n",
            [crate::process::ClientToolCapability::FilesystemRead],
        );
        let registry = registered_workspace_tools_with_client(
            directory.path(),
            Some(ClientToolIo::new("session-1", client.clone())),
        )
        .await;

        let read = registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: "read-1".to_owned(),
                    arguments: json!({"file_path": "main.rs"}),
                },
            )
            .await
            .expect("the client answers the read");
        assert_eq!(read.model_text, "1|unsaved one\n2|unsaved two");
        assert_eq!(client.methods(), ["clientTool/readTextFile"]);
        // The request carries the confined absolute path, which is the only
        // form a client can resolve, and the result keeps the workspace-
        // relative display the local read reports.
        let expected = std::fs::canonicalize(directory.path())
            .expect("canonical root")
            .join("main.rs")
            .to_string_lossy()
            .into_owned();
        let requests = client.requests.lock().expect("requests");
        assert!(
            matches!(
                &requests[0],
                crate::process::ClientToolRequest::ReadTextFile { session_id, path, .. }
                    if session_id == "session-1" && *path == expected
            ),
            "{requests:?} does not carry {expected}"
        );
    }

    /// A write the client hosts never touches the workspace, so the file the
    /// user has open is the only copy that changed.
    #[tokio::test]
    async fn a_client_hosting_writes_answers_write_file_and_edit() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("main.rs"), "on disk\n").expect("file");
        let client = EditorClient::hosting(
            "alpha\n",
            [
                crate::process::ClientToolCapability::FilesystemRead,
                crate::process::ClientToolCapability::FilesystemWrite,
            ],
        );
        let registry = registered_workspace_tools_with_client(
            directory.path(),
            Some(ClientToolIo::new("session-1", client.clone())),
        )
        .await;

        registry
            .invoke(
                "write_file",
                ToolInvocation {
                    call_id: "write-1".to_owned(),
                    arguments: json!({"file_path": "created.rs", "content": "beta\n"}),
                },
            )
            .await
            .expect("the client answers the write");
        assert!(
            !directory.path().join("created.rs").exists(),
            "a delegated write reached the workspace filesystem"
        );

        let edited = registry
            .invoke(
                "edit",
                ToolInvocation {
                    call_id: "edit-1".to_owned(),
                    arguments: json!({
                        "file_path": "main.rs",
                        "old_string": "beta",
                        "new_string": "gamma",
                    }),
                },
            )
            .await
            .expect("the client answers the edit");
        assert!(edited.model_text.contains("+gamma"), "{edited:?}");
        assert_eq!(
            *client.buffer.lock().expect("buffer"),
            "gamma\n",
            "the edit was applied to the client's buffer"
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("main.rs")).expect("on disk"),
            "on disk\n",
            "a delegated edit rewrote the workspace file"
        );
        assert_eq!(
            client.methods(),
            [
                "clientTool/writeTextFile",
                "clientTool/readTextFile",
                "clientTool/writeTextFile",
            ]
        );
    }

    /// A client that declared nothing leaves every tool on the workspace, which
    /// is what keeps a terminal client from paying for a delegation it cannot
    /// answer.
    #[tokio::test]
    async fn a_client_declaring_no_filesystem_leaves_the_tools_on_the_workspace() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("main.rs"), "on disk\n").expect("file");
        let client = EditorClient::hosting("unsaved\n", []);
        let registry = registered_workspace_tools_with_client(
            directory.path(),
            Some(ClientToolIo::new("session-1", client.clone())),
        )
        .await;

        let read = registry
            .invoke(
                "read_file",
                ToolInvocation {
                    call_id: "read-1".to_owned(),
                    arguments: json!({"file_path": "main.rs"}),
                },
            )
            .await
            .expect("the workspace answers the read");
        assert_eq!(read.model_text, "1|on disk");
        registry
            .invoke(
                "write_file",
                ToolInvocation {
                    call_id: "write-1".to_owned(),
                    arguments: json!({"file_path": "created.rs", "content": "beta\n"}),
                },
            )
            .await
            .expect("the workspace answers the write");
        assert!(
            directory.path().join("created.rs").exists(),
            "the write did not reach the workspace filesystem"
        );
        assert!(
            client.methods().is_empty(),
            "an undeclared capability still reached the client: {:?}",
            client.methods()
        );
    }

    /// The same, plus the review manager the caller needs to inspect what the
    /// write captured for rewind.
    async fn registered_workspace_tools_with_review(
        root: &Path,
    ) -> (ToolRegistry, Arc<ReviewManager>) {
        let workspace = Arc::new(Workspace::open(root).expect("workspace"));
        let review = Arc::new(ReviewManager::new(workspace.clone()));
        review.begin_turn("turn-1").expect("turn");
        let policy = PermissionStore::default();
        // The reference declares `edit` and `write_file` as `ask`, so a harness
        // exercising their bodies grants them the way an operator's "allow
        // always" does rather than answering a prompt per call.
        for tool in ["edit", "write_file"] {
            policy.set_tool_permission(tool, PermissionMode::Always);
        }
        policy
            .set_trust(root, TrustDecision::Trusted, TrustRootKind::Workspace)
            .await
            .expect("trust");
        let registry = ToolRegistry::default();
        WorkspaceTools::new(workspace, review.clone())
            .register(
                &registry,
                &ToolGuard::new(policy.clone(), Arc::new(RejectApproval)),
            )
            .expect("register");
        (registry, review)
    }

    /// `write_file` creates a file, creates the parents it needs, and captures
    /// what it replaced so the turn stays rewindable.
    #[tokio::test]
    async fn write_file_creates_missing_parents_and_captures_the_turn_baseline() {
        let directory = tempdir().expect("tempdir");
        let (registry, review) = registered_workspace_tools_with_review(directory.path()).await;

        let created = registry
            .invoke(
                "write_file",
                ToolInvocation {
                    call_id: "write-1".to_owned(),
                    arguments: json!({"file_path": "nested/deep/new.txt", "content": "alpha\n"}),
                },
            )
            .await
            .expect("a missing parent directory is created");
        assert!(
            created.model_text.contains("nested/deep/new.txt"),
            "{created:?}"
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("nested/deep/new.txt")).expect("file"),
            "alpha\n"
        );
        // A hunk exists only for a path whose pre-write state the review
        // manager captured, so this is the rewind capture observed from
        // outside.
        let hunks = review.view().expect("view").pending_hunks;
        assert_eq!(
            hunks
                .iter()
                .map(|hunk| hunk.path.as_str())
                .collect::<Vec<_>>(),
            ["nested/deep/new.txt"]
        );
    }

    /// The reference refuses to overwrite and names `edit` instead, and the
    /// refusal still runs after the baseline capture, so the turn can rewind.
    #[tokio::test]
    async fn write_file_refuses_an_existing_file_and_names_the_edit_tool() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("held.txt"), "original\n").expect("file");
        let (registry, _review) = registered_workspace_tools_with_review(directory.path()).await;

        let refused = registry
            .invoke(
                "write_file",
                ToolInvocation {
                    call_id: "write-1".to_owned(),
                    arguments: json!({"file_path": "held.txt", "content": "replacement\n"}),
                },
            )
            .await
            .expect_err("an existing file is not overwritten");
        assert!(refused.to_string().contains("already exists"), "{refused}");
        assert!(refused.to_string().contains("edit"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("held.txt")).expect("file"),
            "original\n"
        );
    }

    /// The path policy and the write limit both bind `write_file`, which is
    /// what keeps a shell-free tool from reaching outside the workspace.
    #[tokio::test]
    async fn write_file_is_bounded_by_the_workspace_root_and_the_write_limit() {
        let directory = tempdir().expect("tempdir");
        let (registry, _review) = registered_workspace_tools_with_review(directory.path()).await;

        let escaped = registry
            .invoke(
                "write_file",
                ToolInvocation {
                    call_id: "write-1".to_owned(),
                    arguments: json!({"file_path": "../outside.txt", "content": "no"}),
                },
            )
            .await
            .expect_err("a path outside the root is refused");
        assert!(
            escaped.to_string().contains("permission denied"),
            "{escaped}"
        );
        assert!(!directory.path().join("../outside.txt").exists());

        let oversized = registry
            .invoke(
                "write_file",
                ToolInvocation {
                    call_id: "write-2".to_owned(),
                    arguments: json!({
                        "file_path": "big.txt",
                        "content": "x".repeat(DEFAULT_MAX_READ_BYTES + 1),
                    }),
                },
            )
            .await
            .expect_err("a write past the limit is refused");
        assert!(
            oversized.to_string().contains("exceeding the"),
            "{oversized}"
        );
        assert!(!directory.path().join("big.txt").exists());
    }
}

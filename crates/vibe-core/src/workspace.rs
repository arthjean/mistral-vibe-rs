use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io::{self, BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::text::matches_wildcard;
use crate::tools::config::{GrepConfig, ToolConfigResolver};
use crate::tools::reference_text;

mod review;
mod search;
pub mod text_file;
mod tools;

pub use tools::WorkspaceTools;

pub use review::{RestoreTransaction, ReviewManager};
pub use search::GrepOutcome;

pub const DEFAULT_MAX_READ_BYTES: usize = 1_048_576;
/// The tag a model-facing notice is wrapped in, matching the reference name.
pub const WARNING_TAG: &str = "vibe_warning";
/// The per-directory instruction file both implementations read.
const INSTRUCTION_FILE: &str = "AGENTS.md";
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

/// What `read_file` publishes, matching reference `ReadFileResult`.
///
/// The field names and their order are the contract: the reference agent loop
/// renders the result one `field: value` line at a time, so a renamed or
/// reordered field changes what the model reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileResult {
    pub file_path: String,
    pub content: String,
    pub num_lines: usize,
    pub start_line: usize,
    pub requested_offset: Option<usize>,
    pub requested_limit: usize,
    /// [`None`] when the read stopped at a budget, because the file's real
    /// length is unknown without reading the rest of it.
    pub total_lines: Option<usize>,
    pub was_truncated: bool,
}

impl ReadFileResult {
    /// The text the model reads back.
    #[must_use]
    pub fn model_text(&self) -> String {
        reference_text::joined(&[
            ("file_path", self.file_path.clone()),
            ("content", self.content.clone()),
            ("num_lines", self.num_lines.to_string()),
            ("start_line", self.start_line.to_string()),
            (
                "requested_offset",
                reference_text::optional(self.requested_offset),
            ),
            ("requested_limit", self.requested_limit.to_string()),
            ("total_lines", reference_text::optional(self.total_lines)),
            (
                "was_truncated",
                reference_text::boolean(self.was_truncated).to_owned(),
            ),
        ])
    }
}

/// What `write_file` publishes, matching reference `WriteFileResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFileResult {
    pub file_path: String,
    pub bytes_written: usize,
    pub content: String,
}

impl WriteFileResult {
    #[must_use]
    pub fn model_text(&self) -> String {
        reference_text::joined(&[
            ("file_path", self.file_path.clone()),
            ("bytes_written", self.bytes_written.to_string()),
            ("content", self.content.clone()),
        ])
    }
}

/// What `edit` publishes, matching reference `EditResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditResult {
    pub file: String,
    pub message: String,
    pub old_string: String,
    pub new_string: String,
}

impl EditResult {
    #[must_use]
    pub fn model_text(&self) -> String {
        reference_text::joined(&[
            ("file", self.file.clone()),
            ("message", self.message.clone()),
            ("old_string", self.old_string.clone()),
            ("new_string", self.new_string.clone()),
        ])
    }
}

/// One bounded read, the way reference `read_lines_safe` bounds one.
///
/// The line budget and the byte budget both stop the read, and either one
/// leaves `total_lines` unknown: the reference only reports a total when it
/// reached the end of the file, because anything else would be a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedRead {
    pub lines: Vec<String>,
    pub total_lines: Option<usize>,
    pub was_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub path: String,
    pub is_directory: bool,
    pub bytes: u64,
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
    /// The budget the joined match list is clipped to.
    pub max_output_bytes: usize,
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
            max_output_bytes: config.max_output_bytes,
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
    pub pending_hunks: Vec<ReviewHunk>,
}

pub struct Workspace {
    canonical_root: PathBuf,
    directory: Arc<Dir>,
    max_read_bytes: usize,
    max_lines: usize,
    max_discovered_files: usize,
    injected_instructions: Mutex<BTreeSet<PathBuf>>,
    /// One lock per path, so two edits of the same file serialize their
    /// read-modify-write instead of racing and losing one of the two.
    write_locks: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
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
            write_locks: Mutex::new(BTreeMap::new()),
            next_temporary: AtomicU64::new(1),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.canonical_root
    }

    /// The absolute path a tool result names, which is what the reference
    /// publishes and what a client can resolve without a working directory.
    #[must_use]
    pub fn absolute_display(&self, relative: &Path) -> String {
        self.canonical_root
            .join(relative)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// The lock guarding one path's read-modify-write.
    pub(crate) fn write_lock(&self, relative: &Path) -> Result<Arc<Mutex<()>>, WorkspaceError> {
        let mut locks = self
            .write_locks
            .lock()
            .map_err(|_| WorkspaceError::LockPoisoned {
                surface: "file write locks",
            })?;
        Ok(locks.entry(relative.to_path_buf()).or_default().clone())
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

    /// Reads at most `limit` lines from `start_line`, bounded by `max_bytes`.
    ///
    /// Mirrors reference `read_lines_safe` (`vibe/utils/io.py:169`): the file is
    /// streamed a line at a time and the read stops at whichever budget it
    /// reaches first, so a large file is never held whole. Stopping early is
    /// what leaves the total line count unknown, and the reference says so by
    /// reporting no total rather than the count it happened to reach.
    pub fn read_lines_bounded(
        &self,
        path: impl AsRef<Path>,
        start_line: usize,
        limit: usize,
        max_bytes: usize,
    ) -> Result<BoundedRead, WorkspaceError> {
        let relative = self.confined(path.as_ref(), true)?;
        let file = self
            .directory
            .open(&relative)
            .map_err(|source| WorkspaceError::Io {
                path: relative.clone(),
                source,
            })?;
        let mut reader = io::BufReader::new(file);
        let mut collected = Vec::new();
        let mut collected_lines = 0_usize;
        let mut bytes_read = 0_usize;
        let mut line_number = 0_usize;
        let mut was_truncated = true;
        loop {
            let mut raw = Vec::new();
            let read = reader
                .read_until(b'\n', &mut raw)
                .map_err(|source| WorkspaceError::Io {
                    path: relative.clone(),
                    source,
                })?;
            if read == 0 {
                was_truncated = false;
                break;
            }
            line_number = line_number.saturating_add(1);
            if line_number < start_line.max(1) {
                continue;
            }
            if collected_lines >= limit {
                break;
            }
            if bytes_read.saturating_add(raw.len()) > max_bytes {
                let remaining = max_bytes.saturating_sub(bytes_read);
                if remaining > 0 {
                    collected.extend_from_slice(raw.get(..remaining).unwrap_or(&raw));
                }
                break;
            }
            collected.extend_from_slice(&raw);
            bytes_read = bytes_read.saturating_add(raw.len());
            collected_lines = collected_lines.saturating_add(1);
        }
        let decoded = text_file::decode(&collected);
        Ok(BoundedRead {
            lines: decoded.text.lines().map(str::to_owned).collect(),
            total_lines: (!was_truncated).then_some(line_number),
            was_truncated,
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
    /// The answer is the reference's own: one `path:line:text` string, the
    /// count that survived the cap, and whether the cap or the byte budget cut
    /// it short. Every caller inside this port reads that string, which is what
    /// keeps one shape of the answer rather than two.
    pub fn search(
        &self,
        pattern: &str,
        path: &str,
        options: &SearchOptions,
    ) -> Result<GrepOutcome, WorkspaceError> {
        search::run(self, path, pattern, options)
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

    /// Whether a confined path names a directory.
    fn is_directory(&self, relative: &Path) -> bool {
        self.directory
            .metadata(relative)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
    }

    /// Whether the path a caller wrote resolves to something that exists.
    #[must_use]
    pub fn exists_at(&self, path: &str) -> bool {
        self.confined(Path::new(path), false)
            .is_ok_and(|relative| self.exists(&relative))
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
    #[error("the search pattern is empty")]
    EmptyPattern,
    #[error("the search path `{0}` does not exist")]
    MissingSearchPath(String),
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
    #[error("checkpoint capture failed: {0}")]
    Checkpoint(String),
    #[error("{surface} lock is poisoned")]
    LockPoisoned { surface: &'static str },
    #[error("numeric limit cannot be represented on this platform")]
    LimitOverflow,
}

/// Renders a single unified-diff hunk covering the changed span.
///
/// Only the lines that actually differ, plus [`DIFF_CONTEXT_LINES`] of context,
/// reach the model. Emitting whole files would make an one-line edit cost as
/// much as the file itself.
pub(crate) fn unified_diff(before: &str, after: &str) -> String {
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

pub(crate) fn is_ignored(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        path.split('/')
            .any(|component| matches_wildcard(pattern, component))
            || matches_wildcard(pattern, path)
    })
}

pub(crate) fn path_display(path: &Path) -> String {
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
    use serde_json::json;
    use tempfile::tempdir;

    use super::tools::WorkspaceTools;
    use crate::policy::{
        ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionMode,
        PermissionStore, ToolGuard, TrustDecision, TrustRootKind,
    };
    use crate::process::ClientToolIo;
    use crate::tools::{ToolInvocation, ToolRegistry};

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
        // `rg` reads a `.gitignore` only inside a repository, and so does the
        // `ignore` crate it is built on, so the marker is what makes this
        // fixture's ignore file apply at all.
        std::fs::create_dir(directory.path().join(".git")).expect("git marker");
        std::fs::write(directory.path().join(".gitignore"), "ignored.txt\n").expect("ignore");
        std::fs::write(directory.path().join("visible.txt"), "alpha\nbeta\n").expect("visible");
        std::fs::write(directory.path().join("ignored.txt"), "alpha\n").expect("ignored");
        let workspace = Workspace::open(directory.path()).expect("workspace");
        let discovered = workspace.discover().expect("discover");
        assert!(discovered.iter().any(|entry| entry.path == "visible.txt"));
        assert!(discovered.iter().all(|entry| entry.path != "ignored.txt"));
        let read = workspace.read("visible.txt", 2, Some(1)).expect("read");
        assert_eq!(read.numbered_content, "2|beta");
        let matched = workspace
            .search("alpha", ".", &probe_options(10, true))
            .expect("search");
        assert_eq!(matched.match_count, 1);
        assert_eq!(matched.matches, "./visible.txt:1:alpha");

        // `use_default_ignore: false` drops the .gitignore entries and keeps
        // the always-excluded directories, matching the reference split.
        let unfiltered = workspace
            .search("alpha", ".", &probe_options(10, false))
            .expect("search");
        assert_eq!(unfiltered.match_count, 2);
    }

    #[test]
    fn search_reaches_past_the_reader_page_limit() {
        let directory = tempdir().expect("tempdir");
        let mut content = "filler\n".repeat(DEFAULT_MAX_LINES + 10);
        content.push_str("needle\n");
        std::fs::write(directory.path().join("long.txt"), content).expect("long file");
        let workspace = Workspace::open(directory.path()).expect("workspace");

        let matched = workspace
            .search("needle", ".", &probe_options(10, true))
            .expect("search");

        assert_eq!(matched.match_count, 1);
        assert_eq!(
            matched.matches,
            format!("./long.txt:{}:needle", DEFAULT_MAX_LINES + 11)
        );
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

        // The plan is the log's, so its order is the order the dropped turns
        // first touched each path rather than an alphabetical one.
        assert_eq!(
            review.restorable_paths_at(4).expect("latest paths"),
            vec!["main.txt", "generated/later.txt"]
        );
        assert_eq!(
            review.restorable_paths_at(2).expect("earlier paths"),
            vec!["generated/first.txt", "main.txt", "generated/later.txt"]
        );

        let staged = review
            .stage_restore_to_message(4)
            .expect("staged restoration");
        assert!(staged.errors.is_empty(), "every path was writable");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("main.txt")).expect("staged main"),
            "zero\n"
        );
        assert!(!directory.path().join("generated/later.txt").exists());
        assert!(directory.path().join("generated/first.txt").exists());
        staged.transaction.rollback().expect("explicit rollback");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("main.txt")).expect("rolled back main"),
            "two\n"
        );
        assert!(directory.path().join("generated/later.txt").exists());

        let fork = review.fork_at(4).expect("checkpoint fork");
        assert_eq!(
            fork.with_log(|log| (1..=4)
                .filter(|turn| log.history().has_turn(*turn))
                .collect::<Vec<_>>())
                .expect("forked log"),
            vec![2],
            "the fork keeps the turns before the cut and drops the rest"
        );
        let restored = review
            .stage_restore_to_message(2)
            .expect("earlier restoration")
            .transaction
            .commit();
        assert_eq!(
            restored,
            vec!["generated/first.txt", "main.txt", "generated/later.txt"]
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
        assert_eq!(result.typed_result["content"], "        1\u{2192}safe");

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
        assert_eq!(read.typed_result["content"], "        2\u{2192}beta");
        assert_eq!(read.typed_result["requested_offset"], 2);

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
        assert_eq!(grep.typed_result["matches"], "./visible.txt:1:alpha");
        assert_eq!(grep.typed_result["match_count"], 1);
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

        assert_eq!(blank.typed_result["content"], "        1\u{2192}");
        assert_eq!(blank.typed_result["num_lines"], 1);
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

        assert_eq!(matched.typed_result["match_count"], 3);
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
        // The diff moved to the display payload: the model reads the reference
        // fields, and the transcript renders the hunk.
        assert!(
            replaced.display["diff"]
                .as_str()
                .is_some_and(|diff| diff.contains("+two")),
            "{replaced:?}"
        );
        assert!(
            replaced
                .model_text
                .contains("every occurrence was replaced")
        );
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
        assert_eq!(read.typed_result["total_lines"], json!(200));
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
        let paths = matched.typed_result["match_count"]
            .as_u64()
            .expect("a match count");
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
            capped.typed_result["match_count"], 2,
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
        // The budget clips the match list rather than the rendered result, and
        // the flag is what tells the model the answer was cut short.
        assert_eq!(
            clipped.typed_result["matches"].as_str().map(str::len),
            Some(12),
            "{}",
            clipped.model_text
        );
        assert_eq!(clipped.typed_result["was_truncated"], true);
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
        assert_eq!(
            read.typed_result["content"],
            "        1\u{2192}unsaved one\n        2\u{2192}unsaved two"
        );
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
        assert_eq!(edited.typed_result["new_string"], "gamma");
        assert!(
            edited.display["diff"]
                .as_str()
                .is_some_and(|diff| diff.contains("+gamma")),
            "{edited:?}"
        );
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
        assert_eq!(read.typed_result["content"], "        1\u{2192}on disk");
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

    // ----------------------------------------------------------------
    // EP-034: the builtin bodies
    // ----------------------------------------------------------------

    /// US-112: the walk honors `.gitignore` and `.ignore` inside a repository
    /// and drops both when the call asks, while the configured exclusion globs
    /// apply either way.
    #[tokio::test]
    async fn grep_separates_the_ignore_files_from_the_configured_exclusions() {
        let directory = tempdir().expect("tempdir");
        std::fs::create_dir(directory.path().join(".git")).expect("git marker");
        std::fs::create_dir(directory.path().join("build")).expect("build");
        std::fs::write(directory.path().join(".gitignore"), "gitignored.txt\n").expect("gitignore");
        std::fs::write(directory.path().join(".ignore"), "plainignored.txt\n").expect("ignore");
        for name in [
            "kept.txt",
            "gitignored.txt",
            "plainignored.txt",
            "build/out.txt",
        ] {
            std::fs::write(directory.path().join(name), "needle\n").expect("seed");
        }
        let (registry, _) = registered_with_settings(directory.path(), "").await;

        let honored = registry
            .invoke(
                "grep",
                ToolInvocation {
                    call_id: "grep-1".to_owned(),
                    arguments: json!({"pattern": "needle"}),
                },
            )
            .await
            .expect("search");
        assert_eq!(
            honored.typed_result["matches"], "./kept.txt:1:needle",
            "both ignore files and the declared `build/` exclusion apply"
        );

        let unfiltered = registry
            .invoke(
                "grep",
                ToolInvocation {
                    call_id: "grep-2".to_owned(),
                    arguments: json!({"pattern": "needle", "use_default_ignore": false}),
                },
            )
            .await
            .expect("search");
        let matches = unfiltered.typed_result["matches"]
            .as_str()
            .expect("a match list");
        assert!(matches.contains("./gitignored.txt:1:needle"), "{matches}");
        assert!(matches.contains("./plainignored.txt:1:needle"), "{matches}");
        assert!(
            !matches.contains("build/out.txt"),
            "the configured exclusions survive `use_default_ignore: false`: {matches}"
        );
    }

    /// US-112: a binary file is walked past rather than matched or raised on,
    /// and a hidden file is skipped the way `rg` skips one.
    #[tokio::test]
    async fn grep_skips_a_binary_file_without_failing_the_call() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("text.txt"), "needle\n").expect("text");
        std::fs::write(directory.path().join("blob.bin"), b"needle\0needle\n").expect("binary");
        let (registry, _) = registered_with_settings(directory.path(), "").await;

        let matched = registry
            .invoke(
                "grep",
                ToolInvocation {
                    call_id: "grep-1".to_owned(),
                    arguments: json!({"pattern": "needle"}),
                },
            )
            .await
            .expect("a binary file is not an error");

        assert_eq!(matched.typed_result["matches"], "./text.txt:1:needle");
    }

    /// US-112: a search that runs out of time answers nothing rather than
    /// returning what it had as though it were complete.
    #[test]
    fn a_search_past_its_timeout_discards_the_partial_answer() {
        let directory = tempdir().expect("tempdir");
        for index in 0..64 {
            std::fs::write(directory.path().join(format!("f{index}.txt")), "needle\n")
                .expect("seed");
        }
        let workspace = Workspace::open(directory.path()).expect("workspace");
        let options = SearchOptions {
            timeout: Some(Duration::ZERO),
            ..SearchOptions::default()
        };

        let error = workspace
            .search("needle", ".", &options)
            .expect_err("an expired budget answers nothing");

        assert!(error.to_string().contains("did not finish"), "{error}");
    }

    /// US-113: a subdirectory's `AGENTS.md` reaches the model once, and the
    /// second read of the same directory pays for it again in neither the text
    /// nor the progress stream.
    #[tokio::test]
    async fn a_subdirectory_instruction_file_is_injected_once_per_session() {
        let directory = tempdir().expect("tempdir");
        std::fs::create_dir_all(directory.path().join("nested/deeper")).expect("nested");
        std::fs::write(directory.path().join("AGENTS.md"), "the root rules\n").expect("root");
        std::fs::write(
            directory.path().join("nested/AGENTS.md"),
            "the nested rules\n",
        )
        .expect("nested rules");
        std::fs::write(directory.path().join("nested/deeper/main.rs"), "body\n").expect("file");
        let (registry, _) = registered_with_settings(directory.path(), "").await;

        let invocation = || ToolInvocation {
            call_id: "read-1".to_owned(),
            arguments: json!({"file_path": "nested/deeper/main.rs"}),
        };
        let first = registry
            .invoke("read_file", invocation())
            .await
            .expect("read");
        assert!(
            first.model_text.contains("the nested rules"),
            "{}",
            first.model_text
        );
        assert!(
            !first.model_text.contains("the root rules"),
            "the root's own instructions travel with the session prompt, not with a read: {}",
            first.model_text
        );
        assert_eq!(
            first.display["discovered"],
            json!([format!(
                "{}/nested",
                std::fs::canonicalize(directory.path())
                    .expect("canonical")
                    .display()
            )])
        );

        let second = registry
            .invoke("read_file", invocation())
            .await
            .expect("read");
        assert!(
            !second.model_text.contains("the nested rules"),
            "{}",
            second.model_text
        );
        assert_eq!(second.display["discovered"], json!([]));
    }

    /// US-113: the three ways a path can be unreadable each name their own
    /// cause, so a model can tell a typo from a directory.
    #[tokio::test]
    async fn read_file_names_each_unreadable_path_for_what_it_is() {
        let directory = tempdir().expect("tempdir");
        std::fs::create_dir(directory.path().join("nested")).expect("nested");
        let (registry, _) = registered_with_settings(directory.path(), "").await;

        let mut messages = Vec::new();
        for (index, path) in ["", "nowhere.txt", "nested"].iter().enumerate() {
            let error = registry
                .invoke(
                    "read_file",
                    ToolInvocation {
                        call_id: format!("read-{index}"),
                        arguments: json!({"file_path": path}),
                    },
                )
                .await
                .expect_err("an unreadable path is refused");
            messages.push(error.to_string());
        }

        assert!(messages[0].contains("file_path"), "{:?}", messages[0]);
        assert!(messages[1].contains("no file exists"), "{:?}", messages[1]);
        assert!(messages[2].contains("is a directory"), "{:?}", messages[2]);
        assert_eq!(
            messages.iter().collect::<BTreeSet<_>>().len(),
            3,
            "each cause needs its own message: {messages:?}"
        );
    }

    /// US-114: `write_file` refuses an existing file before it opens anything,
    /// and the create-new open refuses it again for a file that appeared in
    /// between.
    #[tokio::test]
    async fn write_file_refuses_an_existing_file_before_and_during_the_write() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("taken.txt"), "original\n").expect("seed");
        let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
        let review = ReviewManager::new(workspace.clone());
        review.begin_turn("turn-1").expect("turn");
        let (registry, _) = registered_with_settings(directory.path(), "").await;

        let refused = registry
            .invoke(
                "write_file",
                ToolInvocation {
                    call_id: "write-1".to_owned(),
                    arguments: json!({"file_path": "taken.txt", "content": "replacement\n"}),
                },
            )
            .await
            .expect_err("an existing file is refused");
        assert!(refused.to_string().contains("already exists"), "{refused}");

        // The same refusal without the pre-check, which is the path a file
        // created between the check and the open would take.
        let raced = review
            .write("taken.txt", b"replacement\n")
            .expect_err("the create-new open refuses it too");
        assert!(raced.to_string().contains("already exists"), "{raced}");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("taken.txt")).expect("unchanged"),
            "original\n"
        );
    }

    /// US-114: a file written in another codec with another line ending keeps
    /// both, so a one-line change does not rewrite the whole file.
    #[tokio::test]
    async fn edit_preserves_the_encoding_and_the_line_ending_it_found() {
        let directory = tempdir().expect("tempdir");
        // "café\r\nlatin\r\n" in Latin-1: the accented byte is 0xE9, which is
        // not valid UTF-8 on its own.
        std::fs::write(
            directory.path().join("legacy.txt"),
            b"caf\xe9\r\nlatin\r\n".as_slice(),
        )
        .expect("seed");
        let (registry, _) = registered_with_settings(directory.path(), "").await;

        registry
            .invoke(
                "edit",
                ToolInvocation {
                    call_id: "edit-1".to_owned(),
                    arguments: json!({
                        "file_path": "legacy.txt",
                        "old_string": "latin",
                        "new_string": "changed",
                    }),
                },
            )
            .await
            .expect("the edit applies");

        assert_eq!(
            std::fs::read(directory.path().join("legacy.txt")).expect("read back"),
            b"caf\xe9\r\nchanged\r\n".as_slice()
        );
    }

    /// US-114: the four ways an edit can be refused each name their own cause.
    #[tokio::test]
    async fn edit_names_each_refusal_for_what_it_is() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("file.txt"), "one\none\n").expect("seed");
        let (registry, _) = registered_with_settings(directory.path(), "").await;

        let cases = [
            (
                json!({"file_path": "file.txt", "old_string": "", "new_string": "x"}),
                "empty",
            ),
            (
                json!({"file_path": "file.txt", "old_string": "one", "new_string": "one"}),
                "identical",
            ),
            (
                json!({"file_path": "file.txt", "old_string": "absent", "new_string": "x"}),
                "is stale",
            ),
            (
                json!({"file_path": "file.txt", "old_string": "one", "new_string": "two"}),
                "matches 2 locations",
            ),
        ];
        for (index, (arguments, expected)) in cases.into_iter().enumerate() {
            let error = registry
                .invoke(
                    "edit",
                    ToolInvocation {
                        call_id: format!("edit-{index}"),
                        arguments,
                    },
                )
                .await
                .expect_err("the edit is refused");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    /// US-114: two edits of one file serialize, so the second reads what the
    /// first wrote rather than the original, and neither sees a half-written
    /// file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_edits_of_one_file_serialize_rather_than_losing_one() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("file.txt"), "alpha beta\n").expect("seed");
        let (registry, _) = registered_with_settings(directory.path(), "").await;
        let registry = Arc::new(registry);

        let first = {
            let registry = registry.clone();
            tokio::spawn(async move {
                registry
                    .invoke(
                        "edit",
                        ToolInvocation {
                            call_id: "edit-1".to_owned(),
                            arguments: json!({
                                "file_path": "file.txt",
                                "old_string": "alpha",
                                "new_string": "ALPHA",
                            }),
                        },
                    )
                    .await
            })
        };
        let second = {
            let registry = registry.clone();
            tokio::spawn(async move {
                registry
                    .invoke(
                        "edit",
                        ToolInvocation {
                            call_id: "edit-2".to_owned(),
                            arguments: json!({
                                "file_path": "file.txt",
                                "old_string": "beta",
                                "new_string": "BETA",
                            }),
                        },
                    )
                    .await
            })
        };
        first.await.expect("join").expect("the first edit applies");
        second
            .await
            .expect("join")
            .expect("the second edit applies");

        assert_eq!(
            std::fs::read_to_string(directory.path().join("file.txt")).expect("read back"),
            "ALPHA BETA\n",
            "one of the two edits was written over the other"
        );
    }

    /// US-114: the file an edit is about to change is snapshotted before the
    /// handler runs, so a revert restores what was there.
    #[tokio::test]
    async fn a_mutating_call_snapshots_the_file_before_it_changes_it() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("file.txt"), "before\n").expect("seed");
        let workspace = Arc::new(Workspace::open(directory.path()).expect("workspace"));
        let review = Arc::new(ReviewManager::new(workspace.clone()));
        review.begin_turn("turn-1").expect("turn");
        let policy = PermissionStore::default();
        policy.set_tool_permission("edit", PermissionMode::Always);
        policy
            .set_trust(
                directory.path(),
                TrustDecision::Trusted,
                TrustRootKind::Workspace,
            )
            .await
            .expect("trust");
        let registry = ToolRegistry::default();
        WorkspaceTools::new(workspace, review.clone())
            .register(
                &registry,
                &ToolGuard {
                    policy: policy.clone(),
                    approval: Arc::new(RejectApproval),
                    config: policy.tool_config(),
                    scratchpad: None,
                },
            )
            .expect("register");

        registry
            .invoke(
                "edit",
                ToolInvocation {
                    call_id: "edit-1".to_owned(),
                    arguments: json!({
                        "file_path": "file.txt",
                        "old_string": "before",
                        "new_string": "after",
                    }),
                },
            )
            .await
            .expect("the edit applies");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("file.txt")).expect("changed"),
            "after\n"
        );

        review.seal_turn().expect("the turn seals");
        review.revert().expect("the turn reverts");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("file.txt")).expect("restored"),
            "before\n"
        );
    }
}

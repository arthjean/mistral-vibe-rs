use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};

use crate::text::matches_wildcard;

mod confinement;
mod context;
mod diff;
mod edit;
mod error;
mod results;
mod review;
mod search;
pub mod text_file;
pub(crate) mod tool_path;
mod tools;

pub use context::{GitInspector, GitInspectorFuture, GitState, ProjectContext};
pub use edit::{EditOccurrence, EditOperation};
pub use error::WorkspaceError;
pub use results::{
    BoundedRead, EditResult, FileEntry, FileRead, MutationResult, ReadFileResult, WriteFileResult,
};
pub use review::{RestoreTransaction, ReviewManager};
pub use search::{GrepOutcome, SearchOptions};
pub use tools::WorkspaceTools;

use diff::unified_diff;

pub const DEFAULT_MAX_READ_BYTES: usize = 1_048_576;
/// The tag a model-facing notice is wrapped in, matching the reference name.
pub const WARNING_TAG: &str = "vibe_warning";
pub const DEFAULT_MAX_LINES: usize = 2_000;
pub const DEFAULT_MAX_DISCOVERED_FILES: usize = 10_000;

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
}

impl Workspace {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let canonical_root =
            crate::platform::canonicalize(path.as_ref()).map_err(|source| WorkspaceError::Io {
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
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.canonical_root
    }

    /// The absolute path a tool result names, which is what the reference
    /// publishes (`str(path)`, so native separators) and what a client can
    /// resolve without a working directory.
    #[must_use]
    pub fn absolute_display(&self, relative: &Path) -> String {
        self.canonical_root
            .join(relative)
            .to_string_lossy()
            .into_owned()
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
        let (directory, relative_to) = self.dir_for(relative)?;
        let mut file = directory
            .open(&relative_to)
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
        let relative = self.located(path.as_ref(), true)?;
        let (directory, relative_to) = self.dir_for(&relative)?;
        let file = directory
            .open(&relative_to)
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

    fn write_new(&self, path: &Path, content: &[u8]) -> Result<MutationResult, WorkspaceError> {
        let relative = self.located(path, false)?;
        let (directory, relative_to) = self.dir_for(&relative)?;
        if content.len() > self.max_read_bytes {
            return Err(WorkspaceError::WriteLimit {
                actual: content.len(),
                limit: self.max_read_bytes,
            });
        }
        // The reference `WriteFileConfig.create_parent_dirs` defaults to true,
        // so a write into a directory that does not exist yet creates it rather
        // than failing.
        if let Some(parent) = relative_to
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            directory
                .create_dir_all(parent)
                .map_err(|source| WorkspaceError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = directory
            .open_with(&relative_to, &options)
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
            occurrences: Vec::new(),
        })
    }

    /// [`crate::atomic_file::write_atomically`]'s recipe, staged and renamed
    /// through the confined directory handle rather than by path, so the
    /// replacement cannot be redirected outside the workspace. The file keeps
    /// the process default mode, as every file the user's tools write.
    fn atomic_replace(&self, relative: &Path, content: &[u8]) -> Result<(), WorkspaceError> {
        let (directory, relative_to) = self.dir_for(relative)?;
        let parent = relative_to
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let temporary = parent.join(crate::atomic_file::temporary_name("vibe"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = directory
            .open_with(&temporary, &options)
            .map_err(|source| WorkspaceError::Io {
                path: temporary.clone(),
                source,
            })?;
        if let Err(source) = file.write_all(content).and_then(|()| file.sync_all()) {
            let _ = directory.remove_file(&temporary);
            return Err(WorkspaceError::Io {
                path: temporary,
                source,
            });
        }
        directory
            .rename(&temporary, &directory, &relative_to)
            .map_err(|source| {
                let _ = directory.remove_file(&temporary);
                WorkspaceError::Io {
                    path: relative.to_path_buf(),
                    source,
                }
            })?;
        #[cfg(unix)]
        directory
            .open(parent)
            .and_then(|parent| parent.sync_all())
            .map_err(|source| WorkspaceError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        Ok(())
    }

    fn read_raw(&self, relative: &Path) -> Result<Vec<u8>, WorkspaceError> {
        let (directory, relative_to) = self.dir_for(relative)?;
        directory
            .read(&relative_to)
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
        let (directory, relative_to) = self.dir_for(relative)?;
        let metadata = directory
            .metadata(&relative_to)
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
        self.dir_for(relative)
            .is_ok_and(|(directory, relative_to)| directory.metadata(&relative_to).is_ok())
    }

    /// Whether a located path names a directory.
    fn is_directory(&self, relative: &Path) -> bool {
        self.dir_for(relative)
            .is_ok_and(|(directory, relative_to)| {
                directory
                    .metadata(&relative_to)
                    .is_ok_and(|metadata| metadata.is_dir())
            })
    }

    /// Whether the path a caller wrote resolves to something that exists.
    #[must_use]
    pub fn exists_at(&self, path: &str) -> bool {
        self.located(Path::new(path), false)
            .is_ok_and(|relative| self.exists(&relative))
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
mod workspace_tests;

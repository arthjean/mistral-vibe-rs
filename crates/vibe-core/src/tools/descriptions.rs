//! The description an operator writes, and where it is read from.
//!
//! Reference `ToolManager._compute_search_paths`
//! (`vibe/core/tools/manager.py:146`) builds one ordered list of tool
//! directories, and `_iter_tool_descriptions` (`:227`) turns each of them into
//! a `prompts` directory whose `<name>.md` files replace the description the
//! matching tool publishes. `available_tool_specs` (`:599`) then prefers that
//! text over the tool's own, which is what lets an operator redescribe a
//! builtin, an MCP tool or a connector tool without touching the binary.
//!
//! Two things differ from the reference and are deliberate:
//!
//! - The reference list starts with `DEFAULT_TOOL_DIR`, the package directory
//!   holding the builtin implementations and their prompt files. This port
//!   compiles its builtin descriptions in, so there is no such directory to
//!   walk and the compiled description is what a search path overrides. The
//!   ordering of everything after it is reproduced exactly, which is what
//!   `descriptions_parity_tests` measures.
//! - The reference walks these directories to *import* tool classes as well.
//!   This port reads them for descriptions only; an implementation dropped in
//!   `tool_paths` is not loaded. `docs/parity.md` records the bound.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::workspace::text_file::decode;

/// The directory, under every search path, that holds the description files.
const PROMPTS_DIRECTORY: &str = "prompts";

/// The extension a search path entry may carry while still naming a directory
/// to read: reference `_iter_tool_classes` accepts a module file and reads the
/// prompts next to it.
const MODULE_EXTENSION: &str = "py";

/// The inputs [`search_paths`] resolves the tool directories from.
///
/// Every one of them is passed in rather than read from the environment, so a
/// test drives the same code a session does over a scratch tree.
#[derive(Debug, Clone, Copy)]
pub struct SearchInputs<'a> {
    /// `tool_paths` entries, verbatim: `~` expansion and anchoring happen here,
    /// where the home and the working directory are known.
    pub configured: &'a [String],
    /// The project tool directories, already gated on the project source and on
    /// the workspace trust by [`crate::config::HarnessFiles::project_tools_dirs`].
    pub projects: &'a [PathBuf],
    /// The user tool directories, empty when the user source is disabled, from
    /// [`crate::config::HarnessFiles::user_tools_dirs`].
    pub user: &'a [PathBuf],
    /// The operator's home, which a leading `~` expands to.
    pub user_home: Option<&'a Path>,
    /// What a relative `tool_paths` entry is anchored on. This is the session's
    /// working directory, not the process's: reference `_expand_paths` calls
    /// `Path.resolve()` and lands on the process directory, which for a server
    /// hosting several sessions is not the directory the operator wrote the
    /// entry against.
    pub working_directory: &'a Path,
}

/// The tool directories to read descriptions from, in reference order.
///
/// Reference `_compute_search_paths` concatenates `config.tool_paths`, the
/// project tool directories and the user tool directories, then deduplicates on
/// the resolved path keeping the first occurrence, so a symlinked spelling of a
/// directory already listed is read once. An entry that cannot be canonicalized,
/// which is every entry naming a path that does not exist, is kept as written:
/// `Path.resolve()` is non-strict there, and dropping the entry would make one
/// bad line in a configuration file empty the rest of the list.
#[must_use]
pub fn search_paths(inputs: &SearchInputs<'_>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = inputs
        .configured
        .iter()
        .map(|entry| anchor(entry, inputs.user_home, inputs.working_directory))
        .collect();
    candidates.extend(inputs.projects.iter().cloned());
    candidates.extend(inputs.user.iter().cloned());

    let mut unique: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        let resolved = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        if !unique.contains(&resolved) {
            unique.push(resolved);
        }
    }
    unique
}

/// Reference `_expand_paths`: a leading `~` becomes the home directory and a
/// relative entry is anchored, so both spellings name one directory rather than
/// one directory per process that reads them.
fn anchor(entry: &str, home: Option<&Path>, working_directory: &Path) -> PathBuf {
    let path = Path::new(entry);
    let expanded = path.strip_prefix("~").map_or_else(
        |_| path.to_path_buf(),
        |rest| home.map_or_else(|| path.to_path_buf(), |home| home.join(rest)),
    );
    if expanded.is_absolute() {
        expanded
    } else {
        working_directory.join(expanded)
    }
}

/// The `prompts` directory a search path entry contributes, or [`None`] when it
/// contributes none.
///
/// Reference `_iter_tool_descriptions` reads `<entry>/prompts` for a directory
/// and `<entry>.parent/prompts` for an entry naming a `.py` module, and skips
/// anything else, which is how a `tool_paths` entry naming a file that never
/// existed costs nothing.
#[must_use]
pub fn prompts_dir(entry: &Path) -> Option<PathBuf> {
    if entry.is_dir() {
        return Some(entry.join(PROMPTS_DIRECTORY));
    }
    if entry
        .extension()
        .is_some_and(|extension| extension == MODULE_EXTENSION)
    {
        return entry.parent().map(|parent| parent.join(PROMPTS_DIRECTORY));
    }
    None
}

/// The overrides `roots` publish, keyed by the file stem, later roots winning.
///
/// Reference `_iter_tool_descriptions` yields `(stem, text)` pairs into a
/// dictionary, so the last search path holding a stem is the one that decides.
/// A file that cannot be read is skipped, and so is one whose text is blank:
/// the tool then keeps its own description rather than publishing an empty one.
#[must_use]
pub fn read_overrides(roots: &[PathBuf]) -> BTreeMap<String, String> {
    let mut overrides = BTreeMap::new();
    for root in roots {
        let Some(prompts) = prompts_dir(root) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(&prompts) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
            .collect();
        files.sort();
        for file in files {
            let Some(stem) = file.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(&file) else {
                continue;
            };
            let text = decode(&bytes).text;
            if text.trim().is_empty() {
                continue;
            }
            overrides.insert(stem.to_owned(), text);
        }
    }
    overrides
}

/// What a session's published surface asks for its description overrides.
///
/// The registry holds one of these rather than a resolved map, because
/// reference `ToolManager` recomputes its descriptions per construction and a
/// file written while a session is open must reach the next publication rather
/// than a cached copy of the previous one.
pub trait DescriptionSource: Send + Sync + fmt::Debug {
    /// The overrides to apply to the surface being published, keyed by tool
    /// name.
    fn overrides(&self) -> BTreeMap<String, String>;
}

/// The [`DescriptionSource`] a session installs: a list of tool directories,
/// re-read at every publication.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectoryDescriptions {
    roots: Vec<PathBuf>,
}

impl DirectoryDescriptions {
    #[must_use]
    pub const fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }
}

impl DescriptionSource for DirectoryDescriptions {
    fn overrides(&self) -> BTreeMap<String, String> {
        read_overrides(&self.roots)
    }
}

#[cfg(test)]
mod descriptions_tests;

#[cfg(test)]
mod descriptions_parity_tests;

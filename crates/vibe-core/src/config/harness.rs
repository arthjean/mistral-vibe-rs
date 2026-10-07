//! Which configuration files this port reads, and which one a write lands in.
//!
//! Two rules live here. Project discovery walks up from the working directory
//! until it finds a `.vibe/config.toml` or reaches the directory holding the
//! vibe home, so opening a subdirectory of a repository keeps its project
//! configuration. Source resolution then decides, from the enabled sources and
//! the workspace trust, which file backs the selected layer, which roots are
//! open, and whether a write may touch disk at all.
//!
//! Reference `vibe/core/config/layers/project.py` for the walk and
//! `vibe/core/config/harness_files/_harness_manager.py` plus
//! `vibe/core/config/default_orchestrator.py` for the selection.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use super::{CONFIG_FILE, ConfigPaths, ConfigTarget, PROJECT_DIRECTORY};

/// The directory under a project root, and under the vibe home, that holds
/// prompt overrides.
const PROMPTS_DIRECTORY: &str = "prompts";

/// The directory under a project root, and under the vibe home, that holds
/// tool directories: `{root}/.vibe/tools` and `{vibe_home}/tools`.
const TOOLS_DIRECTORY: &str = "tools";

/// The directory under a project's `.vibe`, and under the vibe home, that holds
/// agent profiles.
const AGENTS_DIRECTORY: &str = "agents";

/// The instruction file a directory carries. Reference `AGENTS_MD_FILENAME`.
pub const AGENTS_FILE: &str = "AGENTS.md";

/// A configuration file family the process is allowed to read and write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigSource {
    /// The vibe home file, `{vibe_home}/config.toml`.
    User,
    /// The discovered project file, `{root}/.vibe/config.toml`.
    Project,
}

impl ConfigSource {
    /// Both sources, which is what every binary in this workspace enables.
    #[must_use]
    pub fn all() -> BTreeSet<Self> {
        BTreeSet::from([Self::User, Self::Project])
    }
}

/// The project configuration file governing `working_directory`, if any.
///
/// The walk starts at the working directory and climbs one parent at a time,
/// stopping before the directory that holds `vibe_home` so the user file is
/// never picked up as a project file. The nearest file wins. Reference
/// `_discover_config_file`.
///
/// Parents are taken lexically, so the walk is finite even when the working
/// directory is reached through a symlink.
#[must_use]
pub fn discover_project_config(working_directory: &Path, vibe_home: &Path) -> Option<PathBuf> {
    let stop = vibe_home.parent();
    let mut current = Some(working_directory);
    while let Some(directory) = current {
        if Some(directory) == stop {
            break;
        }
        let candidate = directory.join(PROJECT_DIRECTORY).join(CONFIG_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        current = directory.parent();
    }
    None
}

/// The files one session reads, and the one it writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessFiles {
    paths: ConfigPaths,
    sources: BTreeSet<ConfigSource>,
    additional_roots: Vec<PathBuf>,
    project_trusted: bool,
    /// The project file's trust when a caller already resolved it, which pins
    /// it for the life of a session as upstream caches it on the layer; `None`
    /// asks the trust store on every read.
    project_file_trust: Option<bool>,
}

impl HarnessFiles {
    #[must_use]
    pub fn new(
        paths: ConfigPaths,
        sources: BTreeSet<ConfigSource>,
        additional_roots: Vec<PathBuf>,
        project_trusted: bool,
    ) -> Self {
        Self {
            paths,
            sources,
            additional_roots,
            project_trusted,
            project_file_trust: None,
        }
    }

    /// Pins the project file's trust instead of asking the trust store.
    #[must_use]
    pub const fn with_project_file_trust(mut self, trust: Option<bool>) -> Self {
        self.project_file_trust = trust;
        self
    }

    /// The file backing the selected layer, or `None` when no enabled source
    /// resolves to one and the selection is ephemeral.
    ///
    /// Reference `default_layer_resolver` in `build_default_orchestrator`: the
    /// user file while that source is enabled, then a trusted project file.
    /// Where the reference would still select the project layer for a file it
    /// never discovered, this port resolves to the ephemeral layer instead: a
    /// write with no discovered file and no user source stays in memory rather
    /// than creating a project file the operator never had.
    #[must_use]
    pub fn config_file(&self) -> Option<(ConfigTarget, PathBuf)> {
        if self.sources.contains(&ConfigSource::User) {
            return Some((ConfigTarget::User, self.user_config_file()));
        }
        self.trusted_project_config()
            .map(|path| (ConfigTarget::Project, path))
    }

    /// The discovered project file, once the source is enabled and the trust
    /// store allows the `.vibe` directory holding it.
    ///
    /// Reference `ProjectConfigLayer._check_trust`: the question is about the
    /// directory the file sits in, not the working directory, so a file found
    /// above a trusted subdirectory stays out, and so does one whose `.vibe` is
    /// declined inside a trusted directory ([`crate::trust::project_config_trusted`]).
    #[must_use]
    pub fn trusted_project_config(&self) -> Option<PathBuf> {
        if !self.sources.contains(&ConfigSource::Project) {
            return None;
        }
        let file = discover_project_config(&self.paths.working_directory, &self.paths.vibe_home)?;
        let trusted = self.project_file_trust.unwrap_or_else(|| {
            crate::trust::project_config_trusted(
                &crate::trust::TrustStore::for_vibe_home(&self.paths.vibe_home),
                &file,
                &self.paths.working_directory,
                self.project_trusted,
            )
        });
        trusted.then_some(file)
    }

    #[must_use]
    pub fn user_config_file(&self) -> PathBuf {
        self.paths.user_config()
    }

    /// The prompt directories a prompt identifier resolves against, project
    /// ones first.
    ///
    /// A project root contributes `{root}/.vibe/prompts` and the user source
    /// contributes `{vibe_home}/prompts`, each only once it exists on disk, so
    /// the list a resolution error prints names directories that are really
    /// searched. Reference `project_prompts_dirs` and `user_prompts_dirs`.
    #[must_use]
    pub fn prompts_dirs(&self) -> Vec<PathBuf> {
        let mut directories: Vec<PathBuf> = self
            .project_roots()
            .into_iter()
            .map(|root| root.join(PROJECT_DIRECTORY).join(PROMPTS_DIRECTORY))
            .filter(|directory| directory.is_dir())
            .collect();
        if self.sources.contains(&ConfigSource::User) {
            let user = self.paths.vibe_home.join(PROMPTS_DIRECTORY);
            if user.is_dir() {
                directories.push(user);
            }
        }
        directories
    }

    /// The project tool directories a description override is read from.
    ///
    /// Reference `project_tools_dirs` folds `find_local_config_dirs` over the
    /// open roots, and that walk keeps `{root}/.vibe/tools` only when it is a
    /// directory and never recurses below the root. An untrusted workspace
    /// contributes no root, so it contributes no directory either.
    #[must_use]
    pub fn project_tools_dirs(&self) -> Vec<PathBuf> {
        self.project_roots()
            .into_iter()
            .map(|root| root.join(PROJECT_DIRECTORY).join(TOOLS_DIRECTORY))
            .filter(|directory| directory.is_dir())
            .collect()
    }

    /// The user tool directory, which is `{vibe_home}/tools` once the user
    /// source is enabled and the directory exists.
    ///
    /// Reference `user_tools_dirs` returns an empty list when `"user"` is not
    /// among the sources, which is what makes a `--no-user-config` session read
    /// no override the operator wrote for every project.
    #[must_use]
    pub fn user_tools_dirs(&self) -> Vec<PathBuf> {
        if !self.sources.contains(&ConfigSource::User) {
            return Vec::new();
        }
        let directory = self.paths.vibe_home.join(TOOLS_DIRECTORY);
        if directory.is_dir() {
            vec![directory]
        } else {
            Vec::new()
        }
    }

    /// The vibe home the user files hang off.
    #[must_use]
    pub fn vibe_home(&self) -> &Path {
        &self.paths.vibe_home
    }

    /// The project agent directories: every open root's `.vibe/agents` that
    /// is a directory. Reference `project_agents_dirs`.
    #[must_use]
    pub fn project_agents_dirs(&self) -> Vec<PathBuf> {
        self.project_roots()
            .into_iter()
            .map(|root| root.join(PROJECT_DIRECTORY).join(AGENTS_DIRECTORY))
            .filter(|directory| directory.is_dir())
            .collect()
    }

    /// The user agent directory, `{vibe_home}/agents`, once the user source is
    /// enabled and the directory exists. Reference `user_agents_dirs`.
    #[must_use]
    pub fn user_agents_dirs(&self) -> Vec<PathBuf> {
        if !self.sources.contains(&ConfigSource::User) {
            return Vec::new();
        }
        let directory = self.paths.vibe_home.join(AGENTS_DIRECTORY);
        if directory.is_dir() {
            vec![directory]
        } else {
            Vec::new()
        }
    }

    /// The project plugin directories: every open root's `.vibe/plugins`
    /// that is a directory. Reference `project_plugins_dirs`.
    #[must_use]
    pub fn project_plugins_dirs(&self) -> Vec<PathBuf> {
        self.project_roots()
            .into_iter()
            .map(|root| {
                crate::plugins::paths::resolve_lax(&root)
                    .join(PROJECT_DIRECTORY)
                    .join("plugins")
            })
            .filter(|directory| directory.is_dir())
            .collect()
    }

    /// The user plugin directory, `{vibe_home}/plugins`, once the user source
    /// is enabled and the directory exists. Reference `user_plugins_dirs`.
    #[must_use]
    pub fn user_plugins_dirs(&self) -> Vec<PathBuf> {
        if !self.sources.contains(&ConfigSource::User) {
            return Vec::new();
        }
        let directory = self.paths.vibe_home.join("plugins");
        if directory.is_dir() {
            vec![directory]
        } else {
            Vec::new()
        }
    }

    /// The hook files a session loads, in the order their names are claimed:
    /// every open project's `.vibe/hooks.toml`, then the user's
    /// `{vibe_home}/hooks.toml` once the user source is enabled. Reference
    /// `hook_files`, which lists them whether or not they exist.
    #[must_use]
    pub fn hook_files(&self) -> Vec<PathBuf> {
        let mut files = self
            .project_roots()
            .into_iter()
            .map(|root| root.join(PROJECT_DIRECTORY).join(crate::hooks::HOOKS_FILE))
            .collect::<Vec<_>>();
        if self.sources.contains(&ConfigSource::User) {
            files.push(self.paths.vibe_home.join(crate::hooks::HOOKS_FILE));
        }
        files
    }

    /// Whether a write may reach the user file. Reference `persist_allowed`.
    #[must_use]
    pub fn persist_allowed(&self) -> bool {
        self.sources.contains(&ConfigSource::User)
    }

    /// The open project directories: the trusted working directory first, then
    /// the additional directories the caller opened.
    ///
    /// Every entry is absolutized and deduplicated, and an additional directory
    /// equal to the working directory is dropped as redundant. An additional
    /// directory that merely contains the working directory survives, because
    /// each root contributes its own root-level discovery. Reference
    /// `project_roots`.
    #[must_use]
    pub fn project_roots(&self) -> Vec<PathBuf> {
        let additional = self.deduplicated_additional_roots();
        let Some(workdir) = self.open_working_directory() else {
            return additional;
        };
        let mut roots = vec![workdir.clone()];
        roots.extend(additional.into_iter().filter(|root| *root != workdir));
        roots
    }

    /// Whether the working directory's `.vibe` is the vibe home itself, which
    /// is where a session opened in the operator's home directory sits.
    /// Reference `cwd_is_user_config_home`.
    #[must_use]
    pub fn cwd_is_user_config_home(&self) -> bool {
        resolve_lenient(
            &self
                .absolutize(&self.paths.working_directory)
                .join(PROJECT_DIRECTORY),
        ) == resolve_lenient(&self.paths.vibe_home)
    }

    /// Whether the project layer may be read for this working directory at
    /// all: the source is enabled and the directory's `.vibe` is not the user
    /// configuration read under another name. Reference
    /// `project_source_enabled`.
    #[must_use]
    pub fn project_source_enabled(&self) -> bool {
        self.sources.contains(&ConfigSource::Project) && !self.cwd_is_user_config_home()
    }

    /// [`Self::project_roots`] resolved through the filesystem and
    /// deduplicated again, which is the spelling reference `project_roots`
    /// publishes: its `dedup_paths` resolves every root before comparing.
    #[must_use]
    pub fn resolved_project_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for root in self.project_roots() {
            let resolved = resolve_lenient(&root);
            if !roots.contains(&resolved) {
                roots.push(resolved);
            }
        }
        roots
    }

    /// The user-level `AGENTS.md`, stripped, or empty when the user source is
    /// off or the file is missing, unreadable or blank. Reference
    /// `load_user_doc`.
    #[must_use]
    pub fn load_user_doc(&self) -> String {
        if !self.sources.contains(&ConfigSource::User) {
            return String::new();
        }
        read_stripped(&self.paths.vibe_home.join(AGENTS_FILE)).unwrap_or_default()
    }

    /// Where the user-level `AGENTS.md` is read from, as the system prompt
    /// names it: under the vibe home as the reference resolves it.
    #[must_use]
    pub fn user_doc_path(&self) -> PathBuf {
        resolve_lenient(&self.paths.vibe_home).join(AGENTS_FILE)
    }

    /// Every non-empty `AGENTS.md` from each open project root up to its trust
    /// root, outermost first within a root, each directory once across roots.
    ///
    /// Reference `load_project_docs`: the working directory's walk stops at the
    /// closest trusted ancestor the trust store records, which may sit above
    /// it, and a root the store does not cover (an `--add-dir` entry) stops at
    /// itself. A directory reached from two roots keeps the first reading.
    #[must_use]
    pub fn load_project_docs(&self, trust: &crate::trust::TrustStore) -> Vec<(PathBuf, String)> {
        let mut documents: Vec<(PathBuf, (PathBuf, String))> = Vec::new();
        for root in self.resolved_project_roots() {
            let stop = trust.find_trust_root(&root).unwrap_or_else(|| root.clone());
            for (directory, content) in collect_agents_docs(&root, &stop) {
                let key = resolve_lenient(&directory);
                if !documents.iter().any(|(known, _)| *known == key) {
                    documents.push((key, (directory, content)));
                }
            }
        }
        documents
            .into_iter()
            .map(|(_, document)| document)
            .collect()
    }

    fn open_working_directory(&self) -> Option<PathBuf> {
        if !self.project_source_enabled() || !self.project_trusted {
            return None;
        }
        Some(self.absolutize(&self.paths.working_directory))
    }

    fn deduplicated_additional_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for root in &self.additional_roots {
            let resolved = self.absolutize(root);
            if !roots.contains(&resolved) {
                roots.push(resolved);
            }
        }
        roots
    }

    /// Anchors a relative root on the working directory and folds `.` and `..`
    /// away, without touching the filesystem: a root that does not exist yet is
    /// still comparable to one that does.
    fn absolutize(&self, path: &Path) -> PathBuf {
        let anchored = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.paths.working_directory.join(path)
        };
        let mut folded = PathBuf::new();
        for component in anchored.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !folded.pop() {
                        folded.push(component.as_os_str());
                    }
                }
                other => folded.push(other.as_os_str()),
            }
        }
        folded
    }
}

/// Reference `_collect_agents_md` with `stop_inclusive=True`: the non-empty
/// instruction files from `start` up to and including `stop`, outermost first,
/// or nothing when `start` is not below `stop`.
fn collect_agents_docs(start: &Path, stop: &Path) -> Vec<(PathBuf, String)> {
    if !start.starts_with(stop) {
        return Vec::new();
    }
    let mut documents = Vec::new();
    let mut current = start.to_path_buf();
    loop {
        if let Some(content) = read_stripped(&current.join(AGENTS_FILE)) {
            documents.push((current.clone(), content));
        }
        if current == stop {
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    documents.reverse();
    documents
}

/// A file decoded the way the reference's `read_safe` decodes it and stripped,
/// or `None` when it is missing, unreadable or blank.
pub(crate) fn read_stripped(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let decoded = crate::workspace::text_file::decode(&bytes);
    let stripped = crate::text::python_strip(&decoded.text);
    (!stripped.is_empty()).then(|| stripped.to_owned())
}

/// Python's non-strict `Path.resolve()`: symlinks resolved as far as the path
/// exists, and the part that does not exist appended as written.
pub(crate) fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        let Some(name) = current.file_name().map(ToOwned::to_owned) else {
            return path.to_path_buf();
        };
        missing.push(name);
        if !current.pop() {
            return path.to_path_buf();
        }
        if let Ok(mut resolved) = std::fs::canonicalize(&current) {
            for name in missing.iter().rev() {
                resolved.push(name);
            }
            return resolved;
        }
    }
}

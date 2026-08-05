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
        }
    }

    /// The file backing the selected layer, or `None` when no enabled source
    /// resolves to one and the selection is ephemeral.
    ///
    /// A trusted project file wins and the user file is the fallback. Where the
    /// reference would still select the project layer for a file it never
    /// discovered, this port resolves to the ephemeral layer instead: a write
    /// with no discovered file and no user source stays in memory rather than
    /// creating a project file the operator never had.
    #[must_use]
    pub fn config_file(&self) -> Option<(ConfigTarget, PathBuf)> {
        if let Some(path) = self.trusted_project_config() {
            return Some((ConfigTarget::Project, path));
        }
        if self.sources.contains(&ConfigSource::User) {
            return Some((ConfigTarget::User, self.user_config_file()));
        }
        None
    }

    /// The discovered project file, once the source is enabled and the
    /// workspace is trusted.
    #[must_use]
    pub fn trusted_project_config(&self) -> Option<PathBuf> {
        if !self.sources.contains(&ConfigSource::Project) || !self.project_trusted {
            return None;
        }
        discover_project_config(&self.paths.working_directory, &self.paths.vibe_home)
    }

    #[must_use]
    pub fn user_config_file(&self) -> PathBuf {
        self.paths.user_config()
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

    fn open_working_directory(&self) -> Option<PathBuf> {
        if !self.sources.contains(&ConfigSource::Project) || !self.project_trusted {
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

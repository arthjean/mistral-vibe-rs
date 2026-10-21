//! Where a path argument lands, and the capability handle it is reached
//! through.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::Dir;

use super::{Workspace, WorkspaceError, tool_path};

impl Workspace {
    /// Where a file tool's path argument lands: relative to the root when it
    /// resolves inside it, and absolute when it resolves anywhere else.
    ///
    /// Reference `resolve_tool_path` expands a leading `~`, anchors a relative
    /// path on the working directory and resolves links, and the tool then
    /// reaches whatever that names. Whether it may is the permission chain's
    /// question, answered before the tool runs: a path outside every trusted
    /// root raises an `outside_directory` requirement there, and the session
    /// scratchpad is granted outright. This is the one place that lets a path
    /// past the root, and only the file tools and the checkpoint log their
    /// edits feed call it; every other surface stays on [`Self::confined`].
    pub(crate) fn located(
        &self,
        requested: &Path,
        must_exist: bool,
    ) -> Result<PathBuf, WorkspaceError> {
        let expanded = tool_path::expanded(requested);
        let absolute = if expanded.is_absolute() {
            expanded
        } else {
            self.canonical_root.join(expanded)
        };
        let resolved = tool_path::resolved(&absolute);
        if must_exist && let Err(source) = std::fs::metadata(&resolved) {
            return Err(WorkspaceError::Io {
                path: requested.to_path_buf(),
                source,
            });
        }
        Ok(match resolved.strip_prefix(&self.canonical_root) {
            Ok(inside) if inside.as_os_str().is_empty() => PathBuf::from("."),
            Ok(inside) => inside.to_path_buf(),
            Err(_) => resolved,
        })
    }

    /// The capability handle a located path is reached through, and the path
    /// relative to it.
    ///
    /// A relative path is the workspace's own. An absolute one was placed
    /// outside the root by [`Self::located`], already resolved, and is reached
    /// through a handle on its filesystem root, so every operation below stays
    /// the same code whichever side of the root it lands on.
    pub(super) fn dir_for(&self, located: &Path) -> Result<(Arc<Dir>, PathBuf), WorkspaceError> {
        if !located.is_absolute() {
            return Ok((self.directory.clone(), located.to_path_buf()));
        }
        let mut anchor = PathBuf::new();
        let mut below = PathBuf::new();
        for component in located.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => anchor.push(component.as_os_str()),
                other => below.push(other.as_os_str()),
            }
        }
        let directory = Dir::open_ambient_dir(&anchor, ambient_authority()).map_err(|source| {
            WorkspaceError::Io {
                path: anchor.clone(),
                source,
            }
        })?;
        if below.as_os_str().is_empty() {
            below.push(".");
        }
        Ok((Arc::new(directory), below))
    }

    pub(super) fn confined(
        &self,
        requested: &Path,
        must_exist: bool,
    ) -> Result<PathBuf, WorkspaceError> {
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
        // `Dir::canonicalize` answers relative to the confined root, so it
        // never carries a verbatim prefix and needs no `platform` helper.
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
}

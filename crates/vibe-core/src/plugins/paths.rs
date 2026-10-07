//! Portable references to files inside a plugin.
//!
//! Reference `vibe/core/plugins/_paths.py`: a snapshot never carries a host
//! path, only a plugin name and a POSIX path relative to that plugin's root,
//! with the root itself spelled `.`. A reader joins the reference against the
//! checkout it holds for that plugin.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::canonical::normalize_nfc;

/// The plugin root spelled as a reference to itself.
pub const PLUGIN_ROOT_REF: &str = ".";

/// A file or directory inside one plugin. Reference `PluginPathRef`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPathRef {
    pub plugin: String,
    pub path: String,
}

/// A target that resolves outside the plugin it was declared in.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("path {} resolves outside plugin {plugin:?}", path.display())]
pub struct PluginPathOutsideRoot {
    pub plugin: String,
    pub path: PathBuf,
}

/// The portable reference for `target` inside the plugin rooted at `root`.
/// Reference `plugin_path_ref`.
///
/// # Errors
///
/// A target that resolves outside the root.
pub fn plugin_path_ref(
    plugin: &str,
    root: &Path,
    target: &Path,
) -> Result<PluginPathRef, PluginPathOutsideRoot> {
    let resolved_root = resolve_lax(root);
    let resolved_target = resolve_lax(target);
    let Ok(relative) = resolved_target.strip_prefix(&resolved_root) else {
        return Err(PluginPathOutsideRoot {
            plugin: plugin.to_owned(),
            path: target.to_path_buf(),
        });
    };
    let relative = super::content::posix_relative(relative);
    Ok(PluginPathRef {
        plugin: normalize_nfc(plugin),
        path: if relative.is_empty() {
            PLUGIN_ROOT_REF.to_owned()
        } else {
            relative
        },
    })
}

/// Joins a reference against the root of the plugin it names. Reference
/// `resolve_plugin_path`.
#[must_use]
pub fn resolve_plugin_path(reference: &PluginPathRef, root: &Path) -> PathBuf {
    if reference.path == PLUGIN_ROOT_REF {
        return root.to_path_buf();
    }
    reference
        .path
        .split('/')
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

/// Python's `Path.resolve()` without `strict`: the longest existing prefix is
/// resolved through the filesystem and the rest is applied lexically.
#[must_use]
pub fn resolve_lax(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    crate::worktree::resolve_lenient(&absolute)
}

/// Python's `Path.resolve(strict=True)`.
///
/// # Errors
///
/// A path that does not exist, or a loop of links.
pub fn resolve_strict(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// Python's `PurePath.is_relative_to` on two resolved paths.
#[must_use]
pub fn is_relative_to(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

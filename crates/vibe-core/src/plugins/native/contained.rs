//! The containment checks every component a native plugin declares passes:
//! a declared path stays a contained POSIX path, and what it names resolves
//! inside the plugin. Reference `_contained_component_file`,
//! `_contained_component_directory` and the tree checks beside them in
//! `vibe/core/plugins/_native.py`.

use std::fs;
use std::path::{Path, PathBuf};

use super::super::paths::{file_name, is_relative_to, is_symlink, resolve_strict};
use super::super::strict::char_len;

const MAX_PLUGIN_COMPONENT_PATH_LENGTH: usize = 1024;

pub(super) fn validate_plugin_relative_path(value: &str, component: &str) -> Result<(), String> {
    if char_len(value) > MAX_PLUGIN_COMPONENT_PATH_LENGTH || !value.starts_with("./") {
        return Err(format!(
            "a {component} path has to start with './' and hold at most 1024 characters"
        ));
    }
    let relative = &value[2..];
    let parts: Vec<&str> = relative.split('/').collect();
    let pure = Path::new(relative);
    // `PurePosixPath` folds repeated and trailing slashes away, so an empty
    // part only counts when the whole remainder is empty.
    let meaningful: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|part| !part.is_empty())
        .collect();
    if meaningful.is_empty()
        || pure.is_absolute()
        || value.contains('\\')
        || meaningful.iter().any(|part| *part == "." || *part == "..")
    {
        return Err(format!(
            "a {component} path has to be a contained POSIX path"
        ));
    }
    Ok(())
}

pub(super) fn declared_candidate(root: &Path, source: &str) -> PathBuf {
    source
        .strip_prefix("./")
        .unwrap_or(source)
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

pub(super) fn validate_contained_tree(
    plugin_root: &Path,
    requested_root: &Path,
    resolved_root: &Path,
    label: &str,
) -> Result<(), String> {
    if label == "library" && is_symlink(requested_root) {
        return Err("a library path cannot be a symbolic link".to_owned());
    }
    let mut found = Vec::new();
    super::super::compatibility::walk_files(resolved_root, &mut found);
    for path in found {
        if is_symlink(&path) {
            return Err(format!("a {label} folder cannot hold a symbolic link"));
        }
        let resolved = resolve_strict(&path).map_err(|error| error.to_string())?;
        if !is_relative_to(&resolved, plugin_root) {
            return Err(format!("{label} content resolves outside the plugin"));
        }
        if !resolved.is_dir() && !resolved.is_file() {
            return Err(format!(
                "a {label} folder can hold only files and directories"
            ));
        }
    }
    Ok(())
}

pub(super) fn contained_component_file(plugin_root: &Path, path: &Path) -> Result<PathBuf, String> {
    let resolved = resolve_strict(path).map_err(|error| error.to_string())?;
    if !is_relative_to(&resolved, plugin_root) || !resolved.is_file() {
        return Err("the component has to resolve to a regular file inside the plugin".to_owned());
    }
    Ok(resolved)
}

pub(super) fn contained_component_directory(
    plugin_root: &Path,
    path: &Path,
) -> Result<PathBuf, String> {
    let resolved = resolve_strict(path).map_err(|error| error.to_string())?;
    if !is_relative_to(&resolved, plugin_root) || !resolved.is_dir() {
        return Err("the component has to resolve to a directory inside the plugin".to_owned());
    }
    Ok(resolved)
}

pub(super) fn sorted_children(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let mut children: Vec<PathBuf> = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    children.sort_by_key(|path| file_name(path));
    Ok(children)
}

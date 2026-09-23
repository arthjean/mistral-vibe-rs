//! The path argument of a file tool, read the way the reference reads it.
//!
//! Reference `ToolPath` (`vibe/core/tools/utils.py`) runs every `file_path`
//! and `path` argument through `normalize_windows_input_path`
//! (`vibe/utils/paths.py`) before the tool sees it, and `resolve_tool_path`
//! then expands a leading `~` and anchors a relative path on the working
//! directory. Both steps live here so the permission chain and the handler
//! that runs after it resolve the same file.

use std::path::{Component, Path, PathBuf};

use crate::tools::ToolError;

/// The argument as the tool reads it: surrounding whitespace removed on every
/// platform, and on Windows a Git Bash drive path (`/c/work`) folded into its
/// drive form (`C:/work`).
///
/// # Errors
///
/// On Windows, a path naming a drive without a root (`C:work`) or a root
/// without a drive (`\work`) is refused at the argument boundary, because
/// Windows would resolve it against whichever drive or directory is current.
pub fn normalized(argument: &str, raw: &str) -> Result<String, ToolError> {
    let trimmed = raw.trim();
    if !cfg!(windows) {
        return Ok(trimmed.to_owned());
    }
    let folded = fold_msys_drive(trimmed);
    if is_unanchored_windows_path(&folded) {
        return Err(ToolError::SchemaViolation {
            path: format!("/{argument}"),
            message: format!(
                "`{raw}` names a drive or a root but not both, so Windows would resolve it \
                 against whichever drive is current; give a drive and a root (`C:\\work`), a \
                 Git Bash path (`/c/work`), or a path relative to the working directory"
            ),
        });
    }
    Ok(folded)
}

/// `/c/rest` as `C:/rest`, and anything else unchanged.
///
/// Reference `_MSYS_DRIVE`: one ASCII letter after the leading slash, then
/// nothing or a separator and the rest of the path.
fn fold_msys_drive(path: &str) -> String {
    let mut characters = path.chars();
    let (Some('/'), Some(letter)) = (characters.next(), characters.next()) else {
        return path.to_owned();
    };
    let rest = characters.as_str();
    if !letter.is_ascii_alphabetic() || !(rest.is_empty() || rest.starts_with(['/', '\\'])) {
        return path.to_owned();
    }
    let suffix = if rest.is_empty() {
        "/".to_owned()
    } else {
        rest.replace('\\', "/")
    };
    format!("{}:{suffix}", letter.to_ascii_uppercase())
}

/// Whether a Windows path has a drive or a root but not both, as
/// `PureWindowsPath` splits it. A UNC share carries both.
fn is_unanchored_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    let separator = |byte: Option<&u8>| matches!(byte, Some(b'/' | b'\\'));
    if separator(bytes.first()) && separator(bytes.get(1)) {
        return false;
    }
    let has_drive =
        bytes.get(1) == Some(&b':') && bytes.first().is_some_and(u8::is_ascii_alphabetic);
    let has_root = if has_drive {
        separator(bytes.get(2))
    } else {
        separator(bytes.first())
    };
    has_drive != has_root
}

/// The path with a leading `~` replaced by the operator's home directory, as
/// `Path.expanduser` replaces it.
#[must_use]
pub fn expanded(path: &Path) -> PathBuf {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == "~" => {
            match crate::config::user_home_directory() {
                Some(home) => home.join(components.as_path()),
                None => path.to_path_buf(),
            }
        }
        _ => path.to_path_buf(),
    }
}

/// An absolute path resolved the way `Path.resolve()` resolves one that may
/// not exist: every existing prefix has its links followed, and whatever does
/// not exist yet is kept as written with `.` and `..` applied to it.
#[must_use]
pub fn resolved(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                if let Ok(canonical) = std::fs::canonicalize(&resolved) {
                    resolved = canonical;
                }
            }
        }
    }
    resolved
}

#[cfg(test)]
mod tool_path_tests;

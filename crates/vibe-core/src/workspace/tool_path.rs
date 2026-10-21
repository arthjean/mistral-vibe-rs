//! The path argument of a file tool, read the way the reference reads it.
//!
//! Reference `ToolPath` (`vibe/core/tools/utils.py`) runs every `file_path`
//! and `path` argument through `normalize_windows_input_path`
//! (`vibe/utils/paths.py`) before the tool sees it, and `resolve_tool_path`
//! then expands a leading `~` and anchors a relative path on the working
//! directory. The first step lives here; the second is
//! [`crate::platform::expand_home`] then [`crate::platform::resolve_lenient`],
//! which the permission chain and the handler that runs after it both take, so
//! the two resolve the same file.

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
pub(crate) fn fold_msys_drive(path: &str) -> String {
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

#[cfg(test)]
mod tool_path_tests;

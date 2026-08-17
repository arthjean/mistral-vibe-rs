//! Path validation and comparison for the roots a session is opened over.

use std::path::{Component, Path, PathBuf};

use crate::protocol::AcpError;

pub(crate) fn validate_session_paths(
    cwd: &str,
    additional_directories: Option<&[String]>,
    max_additional_directories: usize,
) -> Result<(), AcpError> {
    let additional_directories = additional_directories.unwrap_or_default();
    if additional_directories.len() > max_additional_directories {
        return Err(AcpError::InvalidParams(format!(
            "additionalDirectories cannot contain more than {max_additional_directories} roots"
        )));
    }
    require_absolute_cwd(cwd)?;
    if let Some(path) = additional_directories
        .iter()
        .find(|path| !Path::new(path).is_absolute())
    {
        return Err(AcpError::InvalidParams(format!(
            "additional directory `{path}` must be an absolute path"
        )));
    }
    Ok(())
}

pub(crate) fn require_absolute_cwd(cwd: &str) -> Result<(), AcpError> {
    if Path::new(cwd).is_absolute() {
        Ok(())
    } else {
        Err(AcpError::InvalidParams(
            "cwd must be an absolute path".to_owned(),
        ))
    }
}

pub(crate) fn ensure_matching_cwd(
    requested: &str,
    persisted: &str,
    operation: &str,
) -> Result<(), AcpError> {
    if same_path(requested, persisted) {
        Ok(())
    } else {
        Err(AcpError::InvalidParams(format!(
            "cannot {operation} a session saved for `{persisted}` from cwd `{requested}`"
        )))
    }
}

pub(crate) fn same_path(left: &str, right: &str) -> bool {
    resolved_path(left) == resolved_path(right)
}

/// Resolves a path for comparison. Unresolvable paths fall back to lexical
/// normalization rather than a raw string, so `/a/../b` and `/b` still match.
fn resolved_path(value: &str) -> PathBuf {
    std::fs::canonicalize(value).unwrap_or_else(|_| lexically_normalized(Path::new(value)))
}

fn lexically_normalized(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other),
        }
    }
    normalized
}

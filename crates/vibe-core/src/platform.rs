use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;

mod console;
mod executables;
mod home;
mod paths;

pub use console::{BackgroundCommand, unicode_vars};
#[cfg(windows)]
pub use console::{CREATE_NO_WINDOW, background_creation_flags};
pub use executables::{executable_names, find_executable, is_executable_file, path_candidates};
pub use home::{
    HomeNotFound, VIBE_HOME_ENV_VAR, expand_home, user_home, vibe_home, vibe_home_or_local,
};
pub use paths::{canonicalize, simplified, strip_verbatim_prefix};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Posix,
    Windows,
    GitBash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyPath {
    pub platform: Platform,
    pub root: String,
    pub components: Vec<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PathPolicyError {
    #[error("path is empty")]
    Empty,
    #[error("path contains parent traversal")]
    ParentTraversal,
    #[error("Windows path requires a drive or UNC root")]
    MissingWindowsRoot,
}

pub fn parse_policy_path(platform: Platform, raw: &str) -> Result<PolicyPath, PathPolicyError> {
    if raw.is_empty() {
        return Err(PathPolicyError::Empty);
    }
    match platform {
        Platform::Posix => parse_posix(raw),
        Platform::Windows => parse_windows(raw),
        Platform::GitBash => parse_git_bash(raw),
    }
}

fn parse_posix(raw: &str) -> Result<PolicyPath, PathPolicyError> {
    let path = Path::new(raw);
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::ParentDir => return Err(PathPolicyError::ParentTraversal),
            Component::Normal(value) => components.push(value.to_string_lossy().into_owned()),
            Component::RootDir | Component::CurDir => {}
            Component::Prefix(_) => {}
        }
    }
    Ok(PolicyPath {
        platform: Platform::Posix,
        root: if raw.starts_with('/') { "/" } else { "" }.to_owned(),
        components,
    })
}

fn parse_windows(raw: &str) -> Result<PolicyPath, PathPolicyError> {
    let normalized = raw.replace('/', "\\");
    if normalized.split('\\').any(|part| part == "..") {
        return Err(PathPolicyError::ParentTraversal);
    }
    let (root, tail) = if let Some(unc_path) = normalized.strip_prefix("\\\\") {
        let mut parts = unc_path.split('\\');
        let server = parts.next().unwrap_or_default();
        let share = parts.next().unwrap_or_default();
        if server.is_empty() || share.is_empty() {
            return Err(PathPolicyError::MissingWindowsRoot);
        }
        (
            format!("\\\\{server}\\{share}\\"),
            parts.collect::<Vec<_>>(),
        )
    } else if normalized.as_bytes().get(1) == Some(&b':')
        && normalized.as_bytes().get(2) == Some(&b'\\')
    {
        (
            normalized[..3.min(normalized.len())].to_owned(),
            normalized[3.min(normalized.len())..].split('\\').collect(),
        )
    } else {
        return Err(PathPolicyError::MissingWindowsRoot);
    };
    Ok(PolicyPath {
        platform: Platform::Windows,
        root,
        components: tail
            .into_iter()
            .filter(|component| !component.is_empty() && *component != ".")
            .map(str::to_owned)
            .collect(),
    })
}

/// A Git Bash path, read the same way on every host.
///
/// Git Bash accepts both separators and both spellings of a drive: a path
/// opening with a drive letter or a pair of separators is read as Windows
/// reads it (`C:\work`, `C:/work`, `\\server\share`), so a drive without a
/// root (`C:`, `C:work`) is refused rather than positioned. Anything else is
/// split on either separator, an absolute path whose first component is one
/// letter naming that drive (`/c/work` is `C:\work`), and a relative path
/// staying relative.
fn parse_git_bash(raw: &str) -> Result<PolicyPath, PathPolicyError> {
    let bytes = raw.as_bytes();
    let separator = |byte: Option<&u8>| matches!(byte, Some(b'/' | b'\\'));
    let drive = bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':');
    if drive || (separator(bytes.first()) && separator(bytes.get(1))) {
        return parse_windows(raw).map(|parsed| PolicyPath {
            platform: Platform::GitBash,
            ..parsed
        });
    }
    let mut components = Vec::new();
    for part in raw.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return Err(PathPolicyError::ParentTraversal),
            part => components.push(part.to_owned()),
        }
    }
    let root = if !separator(bytes.first()) {
        String::new()
    } else if components
        .first()
        .is_some_and(|first| first.len() == 1 && first.as_bytes()[0].is_ascii_alphabetic())
    {
        format!("{}:\\", components.remove(0).to_ascii_uppercase())
    } else {
        "/".to_owned()
    };
    Ok(PolicyPath {
        platform: Platform::GitBash,
        root,
        components,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_platform_fixtures_without_host_access() {
        assert_eq!(
            parse_policy_path(Platform::Posix, "/work/src/lib.rs")
                .expect("POSIX fixture")
                .components,
            ["work", "src", "lib.rs"]
        );
        assert_eq!(
            parse_policy_path(Platform::Windows, r"C:\work\src\lib.rs")
                .expect("Windows fixture")
                .root,
            r"C:\"
        );
        assert_eq!(
            parse_policy_path(Platform::GitBash, "/c/work/src/lib.rs")
                .expect("Git Bash fixture")
                .root,
            r"C:\"
        );
        assert_eq!(
            parse_policy_path(Platform::Windows, r"C:relative"),
            Err(PathPolicyError::MissingWindowsRoot)
        );
    }

    /// Git Bash reads both drive spellings and both separators the same way on
    /// every host, keeps a relative path relative, and refuses a drive
    /// without a root.
    #[test]
    fn a_git_bash_path_reads_every_windows_spelling() {
        let parsed = |raw: &str| parse_policy_path(Platform::GitBash, raw);
        for raw in [
            r"C:\work\notes",
            "C:/work/notes",
            "/c/work/notes",
            r"c:\work/notes",
        ] {
            let path = parsed(raw).expect(raw);
            assert!(path.root.eq_ignore_ascii_case(r"C:\"), "{raw}: {path:?}");
            assert_eq!(path.components, ["work", "notes"], "{raw}");
        }
        let relative = parsed(r"work\notes").expect("relative");
        assert_eq!(relative.root, "");
        assert_eq!(relative.components, ["work", "notes"]);
        assert_eq!(parsed("/etc/passwd").expect("rooted").root, "/");
        assert_eq!(
            parsed(r"\\server\share\notes").expect("UNC").root,
            r"\\server\share\"
        );
        for raw in ["C:", "C:notes.txt"] {
            assert_eq!(
                parsed(raw),
                Err(PathPolicyError::MissingWindowsRoot),
                "{raw}"
            );
        }
    }
}

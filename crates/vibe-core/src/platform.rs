use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;

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

fn parse_git_bash(raw: &str) -> Result<PolicyPath, PathPolicyError> {
    let parsed = parse_posix(raw)?;
    let mut components = parsed.components;
    let root = components
        .first()
        .filter(|drive| drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic())
        .map(|drive| format!("{}:\\", drive.to_ascii_uppercase()))
        .unwrap_or_else(|| "/".to_owned());
    if root.ends_with(":\\") && !components.is_empty() {
        components.remove(0);
    }
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
}

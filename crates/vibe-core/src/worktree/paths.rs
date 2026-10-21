//! Which paths a managed worktree may be named, live at, and be reached
//! through: the guards the reference runs before it trusts a directory with a
//! session (`vibe/core/git/worktree/repository.py:1127-1316`).

use std::path::{Component, Path, PathBuf};

use super::git::GitRepo;
use super::{ManagedRoot, WorktreeError, managed_bucket_name};
use crate::platform::resolve_lenient;
use crate::pycompat;

/// The characters a Windows path cannot carry, which is the set the reference
/// refuses a worktree name for (`vibe/core/git/worktree/repository.py:31`).
const INVALID_NAME_CHARACTERS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// The device names Windows reserves whatever the extension, upper-cased for
/// comparison (`vibe/core/git/worktree/repository.py:39-43`).
const RESERVED_DEVICE_NAMES: [&str; 25] = [
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "CLOCK$", "COM1", "COM2", "COM3", "COM4",
    "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
    "LPT8", "LPT9",
];

/// The managed root a checkout's worktrees live under.
///
/// Both the managed directory and the repository directory are resolved
/// before the second is checked against the first, so a repository directory
/// that resolves out of the managed root is refused rather than written to
/// (`vibe/core/git/worktree/repository.py:1159-1167`).
#[cfg(test)]
pub(super) fn managed_worktree_root(
    vibe_home: &Path,
    repo_root: &Path,
    common_git_dir: &Path,
) -> Result<PathBuf, WorktreeError> {
    managed_worktree_root_in(
        &ManagedRoot::for_vibe_home(vibe_home),
        repo_root,
        common_git_dir,
    )
}

pub(super) fn managed_worktree_root_in(
    managed: &ManagedRoot,
    repo_root: &Path,
    common_git_dir: &Path,
) -> Result<PathBuf, WorktreeError> {
    let bucket = managed_bucket_name(repo_root, common_git_dir);
    let target = resolve_lenient(&managed.path.join(bucket));
    if target.starts_with(&managed.path) {
        Ok(target)
    } else {
        Err(WorktreeError::ManagedRootEscape {
            target,
            managed_root: managed.path.clone(),
        })
    }
}

/// The directory a session runs in once the worktree exists.
///
/// Five questions, in the reference's order
/// (`vibe/core/git/worktree/repository.py:1283-1316`): the path resolves, it is
/// a directory, it lies inside the worktree once resolved, resolving it moved
/// nothing, and nothing between it and the worktree root carries a `.git`
/// entry. The fourth refuses a link below the root even when it lands inside
/// it: the directory becomes the session's position, its write root and a
/// trust grant, so a committed link must not choose it. The fifth keeps a
/// session out of a nested repository the worktree merely contains.
pub(super) fn target_cwd(
    root: &Path,
    relative_base: &Path,
    name: &str,
) -> Result<PathBuf, WorktreeError> {
    let root = resolve_lenient(root);
    let path = root.join(relative_base);
    let resolved = crate::platform::canonicalize(&path).map_err(|_| {
        WorktreeError::failed(
            name,
            format!(
                "worktree path `{}` does not exist after checkout",
                path.display()
            ),
        )
    })?;
    if !resolved.is_dir() {
        return Err(WorktreeError::failed(
            name,
            format!("worktree path `{}` is not a directory", path.display()),
        ));
    }
    if !resolved.starts_with(&root) {
        return Err(WorktreeError::failed(
            name,
            format!(
                "worktree path `{}` resolves outside worktree `{}`",
                path.display(),
                root.display()
            ),
        ));
    }
    if resolved != normalized(&path) {
        return Err(WorktreeError::failed(
            name,
            format!(
                "worktree path `{}` is reached through a symbolic link to `{}`",
                path.display(),
                resolved.display()
            ),
        ));
    }
    let mut current = resolved.clone();
    while current != root {
        let marker = current.join(".git");
        if marker.exists() || marker.is_symlink() {
            return Err(WorktreeError::failed(
                name,
                format!(
                    "worktree path `{}` belongs to a different git repository",
                    resolved.display()
                ),
            ));
        }
        let Some(parent) = current.parent().map(Path::to_path_buf) else {
            break;
        };
        current = parent;
    }
    Ok(resolved)
}

/// `path` with its `.` components dropped, which is how `Path` joins compare
/// in the reference: `root / "."` is `root` there.
fn normalized(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect()
}

/// Whether any component of `path` below the first one is a symbolic link.
///
/// The first component under the anchor is skipped: a root-level alias such as
/// macOS's `/tmp` belongs to the operating system rather than to the worktree
/// hierarchy (`vibe/core/git/worktree/repository.py:1268-1280`).
pub(super) fn has_linked_path_component(path: &Path) -> bool {
    let mut current = PathBuf::new();
    let mut anchored = false;
    let mut skipped_first = false;
    for component in path.components() {
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            anchored = true;
            current.push(component.as_os_str());
            continue;
        }
        current.push(component.as_os_str());
        if anchored && !skipped_first {
            skipped_first = true;
            continue;
        }
        if current.is_symlink() {
            return true;
        }
    }
    false
}

/// What adopting an existing directory has to prove: it is a stable path, a
/// worktree of this repository, on the expected branch
/// (`vibe/core/git/worktree/repository.py:1190-1235`).
pub(super) fn validate_existing_worktree(
    target: &Path,
    expected_branch: &str,
    expected_common_git_dir: &Path,
) -> Result<(), WorktreeError> {
    validate_listed_worktree(target)?;
    let name = target
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    let repo = GitRepo::at(target)?;
    let inspect = |error: WorktreeError| {
        WorktreeError::failed(
            &name,
            format!("failed to inspect `{}`: {error}", target.display()),
        )
    };
    let actual_common = repo.resolve_git_dir(
        &repo
            .stdout(["rev-parse", "--git-common-dir"], &name)
            .map_err(inspect)?,
    )?;
    if actual_common != expected_common_git_dir {
        return Err(WorktreeError::failed(
            &name,
            format!(
                "path `{}` belongs to a different git repository",
                target.display()
            ),
        ));
    }
    let actual_branch = repo
        .stdout(["rev-parse", "--abbrev-ref", "HEAD"], &name)
        .map_err(inspect)?;
    if actual_branch == "HEAD" || actual_branch != expected_branch {
        let actual = if actual_branch == "HEAD" {
            "detached HEAD"
        } else {
            actual_branch.as_str()
        };
        return Err(WorktreeError::failed(
            &name,
            format!(
                "path `{}` is checked out on `{actual}`, expected `{expected_branch}`",
                target.display()
            ),
        ));
    }
    Ok(())
}

/// The part of [`validate_existing_worktree`] a listing still needs: git
/// already answered which repository and branch each record belongs to, and
/// what it cannot know is a path removed or swapped for a link since it wrote
/// the record (`vibe/core/git/worktree/repository.py:1238-1265`).
pub(super) fn validate_listed_worktree(target: &Path) -> Result<(), WorktreeError> {
    let name = target
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    if has_linked_path_component(target) {
        return Err(WorktreeError::failed(
            &name,
            format!(
                "path `{}` crosses a symbolic link, which is not a stable git worktree path",
                target.display()
            ),
        ));
    }
    if !target.join(".git").is_file() {
        return Err(WorktreeError::failed(
            &name,
            format!(
                "path `{}` already exists but is not a git worktree",
                target.display()
            ),
        ));
    }
    Ok(())
}

/// Refuses a name no filesystem this port runs on can carry as one directory
/// (`vibe/core/git/worktree/repository.py:1127-1150`).
pub(super) fn validate_worktree_name(name: &str) -> Result<(), WorktreeError> {
    if is_portable_worktree_name(name) {
        Ok(())
    } else {
        Err(WorktreeError::InvalidName)
    }
}

/// Not empty and not a relative alias, no trailing space or dot, printable,
/// none of the characters a Windows path forbids, no reserved device name
/// whatever follows the first dot, and one path segment.
#[must_use]
pub fn is_portable_worktree_name(name: &str) -> bool {
    if name.is_empty() || matches!(name, "." | "..") {
        return false;
    }
    if name.ends_with(' ') || name.ends_with('.') || !name.chars().all(pycompat::isprintable) {
        return false;
    }
    if name.contains(INVALID_NAME_CHARACTERS) {
        return false;
    }
    let device = name.split('.').next().unwrap_or(name).to_uppercase();
    if RESERVED_DEVICE_NAMES.contains(&device.as_str()) {
        return false;
    }
    Path::new(name).components().count() == 1
}

/// Asks git whether `branch` is a name it would accept for a ref.
///
/// `check-ref-format` reads no repository, so the directory only decides where
/// the process starts.
#[cfg(test)]
pub(super) fn validate_branch_name(directory: &Path, branch: &str) -> Result<(), WorktreeError> {
    GitRepo::at(directory)?.validate_branch(branch)
}

pub(super) fn path_text(path: &Path) -> Result<&str, WorktreeError> {
    path.to_str().ok_or_else(|| {
        WorktreeError::failed(
            "path",
            format!("path `{}` is not valid UTF-8", path.display()),
        )
    })
}

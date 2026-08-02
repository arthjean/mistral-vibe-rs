use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

use crate::Arguments;

use super::{StartupError, startup_io, vibe_home_directory};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedWorktree {
    pub name: String,
    pub branch: String,
    pub root: PathBuf,
    pub path: PathBuf,
    pub repo_root: PathBuf,
    pub base_commit: String,
    pub created: bool,
    pub branch_created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCleanupState {
    pub has_uncommitted_changes: bool,
    pub has_untracked_files: bool,
    pub new_commit_count: u64,
}

impl WorktreeCleanupState {
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        !self.has_uncommitted_changes && !self.has_untracked_files && self.new_commit_count == 0
    }

    #[must_use]
    pub fn reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        if self.has_uncommitted_changes {
            reasons.push("uncommitted changes".to_owned());
        }
        if self.has_untracked_files {
            reasons.push("untracked files".to_owned());
        }
        if self.new_commit_count > 0 {
            let noun = if self.new_commit_count == 1 {
                "commit"
            } else {
                "commits"
            };
            reasons.push(format!(
                "{} {noun} added to the branch during this session",
                self.new_commit_count
            ));
        }
        reasons
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchWorkspace {
    pub effective_directory: PathBuf,
    pub worktree: Option<PreparedWorktree>,
}

impl LaunchWorkspace {
    pub fn prepare(arguments: &mut Arguments) -> Result<Self, StartupError> {
        let requested = match &arguments.workdir {
            Some(path) => path.clone(),
            None => std::env::current_dir().map_err(|source| StartupError::Io {
                path: PathBuf::from("."),
                source,
            })?,
        };
        let base_directory = canonical_directory(&expand_user_path(&requested))?;

        if arguments.worktree.is_none() || arguments.setup || arguments.check_upgrade {
            resolve_additional_directories(arguments, &base_directory)?;
            arguments.workdir = Some(base_directory.clone());
            return Ok(Self {
                effective_directory: base_directory,
                worktree: None,
            });
        }

        let name = arguments.worktree.as_deref().unwrap_or_default();
        let worktree = prepare_worktree(name, &base_directory, arguments)?;
        resolve_additional_directories(arguments, &worktree.path)?;
        arguments.workdir = Some(worktree.path.clone());
        arguments.trust = true;
        Ok(Self {
            effective_directory: worktree.path.clone(),
            worktree: Some(worktree),
        })
    }
}

fn resolve_additional_directories(
    arguments: &mut Arguments,
    effective_directory: &Path,
) -> Result<(), StartupError> {
    for directory in &mut arguments.add_directories {
        let expanded = expand_user_path(directory);
        let candidate = if expanded.is_absolute() {
            expanded
        } else {
            effective_directory.join(expanded)
        };
        *directory = canonical_directory(&candidate)?;
    }
    Ok(())
}

fn expand_user_path(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    if text == "~" {
        return std::env::var_os("HOME").map_or_else(|| path.to_path_buf(), PathBuf::from);
    }
    for prefix in ["~/", "~\\"] {
        if let Some(remainder) = text.strip_prefix(prefix)
            && let Some(home) = std::env::var_os("HOME")
        {
            return PathBuf::from(home).join(remainder);
        }
    }
    path.to_path_buf()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupOutcome {
    NotOwned,
    Kept,
    Removed,
}

impl PreparedWorktree {
    pub fn cleanup_terminal(self) -> Result<CleanupOutcome, StartupError> {
        let mut output = std::io::stderr().lock();
        if std::io::stdin().is_terminal() {
            return self.cleanup(&mut std::io::stdin().lock(), &mut output);
        }
        #[cfg(unix)]
        {
            let terminal = fs::File::open("/dev/tty")
                .map_err(|source| startup_io(Path::new("/dev/tty"), source))?;
            self.cleanup(&mut std::io::BufReader::new(terminal), &mut output)
        }
        #[cfg(not(unix))]
        self.cleanup(&mut std::io::stdin().lock(), &mut output)
    }

    pub fn cleanup(
        self,
        input: &mut impl BufRead,
        output: &mut impl Write,
    ) -> Result<CleanupOutcome, StartupError> {
        if !self.created {
            return Ok(CleanupOutcome::NotOwned);
        }
        let state = inspect_worktree_for_cleanup(&self)?;
        if !state.is_clean() {
            writeln!(
                output,
                "Worktree {:?} has {}.",
                self.name,
                state.reasons().join(", ")
            )
            .map_err(|source| startup_io(&self.root, source))?;
            writeln!(
                output,
                "Remove it and delete its branch? This discards worktree changes, untracked files, and commits."
            )
            .map_err(|source| startup_io(&self.root, source))?;
            if !confirm(
                input,
                output,
                "Remove worktree? [y/N] ",
                &["y", "yes", "remove"],
            )? {
                writeln!(output, "Keeping worktree: {}", self.root.display())
                    .map_err(|source| startup_io(&self.root, source))?;
                return Ok(CleanupOutcome::Kept);
            }
        }

        let delete_branch = if self.branch_created {
            true
        } else {
            writeln!(
                output,
                "Branch {:?} existed before this session and was attached, not created by Vibe.",
                self.branch
            )
            .map_err(|source| startup_io(&self.root, source))?;
            confirm(
                input,
                output,
                &format!("Also delete branch {:?}? [y/N] ", self.branch),
                &["y", "yes", "delete"],
            )?
        };

        git_checked(
            &self.repo_root,
            ["worktree", "remove", "--force", path_text(&self.root)?],
            &self.name,
        )?;
        if delete_branch {
            git_checked(
                &self.repo_root,
                ["branch", "-D", self.branch.as_str()],
                &self.name,
            )?;
        }
        writeln!(output, "Removed worktree: {}", self.root.display())
            .map_err(|source| startup_io(&self.root, source))?;
        if !delete_branch {
            writeln!(output, "Kept branch: {}", self.branch)
                .map_err(|source| startup_io(&self.root, source))?;
        }
        Ok(CleanupOutcome::Removed)
    }
}

fn prepare_worktree(
    name: &str,
    base: &Path,
    arguments: &Arguments,
) -> Result<PreparedWorktree, StartupError> {
    validate_worktree_name(name)?;
    let checkout_root = git_stdout(base, ["rev-parse", "--show-toplevel"], name)
        .map(PathBuf::from)
        .map_err(|_| StartupError::WorktreeRepositoryRequired)?;
    let checkout_root = canonical_directory(&checkout_root)?;
    let common_git_dir = resolve_git_path(
        &checkout_root,
        &git_stdout(base, ["rev-parse", "--git-common-dir"], name)?,
    )?;
    let repo_root = common_git_dir
        .parent()
        .ok_or_else(|| StartupError::Worktree {
            name: name.to_owned(),
            message: "git common directory has no repository parent".to_owned(),
        })?
        .to_path_buf();
    let relative_base = base
        .strip_prefix(&checkout_root)
        .map_err(|_| StartupError::Worktree {
            name: name.to_owned(),
            message: "working directory is outside the Git checkout".to_owned(),
        })?;
    let digest = hex::encode(Sha256::digest(common_git_dir.to_string_lossy().as_bytes()));
    let repository_name = repo_root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("repository");
    let target = vibe_home_directory(arguments, base)
        .join("worktrees")
        .join(format!("{repository_name}-{}", &digest[..12]))
        .join(name);

    if target.is_dir() {
        validate_existing_worktree(&target, name, &common_git_dir)?;
        return build_prepared_worktree(name, target, relative_base, repo_root, false, false);
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|source| startup_io(parent, source))?;
    }
    let branch_exists = git_status(
        &checkout_root,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
        name,
    )?;
    let target_text = path_text(&target)?;
    if branch_exists {
        git_checked(&checkout_root, ["worktree", "add", target_text, name], name)?;
    } else {
        git_checked(
            &checkout_root,
            ["worktree", "add", "-b", name, target_text],
            name,
        )?;
    }
    build_prepared_worktree(name, target, relative_base, repo_root, true, !branch_exists)
}

fn build_prepared_worktree(
    name: &str,
    root: PathBuf,
    relative_base: &Path,
    repo_root: PathBuf,
    created: bool,
    branch_created: bool,
) -> Result<PreparedWorktree, StartupError> {
    let path = root.join(relative_base);
    if !path.is_dir() {
        return Err(StartupError::Worktree {
            name: name.to_owned(),
            message: format!(
                "worktree path `{}` does not exist after checkout",
                path.display()
            ),
        });
    }
    let base_commit = git_stdout(&root, ["rev-parse", "HEAD"], name)?;
    Ok(PreparedWorktree {
        name: name.to_owned(),
        branch: name.to_owned(),
        root,
        path,
        repo_root,
        base_commit,
        created,
        branch_created,
    })
}

fn validate_existing_worktree(
    target: &Path,
    expected_branch: &str,
    expected_common_git_dir: &Path,
) -> Result<(), StartupError> {
    if !target.join(".git").is_file() {
        return Err(StartupError::Worktree {
            name: expected_branch.to_owned(),
            message: format!(
                "path `{}` already exists but is not a Git worktree",
                target.display()
            ),
        });
    }
    let actual_common = resolve_git_path(
        target,
        &git_stdout(target, ["rev-parse", "--git-common-dir"], expected_branch)?,
    )?;
    if actual_common != expected_common_git_dir {
        return Err(StartupError::Worktree {
            name: expected_branch.to_owned(),
            message: format!(
                "path `{}` belongs to a different Git repository",
                target.display()
            ),
        });
    }
    let actual_branch = git_stdout(target, ["branch", "--show-current"], expected_branch)?;
    if actual_branch != expected_branch {
        let actual = if actual_branch.is_empty() {
            "detached HEAD"
        } else {
            actual_branch.as_str()
        };
        return Err(StartupError::Worktree {
            name: expected_branch.to_owned(),
            message: format!(
                "path `{}` is checked out on `{actual}`, expected `{expected_branch}`",
                target.display()
            ),
        });
    }
    Ok(())
}

fn inspect_worktree_for_cleanup(
    worktree: &PreparedWorktree,
) -> Result<WorktreeCleanupState, StartupError> {
    let status = git_stdout(
        &worktree.root,
        ["status", "--porcelain", "--untracked-files=all"],
        &worktree.name,
    )?;
    let new_commit_count = git_stdout(
        &worktree.root,
        [
            "rev-list",
            "--count",
            &format!("{}..{}", worktree.base_commit, worktree.branch),
        ],
        &worktree.name,
    )?
    .parse::<u64>()
    .map_err(|error| StartupError::Worktree {
        name: worktree.name.clone(),
        message: format!("invalid commit count: {error}"),
    })?;
    Ok(WorktreeCleanupState {
        has_uncommitted_changes: status.lines().any(|line| !line.starts_with("??")),
        has_untracked_files: status.lines().any(|line| line.starts_with("??")),
        new_commit_count,
    })
}

fn validate_worktree_name(name: &str) -> Result<(), StartupError> {
    let is_single_segment = !name.is_empty()
        && !matches!(name, "." | "..")
        && Path::new(name).components().count() == 1
        && !name.contains(['/', '\\'])
        && !name
            .as_bytes()
            .get(1)
            .is_some_and(|character| *character == b':');
    if is_single_segment {
        Ok(())
    } else {
        Err(StartupError::InvalidWorktreeName)
    }
}

fn resolve_git_path(base: &Path, value: &str) -> Result<PathBuf, StartupError> {
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    fs::canonicalize(&path).map_err(|source| startup_io(&path, source))
}

fn git_stdout<'a>(
    directory: &Path,
    arguments: impl IntoIterator<Item = &'a str>,
    name: &str,
) -> Result<String, StartupError> {
    let output = git_output(directory, arguments, name)?;
    if !output.status.success() {
        return Err(git_failure(name, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_checked<'a>(
    directory: &Path,
    arguments: impl IntoIterator<Item = &'a str>,
    name: &str,
) -> Result<(), StartupError> {
    let output = git_output(directory, arguments, name)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(name, &output))
    }
}

fn git_status<'a>(
    directory: &Path,
    arguments: impl IntoIterator<Item = &'a str>,
    name: &str,
) -> Result<bool, StartupError> {
    git_output(directory, arguments, name).map(|output| output.status.success())
}

fn git_output<'a>(
    directory: &Path,
    arguments: impl IntoIterator<Item = &'a str>,
    name: &str,
) -> Result<Output, StartupError> {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .map_err(|error| StartupError::Worktree {
            name: name.to_owned(),
            message: error.to_string(),
        })
}

fn git_failure(name: &str, output: &Output) -> StartupError {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    StartupError::Worktree {
        name: name.to_owned(),
        message: if message.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            message
        },
    }
}

fn confirm(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: &str,
    accepted: &[&str],
) -> Result<bool, StartupError> {
    write!(output, "{prompt}").map_err(|source| startup_io(Path::new("stderr"), source))?;
    output
        .flush()
        .map_err(|source| startup_io(Path::new("stderr"), source))?;
    let mut answer = String::new();
    let read = input
        .read_line(&mut answer)
        .map_err(|source| startup_io(Path::new("stdin"), source))?;
    Ok(read > 0 && accepted.contains(&answer.trim().to_ascii_lowercase().as_str()))
}

fn canonical_directory(path: &Path) -> Result<PathBuf, StartupError> {
    let canonical = fs::canonicalize(path).map_err(|source| startup_io(path, source))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(StartupError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "path is not a directory",
            ),
        })
    }
}

fn path_text(path: &Path) -> Result<&str, StartupError> {
    path.to_str().ok_or_else(|| StartupError::Worktree {
        name: "path".to_owned(),
        message: format!("path `{}` is not valid UTF-8", path.display()),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn test_arguments(root: &Path) -> Arguments {
        let mut arguments = crate::arguments_for_test();
        arguments.workdir = Some(root.to_path_buf());
        arguments.session_root = Some(root.join("vibe-home/sessions"));
        arguments
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {:?} failed", arguments);
    }

    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("repository");
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["config", "user.name", "Vibe Test"]);
        git(root.path(), &["config", "user.email", "vibe@example.test"]);
        fs::write(root.path().join("README.md"), "fixture\n").expect("fixture");
        git(root.path(), &["add", "README.md"]);
        git(root.path(), &["commit", "-qm", "fixture"]);
        root
    }

    #[test]
    fn worktree_is_prepared_at_the_reference_location_and_reused() {
        let root = repository();
        let mut arguments = test_arguments(root.path());
        arguments.worktree = Some("parity".to_owned());
        let first = LaunchWorkspace::prepare(&mut arguments).expect("worktree prepared");
        let prepared = first.worktree.expect("prepared worktree");
        assert!(prepared.created);
        assert!(prepared.path.join("README.md").is_file());
        assert_eq!(arguments.workdir.as_deref(), Some(prepared.path.as_path()));
        assert!(arguments.trust);

        let mut reused_arguments = test_arguments(root.path());
        reused_arguments.worktree = Some("parity".to_owned());
        let reused = LaunchWorkspace::prepare(&mut reused_arguments)
            .expect("existing worktree reused")
            .worktree
            .expect("reused worktree");
        assert!(!reused.created);
        assert_eq!(reused.path, prepared.path);

        git(
            root.path(),
            &[
                "worktree",
                "remove",
                "--force",
                path_text(&prepared.root).expect("path"),
            ],
        );
        git(root.path(), &["branch", "-D", "parity"]);
    }

    #[test]
    fn invalid_and_non_repository_worktrees_fail_before_checkout_changes() {
        let root = repository();
        let original_head = git_stdout(root.path(), ["rev-parse", "HEAD"], "head").expect("head");
        let mut invalid = test_arguments(root.path());
        invalid.worktree = Some("../escape".to_owned());
        assert!(matches!(
            LaunchWorkspace::prepare(&mut invalid),
            Err(StartupError::InvalidWorktreeName)
        ));
        assert_eq!(
            git_stdout(root.path(), ["rev-parse", "HEAD"], "head").expect("head"),
            original_head
        );

        let outside = tempfile::tempdir().expect("non-repository");
        let mut non_repository = test_arguments(outside.path());
        non_repository.worktree = Some("feature".to_owned());
        assert!(matches!(
            LaunchWorkspace::prepare(&mut non_repository),
            Err(StartupError::WorktreeRepositoryRequired)
        ));
    }

    #[test]
    fn conflicting_worktree_path_is_preserved() {
        let root = repository();
        let mut arguments = test_arguments(root.path());
        arguments.worktree = Some("conflict".to_owned());
        let prepared = LaunchWorkspace::prepare(&mut arguments)
            .expect("initial worktree")
            .worktree
            .expect("prepared worktree");
        let target = prepared.root.clone();
        prepared
            .cleanup(&mut Cursor::new(Vec::<u8>::new()), &mut Vec::new())
            .expect("initial worktree removed");
        fs::create_dir_all(&target).expect("conflicting directory");
        fs::write(target.join("owner.txt"), "preserve me\n").expect("conflicting content");

        let mut conflicting = test_arguments(root.path());
        conflicting.worktree = Some("conflict".to_owned());
        assert!(matches!(
            LaunchWorkspace::prepare(&mut conflicting),
            Err(StartupError::Worktree { .. })
        ));
        assert_eq!(
            fs::read_to_string(target.join("owner.txt")).expect("conflict preserved"),
            "preserve me\n"
        );
    }

    #[test]
    fn attached_preexisting_branch_can_survive_cleanup() {
        let root = repository();
        git(root.path(), &["branch", "attached"]);
        let mut arguments = test_arguments(root.path());
        arguments.worktree = Some("attached".to_owned());
        let prepared = LaunchWorkspace::prepare(&mut arguments)
            .expect("attached branch worktree")
            .worktree
            .expect("prepared worktree");
        let worktree_root = prepared.root.clone();
        let outcome = prepared
            .cleanup(&mut Cursor::new(b"n\n"), &mut Vec::new())
            .expect("cleanup choice");
        assert_eq!(outcome, CleanupOutcome::Removed);
        assert!(!worktree_root.exists());
        assert!(
            git_status(
                root.path(),
                ["show-ref", "--verify", "--quiet", "refs/heads/attached"],
                "attached"
            )
            .expect("branch status")
        );
        git(root.path(), &["branch", "-D", "attached"]);
    }

    #[test]
    fn relative_additional_directory_is_resolved_inside_the_worktree() {
        let root = repository();
        fs::create_dir_all(root.path().join("nested/context")).expect("nested directory");
        fs::write(root.path().join("nested/context/file.txt"), "context\n").expect("context file");
        git(root.path(), &["add", "nested/context/file.txt"]);
        git(root.path(), &["commit", "-qm", "nested fixture"]);
        let mut arguments = test_arguments(root.path());
        arguments.worktree = Some("relative-add-dir".to_owned());
        arguments.add_directories = vec![PathBuf::from("nested/context")];
        let prepared = LaunchWorkspace::prepare(&mut arguments)
            .expect("worktree prepared")
            .worktree
            .expect("prepared worktree");
        assert_eq!(
            arguments.add_directories,
            vec![prepared.path.join("nested/context")]
        );
        prepared
            .cleanup(&mut Cursor::new(Vec::<u8>::new()), &mut Vec::new())
            .expect("clean worktree removed");
    }

    #[test]
    fn dirty_owned_worktree_is_kept_when_cleanup_is_declined() {
        let root = repository();
        let mut arguments = test_arguments(root.path());
        arguments.worktree = Some("dirty".to_owned());
        let prepared = LaunchWorkspace::prepare(&mut arguments)
            .expect("worktree prepared")
            .worktree
            .expect("prepared worktree");
        fs::write(prepared.path.join("dirty.txt"), "dirty\n").expect("dirty file");
        let worktree_root = prepared.root.clone();
        let outcome = prepared
            .cleanup(&mut Cursor::new(b"n\n"), &mut Vec::new())
            .expect("cleanup decision");
        assert_eq!(outcome, CleanupOutcome::Kept);
        assert!(worktree_root.is_dir());
        git(
            root.path(),
            &[
                "worktree",
                "remove",
                "--force",
                path_text(&worktree_root).expect("path"),
            ],
        );
        git(root.path(), &["branch", "-D", "dirty"]);
    }

    #[test]
    fn clean_owned_worktree_is_removed_once() {
        let root = repository();
        let mut arguments = test_arguments(root.path());
        arguments.worktree = Some("clean".to_owned());
        let prepared = LaunchWorkspace::prepare(&mut arguments)
            .expect("worktree prepared")
            .worktree
            .expect("prepared worktree");
        let path = prepared.root.clone();
        let outcome = prepared
            .cleanup(&mut Cursor::new(Vec::<u8>::new()), &mut Vec::new())
            .expect("clean worktree removed");
        assert_eq!(outcome, CleanupOutcome::Removed);
        assert!(!path.exists());
    }
}

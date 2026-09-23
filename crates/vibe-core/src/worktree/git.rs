//! The git primitives managed worktrees are built on.
//!
//! The reference routes every call through one class, `GitRepo`
//! (`vibe/core/git/repo.py`), so that GitPython is spoken in one place and its
//! failures become the `GitError` hierarchy of `vibe/core/git/errors.py`. This
//! is that class for a port that drives the `git` binary directly: one opened
//! checkout, the questions the worktree layer asks it, and the refusals mapped
//! onto [`WorktreeError`]'s git-level variants.
//!
//! Two protections are applied to every command rather than to a chosen few.
//! The executable is resolved from absolute `PATH` entries outside the
//! checkout, as `resolve_git_executable` does (`vibe/utils/platform.py:89-149`),
//! so a repository cannot ship the `git` its own worktree operations run. And
//! every invocation carries `-c core.fsmonitor=` and a `core.hooksPath` that
//! names no directory, the two keys a repository can use to make git run a
//! command of its choosing (`vibe/core/git/repo.py:39-48,122-134`). The
//! reference applies the pair to the calls that write refs or read a working
//! tree; applying it to the read-only ones as well changes no answer they give.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::WorktreeError;

/// What `core.hooksPath` is pointed at so a repository's hooks never run.
///
/// Absolute and absent rather than empty: git resolves a relative hooks path
/// against the directory the hooks would run in, which is the untrusted
/// worktree itself (`vibe/core/git/repo.py:39-48`).
pub(crate) const NO_HOOKS_PATH: &str = "/nonexistent-vibe-disabled-git-hooks";

/// The status git exits with when it refuses an option it does not know.
///
/// `git worktree list` learned `-z` in 2.36, and an older git answers this
/// instead of a listing (`vibe/core/git/repo.py:25,494-505`).
pub(crate) const GIT_USAGE_ERROR_STATUS: i32 = 129;

/// The remote whose default branch a new worktree branch starts from.
pub(crate) const DEFAULT_REMOTE: &str = "origin";

/// The environment variable the reference reads an explicitly trusted git from.
///
/// It is GitPython's own name, and the reference treats a process-level value
/// as a trust decision that may point at a portable installation
/// (`vibe/utils/platform.py:134-149`).
const GIT_EXECUTABLE_ENV: &str = "GIT_PYTHON_GIT_EXECUTABLE";

/// The branches a repository that never set `origin/HEAD` most likely treats
/// as its trunk, in the order the reference tries them
/// (`vibe/core/git/repo.py:49-51`).
const CONVENTIONAL_BASE_BRANCHES: [&str; 3] = ["main", "master", "develop"];

/// The sentence both unavailable-git refusals carry, which the launch reports.
const GIT_UNAVAILABLE: &str =
    "no trusted git executable was found; install git or set GIT_PYTHON_GIT_EXECUTABLE";

/// A trusted `git` for work that happens around `cwd`.
///
/// A process-level `GIT_PYTHON_GIT_EXECUTABLE` is honored when it names an
/// absolute executable, and ignored otherwise, as the reference ignores a
/// relative one. Discovery walks the absolute `PATH` entries only, and refuses
/// a candidate inside `cwd`, so neither a relative entry nor a binary the
/// checkout carries can stand in for git.
pub(crate) fn git_executable(cwd: &Path) -> Result<PathBuf, WorktreeError> {
    if let Some(configured) = std::env::var_os(GIT_EXECUTABLE_ENV).filter(|value| !value.is_empty())
    {
        let configured = PathBuf::from(configured);
        return configured
            .is_absolute()
            .then(|| resolved_executable(&configured))
            .flatten()
            .ok_or_else(|| WorktreeError::GitUnavailable(GIT_UNAVAILABLE.to_owned()));
    }
    search_trusted_path("git", cwd)
        .ok_or_else(|| WorktreeError::GitUnavailable(GIT_UNAVAILABLE.to_owned()))
}

/// The first executable called `name` on an absolute `PATH` entry that does not
/// live in the project at `cwd`.
///
/// Reference `_search_trusted_path` (`vibe/utils/platform.py:89-101`). A root or
/// home directory as `cwd` is too broad to disqualify everything below it, so
/// only a binary directly inside it is refused there, which is the reference's
/// `_is_untrusted_project_executable`.
pub(crate) fn search_trusted_path(name: &str, cwd: &Path) -> Option<PathBuf> {
    let project = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let path = std::env::var_os("PATH").unwrap_or_default();
    for entry in std::env::split_paths(&path) {
        let entry = strip_quotes(entry);
        if !entry.is_absolute() {
            continue;
        }
        for executable in executable_names(name) {
            let Some(candidate) = resolved_executable(&entry.join(&executable)) else {
                continue;
            };
            if !is_untrusted_project_executable(&candidate, &project) {
                return Some(candidate);
            }
        }
    }
    None
}

fn strip_quotes(entry: PathBuf) -> PathBuf {
    let text = entry.to_string_lossy();
    match text
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        Some(inner) => PathBuf::from(inner),
        None => entry,
    }
}

fn executable_names(name: &str) -> Vec<OsString> {
    if cfg!(windows) && Path::new(name).extension().is_none() {
        let extensions =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
        extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| OsString::from(format!("{name}{}", extension.to_lowercase())))
            .collect()
    } else {
        vec![OsString::from(name)]
    }
}

fn resolved_executable(candidate: &Path) -> Option<PathBuf> {
    let resolved = fs::canonicalize(candidate).ok()?;
    let metadata = fs::metadata(&resolved).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    Some(resolved)
}

fn is_untrusted_project_executable(candidate: &Path, project: &Path) -> bool {
    if !candidate.starts_with(project) {
        return false;
    }
    let overbroad = project.parent().is_none()
        || std::env::home_dir()
            .and_then(|home| fs::canonicalize(home).ok())
            .is_some_and(|home| home == project);
    if overbroad {
        candidate.parent() == Some(project)
    } else {
        true
    }
}

/// Runs git in `directory` with the repository's command hooks disabled.
///
/// `name` is what a spawn failure other than a missing binary is blamed on.
pub(crate) fn output<'a>(
    executable: &Path,
    directory: &Path,
    arguments: impl IntoIterator<Item = &'a str>,
    name: &str,
) -> Result<Output, WorktreeError> {
    command(executable, directory)
        .args(arguments)
        .output()
        .map_err(|error| spawn_failure(name, &error))
}

/// The command every worktree call starts from.
pub(crate) fn command(executable: &Path, directory: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .arg("-C")
        .arg(directory)
        .args(["-c", "core.fsmonitor="])
        .arg("-c")
        .arg(format!("core.hooksPath={NO_HOOKS_PATH}"))
        // Git must never stop to ask: a remote needing credentials would
        // otherwise open a helper and block (`vibe/core/git/repo.py:30-34`).
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never");
    command
}

pub(crate) fn spawn_failure(name: &str, error: &std::io::Error) -> WorktreeError {
    if error.kind() == std::io::ErrorKind::NotFound {
        WorktreeError::GitUnavailable(error.to_string())
    } else {
        WorktreeError::failed(name, error.to_string())
    }
}

/// What git said about a refusal: its own stderr, or its exit status when it
/// said nothing.
pub(crate) fn message(output: &Output) -> String {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if message.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        message
    }
}

/// One checkout, opened once, with the layout questions answered lazily.
///
/// Reference `GitRepo` (`vibe/core/git/repo.py:137-512`). The working
/// directory is the checkout the base sits in, which for a base inside a
/// linked worktree is that worktree rather than the primary checkout.
#[derive(Debug, Clone)]
pub(crate) struct GitRepo {
    executable: PathBuf,
    working_dir: PathBuf,
}

/// Where a repository's shared data and its primary checkout are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoPaths {
    pub(crate) repo_root: PathBuf,
    pub(crate) common_git_dir: PathBuf,
}

/// One entry of a porcelain worktree listing, reduced to what the worktree
/// layer reads (`vibe/core/git/repo.py:514-548`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeListing {
    pub(crate) root: PathBuf,
    pub(crate) branch: Option<String>,
    pub(crate) prunable: bool,
}

/// Committed lines a branch added and removed against its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchChanges {
    pub additions: u64,
    pub deletions: u64,
}

impl GitRepo {
    /// Opens the checkout `base` sits in.
    ///
    /// A directory in no repository is
    /// [`WorktreeError::RepositoryRequired`], which is the one refusal every
    /// caller acts on, and a machine without a trusted git keeps its own
    /// answer (`vibe/core/git/repo.py:143-152`).
    pub(crate) fn open(base: &Path) -> Result<Self, WorktreeError> {
        let executable = git_executable(base)?;
        if !base.is_dir() {
            return Err(WorktreeError::RepositoryRequired);
        }
        let output = output(&executable, base, ["rev-parse", "--show-toplevel"], "open")?;
        if !output.status.success() {
            return Err(WorktreeError::RepositoryRequired);
        }
        let reported = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if reported.is_empty() {
            // A bare repository, or the inside of a git directory: there is no
            // checkout to work in, which the reference refuses the same way.
            return Err(WorktreeError::RepositoryRequired);
        }
        let working_dir =
            fs::canonicalize(&reported).map_err(|_| WorktreeError::RepositoryRequired)?;
        Ok(Self {
            executable,
            working_dir,
        })
    }

    /// Opens a worktree in its own right rather than through the repository it
    /// is linked to, so what is read is that worktree's own state.
    pub(crate) fn at(target: &Path) -> Result<Self, WorktreeError> {
        let executable = git_executable(target)?;
        Ok(Self {
            executable,
            working_dir: target.to_path_buf(),
        })
    }

    pub(crate) fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    pub(crate) fn executable(&self) -> &Path {
        &self.executable
    }

    pub(crate) fn output<'a>(
        &self,
        arguments: impl IntoIterator<Item = &'a str>,
        name: &str,
    ) -> Result<Output, WorktreeError> {
        output(&self.executable, &self.working_dir, arguments, name)
    }

    /// Git's standard output for a command that has to succeed, trimmed, or a
    /// git-level failure carrying what git said.
    pub(crate) fn stdout<'a>(
        &self,
        arguments: impl IntoIterator<Item = &'a str>,
        name: &str,
    ) -> Result<String, WorktreeError> {
        let output = self.output(arguments, name)?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            Err(WorktreeError::git(message(&output)))
        }
    }

    pub(crate) fn checked<'a>(
        &self,
        arguments: impl IntoIterator<Item = &'a str>,
        name: &str,
    ) -> Result<(), WorktreeError> {
        self.stdout(arguments, name).map(|_| ())
    }

    /// The repository's shared git directory and its primary checkout.
    ///
    /// Both rev-parse answers are read from the working directory rather than
    /// from wherever the caller started, because git reports them relative to
    /// the directory it ran in (`vibe/core/git/repo.py:348-377`).
    pub(crate) fn paths(&self) -> Result<RepoPaths, WorktreeError> {
        let common_git_dir =
            self.resolve_git_dir(&self.stdout(["rev-parse", "--git-common-dir"], "paths")?)?;
        let git_dir = self.resolve_git_dir(&self.stdout(["rev-parse", "--git-dir"], "paths")?)?;
        let repo_root = if git_dir == common_git_dir {
            self.working_dir.clone()
        } else if common_git_dir
            .file_name()
            .is_some_and(|value| value == ".git")
        {
            common_git_dir
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| WorktreeError::git("the git common directory has no parent"))?
        } else {
            return Err(WorktreeError::git(
                "the primary checkout cannot be determined from a linked worktree of a \
                 repository using a separate git directory",
            ));
        };
        Ok(RepoPaths {
            repo_root,
            common_git_dir,
        })
    }

    /// A git directory as git reported it, resolved against the working
    /// directory when it is relative (`vibe/core/git/repo.py:361-365`).
    pub(crate) fn resolve_git_dir(&self, value: &str) -> Result<PathBuf, WorktreeError> {
        let path = PathBuf::from(value);
        let path = if path.is_absolute() {
            path
        } else {
            self.working_dir.join(path)
        };
        fs::canonicalize(&path).map_err(|source| WorktreeError::Io { path, source })
    }

    /// Where `base` sits inside this checkout.
    ///
    /// The base is resolved before it is measured, so a directory reached
    /// through a symbolic link is placed by where it actually is. One that
    /// resolves out of the checkout is a git-level refusal naming both paths
    /// (`vibe/core/git/repo.py:379-386`).
    pub(crate) fn relative_base(&self, base: &Path) -> Result<PathBuf, WorktreeError> {
        let resolved = super::resolve_lenient(base);
        resolved
            .strip_prefix(&self.working_dir)
            .map(Path::to_path_buf)
            .map_err(|_| {
                WorktreeError::git(format!(
                    "path `{}` is outside git repository `{}`",
                    resolved.display(),
                    self.working_dir.display()
                ))
            })
    }

    /// Refuses a branch git would not accept as a ref name
    /// (`vibe/core/git/repo.py:388-392`).
    pub(crate) fn validate_branch(&self, branch: &str) -> Result<(), WorktreeError> {
        let output = self.output(["check-ref-format", "--branch", branch], branch)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(WorktreeError::InvalidBranch {
                branch: branch.to_owned(),
            })
        }
    }

    /// Whether `branch` is a local branch.
    ///
    /// `show-ref --verify --quiet` answers 1 for a ref that is not there and
    /// another non-zero status for a repository it could not read, and only the
    /// first means absent (`vibe/core/git/repo.py:394-404`).
    pub(crate) fn branch_exists(&self, branch: &str) -> Result<bool, WorktreeError> {
        let reference = format!("refs/heads/{branch}");
        let output = self.output(
            ["show-ref", "--verify", "--quiet", reference.as_str()],
            branch,
        )?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(WorktreeError::git(format!(
                "failed to inspect branch `{branch}`: {}",
                message(&output)
            ))),
        }
    }

    /// Deletes a branch, safely unless `force` (`vibe/core/git/repo.py:406-410`).
    pub(crate) fn delete_branch(&self, branch: &str, force: bool) -> Result<(), WorktreeError> {
        let flag = if force { "-D" } else { "-d" };
        self.checked(["branch", flag, branch], branch)
    }

    /// The remote-tracking ref for `remote`'s default branch, as
    /// `origin/main`, or [`None`] when the remote is absent or its `HEAD` was
    /// never recorded locally (`vibe/core/git/repo.py:412-423`).
    pub(crate) fn remote_default_branch_ref(&self, remote: &str) -> Option<String> {
        let symbolic = format!("refs/remotes/{remote}/HEAD");
        let output = self
            .output(["symbolic-ref", "--quiet", symbolic.as_str()], remote)
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .strip_prefix("refs/remotes/")
            .map(str::to_owned)
    }

    /// Checks a worktree out at `target`, creating `branch` when asked to.
    ///
    /// The start point is omitted rather than defaulted when there is none,
    /// because git's own default is the invoking checkout's `HEAD`, which is
    /// the right answer for a repository with no remote
    /// (`vibe/core/git/repo.py:460-486`).
    pub(crate) fn add_worktree(
        &self,
        target: &Path,
        branch: &str,
        branch_created: bool,
        start_point: Option<&str>,
    ) -> Result<(), WorktreeError> {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|source| WorktreeError::io(parent, source))?;
        }
        let target_text = super::path_text(target)?;
        let mut arguments = vec!["worktree", "add"];
        if branch_created {
            arguments.extend(["-b", branch, target_text]);
            arguments.extend(start_point);
        } else {
            arguments.extend([target_text, branch]);
        }
        let output = self.output(arguments, branch)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(WorktreeError::git(format!(
                "failed to create worktree `{}` for branch `{branch}`: {}",
                target
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                message(&output)
            )))
        }
    }

    /// Removes the worktree at `target`, carrying git's own words on failure
    /// (`vibe/core/git/repo.py:488-492`).
    pub(crate) fn remove_worktree(&self, target: &Path) -> Result<(), WorktreeError> {
        let target_text = super::path_text(target)?;
        self.checked(["worktree", "remove", "--force", target_text], "remove")
    }

    /// Every checkout git reports for this repository, the primary first.
    ///
    /// `-z` is tried first because a NUL-separated listing is the only one that
    /// survives a path containing a newline, and a git too old for it is asked
    /// again without (`vibe/core/git/repo.py:494-512`).
    pub(crate) fn records(&self) -> Result<Vec<WorktreeListing>, WorktreeError> {
        let terminated = self.output(["worktree", "list", "--porcelain", "-z"], "list")?;
        if terminated.status.success() {
            return Ok(parse_worktree_records(
                &String::from_utf8_lossy(&terminated.stdout),
                '\0',
            ));
        }
        if terminated.status.code() != Some(GIT_USAGE_ERROR_STATUS) {
            return Err(WorktreeError::ListFailed {
                message: message(&terminated),
            });
        }
        let plain = self.output(["worktree", "list", "--porcelain"], "list")?;
        if !plain.status.success() {
            return Err(WorktreeError::ListFailed {
                message: message(&plain),
            });
        }
        Ok(parse_worktree_records(
            &String::from_utf8_lossy(&plain.stdout),
            '\n',
        ))
    }

    /// The commit `HEAD` names in this checkout.
    ///
    /// A failure is a git-level refusal naming the checkout, whatever git said,
    /// which is what `GitRepo.head_commit_at` raises
    /// (`vibe/core/git/repo.py:157-169`).
    pub(crate) fn head_commit(&self) -> Result<String, WorktreeError> {
        let output = self.output(
            ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
            "HEAD",
        );
        let name = self
            .working_dir
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        match output {
            Ok(output) if output.status.success() => {
                let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if commit.is_empty() {
                    Err(WorktreeError::git(format!(
                        "failed to inspect HEAD for `{name}`: HEAD names no commit"
                    )))
                } else {
                    Ok(commit)
                }
            }
            Ok(output) => Err(WorktreeError::git(format!(
                "failed to inspect HEAD for `{name}`: {}",
                message(&output)
            ))),
            Err(error @ WorktreeError::GitUnavailable(_)) => Err(error),
            Err(error) => Err(WorktreeError::git(format!(
                "failed to inspect HEAD for `{name}`: {error}"
            ))),
        }
    }

    /// The branch this checkout is on, or [`None`] on a detached `HEAD`
    /// (`vibe/core/git/repo.py:285-290`).
    pub(crate) fn branch(&self) -> Option<String> {
        let output = self
            .output(["symbolic-ref", "--quiet", "--short", "HEAD"], "HEAD")
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!branch.is_empty()).then_some(branch)
    }

    /// A best-effort guess at the branch this work merges back into, without
    /// asking the network (`vibe/core/git/repo.py:292-327`).
    pub(crate) fn base_branch(&self) -> Option<String> {
        self.origin_head_branch()
            .or_else(|| self.configured_default_branch())
            .or_else(|| {
                CONVENTIONAL_BASE_BRANCHES
                    .iter()
                    .find(|branch| self.has_branch(branch))
                    .map(|branch| (*branch).to_owned())
            })
    }

    fn origin_head_branch(&self) -> Option<String> {
        let output = self
            .output(
                ["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
                "origin",
            )
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let reference = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let branch = reference
            .strip_prefix("origin/")
            .unwrap_or(&reference)
            .to_owned();
        (!branch.is_empty()).then_some(branch)
    }

    fn configured_default_branch(&self) -> Option<String> {
        let output = self
            .output(["config", "--get", "init.defaultBranch"], "config")
            .ok()?;
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!branch.is_empty() && self.has_branch(&branch)).then_some(branch)
    }

    fn has_branch(&self, branch: &str) -> bool {
        [
            format!("refs/remotes/origin/{branch}"),
            format!("refs/heads/{branch}"),
        ]
        .iter()
        .any(|reference| self.has_ref(reference))
    }

    fn has_ref(&self, reference: &str) -> bool {
        self.output(["show-ref", "--verify", "--quiet", reference], reference)
            .is_ok_and(|output| output.status.success())
    }

    /// Committed lines `reference` added and removed since it left its base.
    ///
    /// Measured from the merge base, so commits that landed on the base since
    /// are not counted against the branch, and against the remote-tracking
    /// base when there is one. [`None`] is "nothing to compare": no base, an
    /// unborn head, a gone ref or unrelated history
    /// (`vibe/core/git/repo.py:242-283`).
    pub(crate) fn changes_on(&self, reference: &str) -> Option<BranchChanges> {
        let base = self.base_branch()?;
        let base_ref = [
            format!("refs/remotes/origin/{base}"),
            format!("refs/heads/{base}"),
        ]
        .into_iter()
        .find(|candidate| self.has_ref(candidate))?;
        let merge_base = self
            .stdout(["merge-base", reference, base_ref.as_str()], reference)
            .ok()?;
        if merge_base.is_empty() {
            return None;
        }
        let range = format!("{merge_base}..{reference}");
        let numstat = self
            .stdout(["diff", "--numstat", range.as_str()], reference)
            .ok()?;
        Some(sum_numstat(&numstat))
    }
}

/// Totals one `git diff --numstat` block, binary files contributing nothing
/// (`vibe/core/git/repo.py:551-568`).
pub(crate) fn sum_numstat(numstat: &str) -> BranchChanges {
    let mut changes = BranchChanges {
        additions: 0,
        deletions: 0,
    };
    for line in numstat.lines() {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() < 3 {
            continue;
        }
        if let Ok(added) = fields[0].parse::<u64>() {
            changes.additions += added;
        }
        if let Ok(removed) = fields[1].parse::<u64>() {
            changes.deletions += removed;
        }
    }
    changes
}

/// Splits a porcelain listing into records on `separator`.
///
/// An empty token ends the record being built, which is how both spellings
/// separate their entries, and a `worktree` attribute also ends the previous
/// one, so a listing whose last entry is not terminated still yields it.
/// Attributes this does not name are skipped rather than refused, because git
/// is free to add them (`vibe/core/git/repo.py:521-548`).
pub(crate) fn parse_worktree_records(output: &str, separator: char) -> Vec<WorktreeListing> {
    let mut records = Vec::new();
    let mut current: Option<WorktreeListing> = None;
    for token in output.split(separator) {
        if token.is_empty() {
            records.extend(current.take());
            continue;
        }
        let (field, value) = token.split_once(' ').unwrap_or((token, ""));
        match field {
            "worktree" => {
                records.extend(current.take());
                current = Some(WorktreeListing {
                    root: PathBuf::from(value),
                    branch: None,
                    prunable: false,
                });
            }
            "branch" => {
                if let Some(record) = current.as_mut() {
                    record.branch = Some(
                        value
                            .strip_prefix("refs/heads/")
                            .unwrap_or(value)
                            .to_owned(),
                    );
                }
            }
            "prunable" => {
                if let Some(record) = current.as_mut() {
                    record.prunable = true;
                }
            }
            _ => {}
        }
    }
    records.extend(current);
    records
}

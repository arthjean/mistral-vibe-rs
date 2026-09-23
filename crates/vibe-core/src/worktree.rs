//! Managed git worktrees: preparation, ownership, retention and removal.
//!
//! The reference publishes this contract from `vibe/core/git/worktree/`, over
//! the git layer in `vibe/core/git/repo.py` and the error hierarchy in
//! `vibe/core/git/errors.py`, one layer below its CLI and its app server, and
//! this module sits in the same place for the same reason: a session lifecycle
//! is not a terminal concern.
//!
//! The pieces, in the reference's own split:
//!
//! - [`WorktreeRepository`], the managed worktrees of one repository: prepare
//!   a named one, prepare one named for a prompt, list the linked ones, put a
//!   retained one back (`repository.py:237-1062`);
//! - [`ManagedWorktree`], one worktree Vibe created, addressed by a path inside
//!   it: who holds it, releasing it, and the retention sweep
//!   (`repository.py:663-1099`);
//! - the claims under `<vibe home>/worktrees/.claims`, which is how two
//!   processes agree on who owns and who occupies a worktree (`record.py`);
//! - [`lifecycle`], the path-shaped session lifecycle both the CLI and the app
//!   server drive (`vibe/core/session/worktrees.py`);
//! - the naming of worktrees nobody named (`naming.py`, `naming_model.py`).
//!
//! Everything a terminal owns stays in the CLI adapter: prompting before a
//! destructive removal, reading stdin, and resolving `--add-dir`.
//!
//! `crates/vibe-core/src/worktree/worktree_parity_tests.rs` replays the
//! reference's own verdicts over the same scripted repositories, so a
//! divergence in this module fails the build instead of aging into a wrong
//! scorecard row.

use std::cell::OnceCell;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use thiserror::Error;

pub mod fetch;
mod git;
pub mod lifecycle;
mod managed;
mod naming;
pub mod naming_model;
mod record;
mod slug_table;

pub use git::BranchChanges;
pub use managed::{
    ManagedWorktree, RetainedRepositoryMapping, SNAPSHOT_REF_PREFIX, WorktreeRelease,
    WorktreeReleaseOutcome,
};
pub use naming::{
    MAX_WORKTREE_NAME_LENGTH, random_slug, worktree_name_from_text, worktree_name_with_suffix,
};
pub use record::{
    CLAIMS_DIR_NAME, PendingSessionHold, PruneLock, WorktreeClaim, WorktreeRecord,
    WorktreeRecoveryRecord, managed_bucket_name,
};

use git::{DEFAULT_REMOTE, GitRepo, RepoPaths};

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod managed_tests;
#[cfg(test)]
mod naming_tests;
#[cfg(test)]
mod worktree_parity_tests;
#[cfg(test)]
mod worktree_tests;

/// The directory the managed roots live under, inside the vibe home.
const MANAGED_DIRECTORY: &str = "worktrees";

/// How many hex digits of the common git directory's digest name a managed
/// root (`vibe/core/git/worktree/record.py:103-105`).
const REPOSITORY_DIGEST_LENGTH: usize = 12;

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

/// The prefix every automatically named worktree's branch carries
/// (`vibe/core/git/worktree/repository.py:33`).
pub const AUTO_WORKTREE_BRANCH_PREFIX: &str = "vibe/";

/// How many names an automatic preparation tries before giving up, and how
/// many recovery branch names a restore tries
/// (`vibe/core/git/worktree/repository.py:34`).
const MAX_AUTO_WORKTREE_ATTEMPTS: usize = 100;

/// What Windows canonicalization prefixes an absolute path with.
const VERBATIM_PREFIX: &str = r"\\?\";

/// The `\\?\UNC\` form, which names a network share rather than a drive.
const VERBATIM_UNC_PREFIX: &str = r"\\?\UNC\";

/// One character Python calls non-printable: anything in the Unicode `Other`
/// or `Separator` categories, minus the ASCII space.
///
/// `str.isprintable` is what the reference tests a name with
/// (`vibe/core/git/worktree/repository.py:1139-1141`), and the classes come from
/// `regex`, which carries the Unicode tables the check needs.
#[expect(
    clippy::expect_used,
    reason = "the pattern is a compile-time constant that compiles"
)]
static NON_PRINTABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[^\P{C} ]|[^\P{Z} ]").expect("the non-printable pattern compiles")
});

/// The directory every managed worktree of every checkout lives under,
/// resolved once, and the claims beside them.
///
/// The reference reads the same path as a global (`WORKTREES_DIR`, from
/// `VIBE_HOME`). Here it is a value the caller builds from the vibe home its
/// session runs under, which is what lets a test or a second vibe home use
/// its own. It is resolved so a vibe home reached through a symbolic link
/// names the same directory as one reached directly.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ManagedRoot {
    path: PathBuf,
}

impl ManagedRoot {
    #[must_use]
    pub fn for_vibe_home(vibe_home: &Path) -> Self {
        Self {
            path: managed_worktrees_root(vibe_home),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where ownership records and holders live.
    #[must_use]
    pub fn claims(&self) -> PathBuf {
        self.path.join(CLAIMS_DIR_NAME)
    }

    /// The managed root a prepared worktree's directory sits under, two levels
    /// up from it: `<managed root>/<bucket>/<name>`.
    #[must_use]
    pub fn containing(worktree_root: &Path) -> Option<Self> {
        let path = worktree_root.parent()?.parent()?;
        Some(Self {
            path: path.to_path_buf(),
        })
    }
}

/// A worktree a session prepared, and everything cleanup needs to judge it.
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
    /// The attachment hold a reused worktree comes back with, which the caller
    /// turns into its session's holder or releases when it gives up
    /// (`vibe/core/git/worktree/repository.py:64-75`).
    pub pending_hold: Option<PendingSessionHold>,
}

/// A linked worktree of a checkout, as enumeration reports it.
///
/// Deliberately not a [`PreparedWorktree`]: nothing here was created by this
/// process, so there is no base commit to measure against and no decision about
/// whether removing it may delete a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedWorktree {
    pub name: String,
    pub branch: String,
    pub root: PathBuf,
    pub path: PathBuf,
    pub repo_root: PathBuf,
}

/// What a prepared worktree holds that removing it would destroy.
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

    /// Why removing this worktree would lose work, in the reference's order: the
    /// two booleans first, then the commit count with the noun agreeing with it
    /// (`vibe/core/git/worktree/repository.py:209-234`).
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
                "{} {noun} added during this session",
                self.new_commit_count
            ));
        }
        reasons
    }
}

/// Every way a worktree operation can refuse.
///
/// One enum where the reference publishes a hierarchy: `GitError` with its
/// `GitUnavailableError` and `GitRepositoryNotFoundError` subclasses, and
/// `WorktreeError` below `GitError` for what the worktree layer itself refuses
/// (`vibe/core/git/errors.py`, `vibe/core/git/worktree/repository.py:45`).
/// [`WorktreeError::reference_class`] names which class each variant stands
/// for, which is what a caller that branches on the class reads.
#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error(
        "--worktree NAME must be one portable path segment: no separator, no drive letter, \
             no character a Windows path forbids, and no reserved device name"
    )]
    InvalidName,
    /// A branch git itself refuses to name a ref with, a git-level refusal
    /// asked of `git check-ref-format --branch` (`vibe/core/git/repo.py:388-392`).
    #[error("`{branch}` is not a valid git branch name")]
    InvalidBranch { branch: String },
    /// A managed root that resolved out of the vibe home it was built under.
    #[error("managed worktree root `{target}` resolves outside `{managed_root}`")]
    ManagedRootEscape {
        target: PathBuf,
        managed_root: PathBuf,
    },
    /// The path is inside no git repository.
    #[error("--worktree requires a git repository")]
    RepositoryRequired,
    #[error("git worktree operations require git on PATH: {0}")]
    GitUnavailable(String),
    /// A refusal of the git layer rather than of the worktree layer: a command
    /// git failed, a layout it cannot answer for, a base outside the checkout.
    #[error("{message}")]
    Git { message: String },
    /// A refusal of the worktree layer, naming the worktree.
    #[error("worktree `{name}` failed: {message}")]
    Failed { name: String, message: String },
    /// Git refused to enumerate the worktrees of a checkout that exists.
    #[error("failed to list git worktrees: {message}")]
    ListFailed { message: String },
    /// A failure that carries a second failure it did not replace.
    ///
    /// The reference attaches the rollback's own failure to the original
    /// exception with `add_note` rather than raising it
    /// (`vibe/core/git/worktree/repository.py:506-510`), so the class of the
    /// original is what the note is attached to.
    #[error("{source}; {note}")]
    Noted {
        name: String,
        #[source]
        source: Box<WorktreeError>,
        note: String,
    },
    /// The claim store refused: an unusable holder id, a holder already
    /// active, or a record that could not be written
    /// (`vibe/core/git/worktree/record.py:40`).
    #[error("{message}")]
    Record { message: String },
    #[error("worktree I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl WorktreeError {
    fn failed(name: &str, message: impl Into<String>) -> Self {
        Self::Failed {
            name: name.to_owned(),
            message: message.into(),
        }
    }

    fn git(message: impl Into<String>) -> Self {
        Self::Git {
            message: message.into(),
        }
    }

    fn record(message: impl Into<String>) -> Self {
        Self::Record {
            message: message.into(),
        }
    }

    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    /// The exception class the reference raises for this refusal.
    #[must_use]
    pub fn reference_class(&self) -> &'static str {
        match self {
            Self::InvalidName | Self::ManagedRootEscape { .. } | Self::Failed { .. } => {
                "WorktreeError"
            }
            Self::InvalidBranch { .. } | Self::Git { .. } | Self::ListFailed { .. } => "GitError",
            Self::RepositoryRequired => "GitRepositoryNotFoundError",
            Self::GitUnavailable(_) => "GitUnavailableError",
            Self::Noted { source, .. } => source.reference_class(),
            Self::Record { .. } => "WorktreeRecordError",
            Self::Io { .. } => "OSError",
        }
    }

    /// Whether the reference would catch this as a `GitError`, which is what
    /// its callers catch.
    #[must_use]
    pub fn is_git_error(&self) -> bool {
        !matches!(self.reference_class(), "WorktreeRecordError" | "OSError")
    }
}

/// The managed worktrees of one git repository.
///
/// Opened against a path inside the repository rather than the repository
/// root, because that path decides two things at once: which repository is
/// meant, and which subdirectory of a prepared worktree the caller lands in
/// (`vibe/core/git/worktree/repository.py:237-247`).
#[derive(Debug)]
pub struct WorktreeRepository {
    git: GitRepo,
    base: PathBuf,
    managed: ManagedRoot,
    paths: OnceCell<RepoPaths>,
}

impl WorktreeRepository {
    /// Opens the repository `base` sits in.
    pub fn open(base: &Path, managed: &ManagedRoot) -> Result<Self, WorktreeError> {
        Self::open_at(base, None, managed)
    }

    /// Opens the repository at `repository_root` while keeping `base` as the
    /// position, which is how a retained worktree's session is listed against
    /// the repository it came from.
    pub fn open_at(
        base: &Path,
        repository_root: Option<&Path>,
        managed: &ManagedRoot,
    ) -> Result<Self, WorktreeError> {
        let git = GitRepo::open(repository_root.unwrap_or(base))?;
        Ok(Self {
            git,
            base: base.to_path_buf(),
            managed: managed.clone(),
            paths: OnceCell::new(),
        })
    }

    /// The repository a path belongs to, as its bucket name, or [`None`] when
    /// it is in none (`vibe/core/git/worktree/repository.py:263-271`).
    #[must_use]
    pub fn bucket_for(base: &Path, managed: &ManagedRoot) -> Option<String> {
        Self::open(base, managed).ok()?.bucket().ok()
    }

    fn paths(&self) -> Result<&RepoPaths, WorktreeError> {
        if let Some(paths) = self.paths.get() {
            return Ok(paths);
        }
        let paths = self.git.paths()?;
        Ok(self.paths.get_or_init(|| paths))
    }

    /// The primary checkout.
    pub fn root(&self) -> Result<PathBuf, WorktreeError> {
        Ok(self.paths()?.repo_root.clone())
    }

    /// The repository's bucket under the managed root.
    pub fn bucket(&self) -> Result<String, WorktreeError> {
        let paths = self.paths()?;
        Ok(managed_bucket_name(&paths.repo_root, &paths.common_git_dir))
    }

    /// Where this repository's managed worktrees live, refused when it would
    /// resolve out of the managed root.
    pub fn worktree_root(&self) -> Result<PathBuf, WorktreeError> {
        let paths = self.paths()?;
        managed_worktree_root_in(&self.managed, &paths.repo_root, &paths.common_git_dir)
    }

    fn relative_base(&self) -> Result<PathBuf, WorktreeError> {
        self.git.relative_base(&self.base)
    }

    /// Where the base sits when mapped onto the primary checkout, under the
    /// checks [`WorktreeRepository::linked`] applies, or [`None`] when it has
    /// no usable counterpart there (`vibe/core/git/worktree/repository.py:273-293`).
    #[must_use]
    pub fn repository_counterpart(&self) -> Option<PathBuf> {
        let root = self.root().ok()?;
        let relative = self.relative_base().ok()?;
        target_cwd(&root, &relative, "counterpart").ok()
    }

    /// The same position joined onto the primary checkout without requiring it
    /// to exist there.
    pub fn repository_mapped_cwd(&self) -> Result<PathBuf, WorktreeError> {
        let root = self.root()?;
        let relative = self.relative_base()?;
        // Joining an empty path would append a separator the reference's
        // `Path` join never writes.
        if relative.as_os_str().is_empty() {
            return Ok(root);
        }
        Ok(root.join(relative))
    }

    /// The branch the checkout this repository was opened at is on.
    #[must_use]
    pub fn branch(&self) -> Option<String> {
        self.git.branch()
    }

    /// Prepares the managed worktree `name`, checked out on `branch` or on a
    /// branch of its own name.
    ///
    /// The directory is claimed with an atomic `mkdir` before git runs, so two
    /// preparations of the same name cannot both run `git worktree add`, and
    /// the ownership record is written before the add, so a crash never leaves
    /// a worktree nothing owns. A directory that is already there is reused
    /// when it is this repository's worktree on the expected branch, and comes
    /// back with an attachment hold (`vibe/core/git/worktree/repository.py:321-377`).
    pub fn prepare(
        &self,
        name: &str,
        branch: Option<&str>,
    ) -> Result<PreparedWorktree, WorktreeError> {
        validate_worktree_name(name)?;
        let branch = branch.unwrap_or(name);
        self.git.validate_branch(branch)?;
        let paths = self.paths()?.clone();
        let target = self.worktree_root()?.join(name);
        let bucket = self.bucket()?;
        let claim = WorktreeClaim::new(&self.managed, &bucket, name);
        if claim.has_recovery() && !target.exists() {
            return Err(WorktreeError::failed(
                name,
                "the name is reserved by a retained session snapshot",
            ));
        }

        match create_directory(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let pending_hold = ManagedWorktree::new(claim).hold_for_attachment()?;
                let reused = validate_existing_worktree(&target, branch, &paths.common_git_dir)
                    .and_then(|()| {
                        self.build_prepared(
                            name,
                            branch,
                            &target,
                            false,
                            false,
                            pending_hold.clone(),
                        )
                    });
                if reused.is_err()
                    && let Some(hold) = &pending_hold
                {
                    hold.release();
                }
                return reused;
            }
            Err(error) => {
                return Err(WorktreeError::failed(
                    name,
                    format!(
                        "failed to claim worktree directory `{}`: {error}",
                        target.display()
                    ),
                ));
            }
        }

        let branch_created = !self.git.branch_exists(branch)?;
        let record = WorktreeRecord::new(name, branch, &paths.repo_root, branch_created);
        record_starting_claim(&claim, &record, &target)?;
        self.create(&claim, &record, &target, branch_created, None)
    }

    /// Prepares a new worktree named after `suggested`, else `prompt`, else a
    /// random slug, on a `vibe/<name>` branch.
    ///
    /// Never reuses: a name whose branch or directory is taken, or that a
    /// retained snapshot reserves, moves on to the next suffix
    /// (`vibe/core/git/worktree/repository.py:379-391`).
    pub fn prepare_auto(
        &self,
        prompt: Option<&str>,
        suggested: Option<&str>,
    ) -> Result<PreparedWorktree, WorktreeError> {
        let repo_root = self.root()?;
        let (name, branch, target) =
            self.claim_auto_name(&auto_worktree_name(prompt, suggested))?;
        let claim = WorktreeClaim::new(&self.managed, &self.bucket()?, &name);
        let record = WorktreeRecord::new(&name, &branch, &repo_root, true);
        record_starting_claim(&claim, &record, &target)?;
        self.create(&claim, &record, &target, true, None)
    }

    /// Every checkout git reports for this repository, resolved, with its
    /// branch or [`None`] when detached. Wider than
    /// [`WorktreeRepository::linked`]: it keeps the checkouts a session may be
    /// sitting in but may not be moved into
    /// (`vibe/core/git/worktree/repository.py:415-433`).
    pub fn checkouts(&self) -> Result<Vec<(PathBuf, Option<String>)>, WorktreeError> {
        Ok(self
            .git
            .records()?
            .into_iter()
            .map(|record| (resolve_lenient(&record.root), record.branch))
            .collect())
    }

    /// Every linked worktree a session may be moved into, in path order.
    ///
    /// The primary checkout is dropped, and so is a record git marks prunable,
    /// one on a detached `HEAD`, one whose path crosses a link or is no longer a
    /// worktree, and one where the base's position does not resolve. A stale
    /// entry is not a reason to refuse to answer. The base is resolved up front,
    /// so a base outside the checkout is the caller's error rather than a skipped
    /// entry (`vibe/core/git/worktree/repository.py:435-462`).
    pub fn linked(&self) -> Result<Vec<LinkedWorktree>, WorktreeError> {
        let relative_base = self.relative_base()?;
        let records = self.git.records()?;
        let repo_root = self.root()?;
        let mut linked = Vec::new();
        for record in records.into_iter().skip(1) {
            let (Some(branch), false) = (record.branch, record.prunable) else {
                continue;
            };
            if validate_listed_worktree(&record.root).is_err() {
                continue;
            }
            let root = resolve_lenient(&record.root);
            let Ok(path) = target_cwd(&root, &relative_base, "list") else {
                continue;
            };
            let name = root
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default();
            linked.push(LinkedWorktree {
                name,
                branch,
                root,
                path,
                repo_root: repo_root.clone(),
            });
        }
        // Ordered by the string form of the path, which is the key the
        // reference sorts on.
        linked.sort_by(|left, right| {
            left.path
                .to_string_lossy()
                .cmp(&right.path.to_string_lossy())
        });
        Ok(linked)
    }

    /// Lines added and removed on each branch against the repository's base,
    /// measured from the checkout this repository was opened at.
    #[must_use]
    pub fn changes_on(&self, branch: &str) -> Option<BranchChanges> {
        self.git.changes_on(branch)
    }

    /// The ref a newly created branch starts from: the remote's default branch,
    /// refreshed first when the remote answers, or [`None`] to leave git's own
    /// default of the invoking checkout's `HEAD`.
    ///
    /// A failed refresh keeps the stale remote-tracking ref, which is still a
    /// better base than a local branch that only moves when pulled
    /// (`vibe/core/git/worktree/repository.py:464-488`).
    fn base_ref(&self) -> Option<String> {
        let reference = self.git.remote_default_branch_ref(DEFAULT_REMOTE)?;
        let (remote, branch) = reference
            .split_once('/')
            .unwrap_or((reference.as_str(), ""));
        if let Err(error) = fetch::fetch_branch(&self.git, remote, branch) {
            crate::observability::log(
                crate::observability::LogLevel::Warning,
                &format!("Could not refresh {reference} before branching: {error}"),
            );
        }
        Some(reference)
    }

    fn create(
        &self,
        claim: &WorktreeClaim,
        record: &WorktreeRecord,
        target: &Path,
        branch_created: bool,
        start_point: Option<&str>,
    ) -> Result<PreparedWorktree, WorktreeError> {
        let branch = record.branch.as_str();
        let base = match start_point {
            Some(start) => Some(start.to_owned()),
            None if branch_created => self.base_ref(),
            None => None,
        };
        if let Err(error) = self
            .git
            .add_worktree(target, branch, branch_created, base.as_deref())
        {
            self.discard_claim(target, branch, branch_created);
            return Err(error);
        }
        let prepared =
            match self.build_prepared(&record.name, branch, target, true, branch_created, None) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return Err(
                        match self.clean_up_failed_prepare(target, branch, branch_created) {
                            Some(note) => WorktreeError::Noted {
                                name: record.name.clone(),
                                source: Box::new(error),
                                note,
                            },
                            None => error,
                        },
                    );
                }
            };
        let mut completed = record.clone();
        completed.base_commit = Some(prepared.base_commit.clone());
        claim.write(&completed)?;
        Ok(prepared)
    }

    /// Recreates a worktree retention removed, from its snapshot, on its old
    /// branch or on the first free `-restored` variant of it
    /// (`vibe/core/git/worktree/repository.py:512-573`).
    pub(crate) fn restore(
        &self,
        claim: &WorktreeClaim,
        recovery: &WorktreeRecoveryRecord,
    ) -> Result<PreparedWorktree, WorktreeError> {
        if claim.bucket != self.bucket()? || claim.name != recovery.name {
            return Err(WorktreeError::failed(
                &claim.name,
                "the recovery metadata does not match its repository",
            ));
        }
        let target = self.worktree_root()?.join(&claim.name);
        claim.mark_starting().map_err(|_| {
            WorktreeError::failed(
                &claim.name,
                format!(
                    "a restore is already in progress for `{}`",
                    target.display()
                ),
            )
        })?;

        let mut reserved = false;
        let reservation = (|| {
            let branch = self.available_recovery_branch(&recovery.branch)?;
            let record = WorktreeRecord::new(&claim.name, &branch, &self.root()?, true);
            match create_directory(&target) {
                Ok(()) => reserved = true,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(WorktreeError::failed(
                        &claim.name,
                        format!("cannot restore into occupied path `{}`", target.display()),
                    ));
                }
                Err(error) => {
                    return Err(WorktreeError::failed(
                        &claim.name,
                        format!("failed to reserve `{}`: {error}", target.display()),
                    ));
                }
            }
            claim.write(&record)?;
            Ok(record)
        })();
        let record = match reservation {
            Ok(record) => record,
            Err(error) => {
                claim.finish_starting();
                if reserved {
                    let _ = fs::remove_dir(&target);
                }
                return Err(error);
            }
        };
        self.create(claim, &record, &target, true, Some(&recovery.snapshot_ref))
            .inspect_err(|_| claim.finish_starting())
    }

    fn available_recovery_branch(&self, preferred: &str) -> Result<String, WorktreeError> {
        if !self.git.branch_exists(preferred)? {
            return Ok(preferred.to_owned());
        }
        let base = format!("{preferred}-restored");
        for suffix in 1..=MAX_AUTO_WORKTREE_ATTEMPTS {
            let candidate = if suffix == 1 {
                base.clone()
            } else {
                format!("{base}-{suffix}")
            };
            if !self.git.branch_exists(&candidate)? {
                return Ok(candidate);
            }
        }
        Err(WorktreeError::failed(
            preferred,
            "no unused recovery branch name was found",
        ))
    }

    /// The record of a worktree now on disk.
    ///
    /// The base commit is the worktree's own `HEAD` at session start, so
    /// cleanup counts only commits added during the session, not those an
    /// attached branch already carried (`vibe/core/git/worktree/repository.py:586-608`).
    fn build_prepared(
        &self,
        name: &str,
        branch: &str,
        target: &Path,
        created: bool,
        branch_created: bool,
        pending_hold: Option<PendingSessionHold>,
    ) -> Result<PreparedWorktree, WorktreeError> {
        let path = target_cwd(target, &self.relative_base()?, name)?;
        let repo_root = self.root()?;
        let base_commit = GitRepo::at(target)?.head_commit()?;
        Ok(PreparedWorktree {
            name: name.to_owned(),
            branch: branch.to_owned(),
            root: target.to_path_buf(),
            path,
            repo_root,
            base_commit,
            created,
            branch_created,
            pending_hold,
        })
    }

    /// The first free name for an automatic worktree, with its branch and its
    /// claimed directory.
    ///
    /// The `mkdir` is the atomic claim: `git worktree add` cannot say why it
    /// failed, so a lost race has to be detected before git runs
    /// (`vibe/core/git/worktree/repository.py:610-635`).
    fn claim_auto_name(&self, base_name: &str) -> Result<(String, String, PathBuf), WorktreeError> {
        let worktree_root = self.worktree_root()?;
        let bucket = self.bucket()?;
        for name in auto_worktree_candidates(base_name) {
            let branch = format!("{AUTO_WORKTREE_BRANCH_PREFIX}{name}");
            let claim = WorktreeClaim::new(&self.managed, &bucket, &name);
            if self.git.branch_exists(&branch)? || claim.has_recovery() {
                continue;
            }
            let target = worktree_root.join(&name);
            match create_directory(&target) {
                Ok(()) => return Ok((name, branch, target)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(WorktreeError::failed(
                        &name,
                        format!(
                            "failed to claim worktree directory `{}`: {error}",
                            target.display()
                        ),
                    ));
                }
            }
        }
        Err(WorktreeError::failed(
            base_name,
            format!(
                "no unused worktree name was found after {MAX_AUTO_WORKTREE_ATTEMPTS} attempts"
            ),
        ))
    }

    /// Takes back a reservation whose `git worktree add` failed.
    ///
    /// `rmdir` refuses a populated directory, so a partial checkout is never
    /// lost. Only a branch this reservation created is deleted, and safely, so
    /// a racing branch carrying commits survives
    /// (`vibe/core/git/worktree/repository.py:637-661`).
    fn discard_claim(&self, target: &Path, branch: &str, branch_created: bool) {
        delete_record_for(&self.managed, target);
        let _ = fs::remove_dir(target);
        if branch_created {
            let _ = self.git.delete_branch(branch, false);
        }
    }

    /// Undoes a worktree this call created after preparation failed on it, and
    /// answers the note to attach when the undoing itself failed
    /// (`vibe/core/git/worktree/repository.py:663-676`).
    fn clean_up_failed_prepare(
        &self,
        target: &Path,
        branch: &str,
        branch_created: bool,
    ) -> Option<String> {
        delete_record_for(&self.managed, target);
        let cleaned = self.git.remove_worktree(target).and_then(|()| {
            if branch_created {
                self.git.delete_branch(branch, true)
            } else {
                Ok(())
            }
        });
        cleaned.err().map(|error| {
            format!(
                "failed to clean up worktree `{}` after the preparation failed: {error}",
                target
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default()
            )
        })
    }
}

/// Reserves a claim before the add, as both preparation paths do, releasing
/// the reservation if the record cannot be written
/// (`vibe/core/git/worktree/repository.py:393-404`).
fn record_starting_claim(
    claim: &WorktreeClaim,
    record: &WorktreeRecord,
    target: &Path,
) -> Result<(), WorktreeError> {
    let started = claim.mark_starting().and_then(|()| claim.write(record));
    if started.is_err() {
        claim.delete();
        let _ = fs::remove_dir(target);
    }
    started
}

fn delete_record_for(managed: &ManagedRoot, target: &Path) {
    if let Some(claim) = WorktreeClaim::locate(managed, target) {
        claim.delete();
    }
}

/// `mkdir(parents=True)`: the parents may exist, the directory itself may not.
fn create_directory(target: &Path) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(target)
}

/// The name an automatic worktree starts from.
///
/// The model's answer and the raw prompt go through the same slugifier, and
/// either can reduce to nothing or to a reserved device name, hence the walk
/// down to a random slug (`vibe/core/git/worktree/repository.py:1103-1114`).
#[must_use]
pub fn auto_worktree_name(prompt: Option<&str>, suggested: Option<&str>) -> String {
    [suggested, prompt]
        .into_iter()
        .flatten()
        .map(worktree_name_from_text)
        .find(|name| is_portable_worktree_name(name))
        .unwrap_or_else(random_slug)
}

/// The base name, then its `-2` to `-100` variants.
fn auto_worktree_candidates(base_name: &str) -> impl Iterator<Item = String> + '_ {
    std::iter::once(base_name.to_owned()).chain(
        (2..=MAX_AUTO_WORKTREE_ATTEMPTS)
            .map(move |suffix| worktree_name_with_suffix(base_name, suffix)),
    )
}

/// Prepares the managed worktree named `name` for the checkout holding `base`.
///
/// The one-call form of [`WorktreeRepository::prepare`] for a caller that has
/// the vibe home rather than a [`ManagedRoot`].
pub fn prepare_worktree(
    name: &str,
    base: &Path,
    vibe_home: &Path,
    branch: Option<&str>,
) -> Result<PreparedWorktree, WorktreeError> {
    WorktreeRepository::open(base, &ManagedRoot::for_vibe_home(vibe_home))?.prepare(name, branch)
}

/// Every linked worktree of the checkout holding `base`, in path order.
pub fn list_linked_worktrees(base: &Path) -> Result<Vec<LinkedWorktree>, WorktreeError> {
    // Listing reads no claim, so the managed root is never consulted.
    let unused = ManagedRoot {
        path: PathBuf::new(),
    };
    WorktreeRepository::open(base, &unused)?.linked()
}

impl PreparedWorktree {
    /// What removing this worktree would discard, relative to the session
    /// start.
    ///
    /// The commit count is taken against the worktree's own `HEAD` rather than
    /// its named branch, so commits made on a detached `HEAD` still block
    /// cleanup (`vibe/core/git/worktree/repository.py:77-111`).
    pub fn inspect_for_cleanup(&self) -> Result<WorktreeCleanupState, WorktreeError> {
        let inspect = |error: WorktreeError| {
            WorktreeError::failed(
                &self.name,
                format!("failed to inspect the worktree: {error}"),
            )
        };
        let repo = GitRepo::at(&self.root)?;
        let status = repo
            .stdout(
                ["status", "--porcelain", "--untracked-files=all"],
                &self.name,
            )
            .map_err(inspect)?;
        let range = format!("{}..HEAD", self.base_commit);
        let new_commit_count = repo
            .stdout(["rev-list", "--count", range.as_str()], &self.name)
            .map_err(inspect)?
            .parse::<u64>()
            .map_err(|error| {
                WorktreeError::failed(&self.name, format!("invalid commit count: {error}"))
            })?;
        Ok(WorktreeCleanupState {
            has_uncommitted_changes: status.lines().any(|line| !line.starts_with("??")),
            has_untracked_files: status.lines().any(|line| line.starts_with("??")),
            new_commit_count,
        })
    }

    /// Removes the worktree, and its branch when `delete_branch`
    /// (`vibe/core/git/worktree/repository.py:148-156`).
    pub fn remove(&self, delete_branch: bool) -> Result<(), WorktreeError> {
        let removal = |error: WorktreeError| {
            WorktreeError::failed(
                &self.name,
                format!("failed to remove the worktree: {error}"),
            )
        };
        let repo = GitRepo::at(&self.repo_root)?;
        repo.remove_worktree(&self.root).map_err(removal)?;
        if delete_branch {
            repo.delete_branch(&self.branch, true).map_err(removal)?;
        }
        Ok(())
    }
}

/// What removing this worktree would discard, relative to the session start.
pub fn inspect_worktree_for_cleanup(
    worktree: &PreparedWorktree,
) -> Result<WorktreeCleanupState, WorktreeError> {
    worktree.inspect_for_cleanup()
}

/// Removes a prepared worktree, and its branch when the caller says so.
///
/// `delete_branch` is the caller's decision because only the caller knows
/// whether the branch predates the session.
pub fn remove_worktree(
    worktree: &PreparedWorktree,
    delete_branch: bool,
) -> Result<(), WorktreeError> {
    worktree.remove(delete_branch)
}

/// The directory every managed worktree of every checkout lives under.
///
/// Resolved so a vibe home reached through a symbolic link names the same
/// directory as one reached directly.
#[must_use]
pub fn managed_worktrees_root(vibe_home: &Path) -> PathBuf {
    resolve_lenient(&vibe_home.join(MANAGED_DIRECTORY))
}

/// The managed root a checkout's worktrees live under.
///
/// Both the managed directory and the repository directory are resolved
/// before the second is checked against the first, so a repository directory
/// that resolves out of the managed root is refused rather than written to
/// (`vibe/core/git/worktree/repository.py:1159-1167`).
#[cfg(test)]
fn managed_worktree_root(
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

fn managed_worktree_root_in(
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

/// The path string the digest is taken over, without what Windows
/// canonicalization prefixes it with.
///
/// The reference hashes `Path.resolve()`, which never carries the verbatim
/// prefix; `fs::canonicalize` on Windows always does. On every other platform
/// this is the identity.
fn strip_verbatim_prefix(value: &str) -> String {
    if let Some(remainder) = value.strip_prefix(VERBATIM_UNC_PREFIX) {
        format!(r"\\{remainder}")
    } else if let Some(remainder) = value.strip_prefix(VERBATIM_PREFIX) {
        remainder.to_owned()
    } else {
        value.to_owned()
    }
}

/// The reference's non-strict `resolve` over a path that may not exist yet:
/// the deepest existing ancestor is canonicalized and the remainder appended,
/// so two spellings of one location compare equal.
fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(resolved) = fs::canonicalize(path) {
        return resolved;
    }
    // The longest prefix that exists is resolved, and the rest is applied
    // lexically, `..` included, which is what a non-strict `Path.resolve()`
    // does with a tail that is not there.
    let components = path.components().collect::<Vec<_>>();
    for split in (1..components.len()).rev() {
        let prefix = components[..split].iter().collect::<PathBuf>();
        let Ok(mut resolved) = fs::canonicalize(&prefix) else {
            continue;
        };
        for component in &components[split..] {
            match component {
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::CurDir => {}
                other => resolved.push(other.as_os_str()),
            }
        }
        return resolved;
    }
    path.to_path_buf()
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
fn target_cwd(root: &Path, relative_base: &Path, name: &str) -> Result<PathBuf, WorktreeError> {
    let root = resolve_lenient(root);
    let path = root.join(relative_base);
    let resolved = fs::canonicalize(&path).map_err(|_| {
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
fn has_linked_path_component(path: &Path) -> bool {
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
fn validate_existing_worktree(
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
fn validate_listed_worktree(target: &Path) -> Result<(), WorktreeError> {
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
fn validate_worktree_name(name: &str) -> Result<(), WorktreeError> {
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
    if name.ends_with(' ') || name.ends_with('.') || NON_PRINTABLE.is_match(name) {
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
fn validate_branch_name(directory: &Path, branch: &str) -> Result<(), WorktreeError> {
    GitRepo::at(directory)?.validate_branch(branch)
}

/// A string as Python's `repr` spells it, which is how the reference quotes a
/// worktree or branch name in what it prints: single quotes unless the text
/// holds one and no double quote, the quote and the backslash escaped, and a
/// character `str.isprintable` refuses written as its escape.
#[must_use]
pub fn python_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut rendered = String::with_capacity(text.len() + 2);
    rendered.push(quote);
    let mut buffer = [0_u8; 4];
    for character in text.chars() {
        match character {
            '\\' => rendered.push_str("\\\\"),
            '\t' => rendered.push_str("\\t"),
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            _ if character == quote => {
                rendered.push('\\');
                rendered.push(character);
            }
            _ if NON_PRINTABLE.is_match(character.encode_utf8(&mut buffer)) => {
                let code = u32::from(character);
                if code < 0x100 {
                    rendered.push_str(&format!("\\x{code:02x}"));
                } else if code < 0x1_0000 {
                    rendered.push_str(&format!("\\u{code:04x}"));
                } else {
                    rendered.push_str(&format!("\\U{code:08x}"));
                }
            }
            _ => rendered.push(character),
        }
    }
    rendered.push(quote);
    rendered
}

fn path_text(path: &Path) -> Result<&str, WorktreeError> {
    path.to_str().ok_or_else(|| {
        WorktreeError::failed(
            "path",
            format!("path `{}` is not valid UTF-8", path.display()),
        )
    })
}

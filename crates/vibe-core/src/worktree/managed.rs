//! One worktree Vibe created, and the sweep that reclaims the unused ones.
//!
//! Reference `ManagedWorktree` (`vibe/core/git/worktree/repository.py:679-1099`).
//! A worktree is addressed by a path inside it, and [`ManagedWorktree::at`]
//! answering [`None`] is the whole "is this one of ours?" question: a directory
//! with no claim is not Vibe's, and each caller decides what that means.
//!
//! Removing a worktree that still holds work saves the work first, as a commit
//! under [`SNAPSHOT_REF_PREFIX`] that nothing checks out, so explicit deletion
//! and automatic retention can reclaim a directory without making its content
//! unrecoverable. A snapshot that cannot be written is the one reason left to
//! keep the directory.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::observability::{self, LogLevel};

use super::git::{self, GitRepo};
use super::record::{PruneLock, WorktreeClaim};
use super::{
    ManagedRoot, PendingSessionHold, PreparedWorktree, WorktreeError, WorktreeRecord,
    WorktreeRecoveryRecord, WorktreeRepository, resolve_lenient, validate_existing_worktree,
};

/// Where snapshots live: under `refs/vibe/` rather than `refs/heads/`, so a
/// snapshot never appears among the branches or as something to push
/// (`vibe/core/git/worktree/repository.py:35-38`).
pub const SNAPSHOT_REF_PREFIX: &str = "refs/vibe/reaped";

/// The index a snapshot is staged through, inside the worktree's own git
/// directory, so the worktree's index is untouched and the file goes away with
/// the worktree (`vibe/core/git/worktree/repository.py:113-146`).
const SNAPSHOT_INDEX: &str = "index.vibe-snapshot";

/// How a release ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeReleaseOutcome {
    Removed,
    KeptDirty,
    KeptInUse,
    KeptUnmanaged,
    NotFound,
}

impl WorktreeReleaseOutcome {
    /// The wire spelling, which is the reference enum's value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Removed => "removed",
            Self::KeptDirty => "kept_dirty",
            Self::KeptInUse => "kept_in_use",
            Self::KeptUnmanaged => "kept_unmanaged",
            Self::NotFound => "not_found",
        }
    }
}

/// What releasing a worktree did (`vibe/core/git/worktree/repository.py:195-206`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRelease {
    pub outcome: WorktreeReleaseOutcome,
    pub root: Option<PathBuf>,
    pub branch: Option<String>,
    pub branch_deleted: bool,
    pub reasons: Vec<String>,
    /// Where the work went when a removed worktree still had some, and
    /// [`None`] when it had none.
    pub snapshot_ref: Option<String>,
}

impl WorktreeRelease {
    fn outcome(outcome: WorktreeReleaseOutcome) -> Self {
        Self {
            outcome,
            root: None,
            branch: None,
            branch_deleted: false,
            reasons: Vec::new(),
            snapshot_ref: None,
        }
    }

    fn with_branch(mut self, branch: &str) -> Self {
        self.branch = Some(branch.to_owned());
        self
    }

    fn with_root(mut self, root: &Path) -> Self {
        self.root = Some(root.to_path_buf());
        self
    }
}

/// Where a retained worktree's session sits when mapped back onto the
/// repository it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedRepositoryMapping {
    pub root: PathBuf,
    pub cwd: PathBuf,
}

/// A worktree Vibe created, addressed through its claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedWorktree {
    claim: WorktreeClaim,
}

impl PreparedWorktree {
    /// Commits the worktree's whole state to a ref nothing checks out, and
    /// answers the ref.
    ///
    /// Untracked files are included and ignored ones are not. It is written
    /// through a second index, so a failure leaves the worktree exactly as it
    /// was, and a failure is the caller's signal to keep the worktree rather
    /// than remove it (`vibe/core/git/worktree/repository.py:113-146`).
    pub fn snapshot(&self) -> Result<String, WorktreeError> {
        let reference = format!("{SNAPSHOT_REF_PREFIX}/{}", self.name);
        let failed = |error: &dyn std::fmt::Display| {
            WorktreeError::failed(
                &self.name,
                format!("failed to snapshot the worktree: {error}"),
            )
        };
        let repo = GitRepo::at(&self.root)?;
        let git_directory = repo
            .stdout(["rev-parse", "--absolute-git-dir"], &self.name)
            .map_err(|error| failed(&error))?;
        let index = PathBuf::from(git_directory).join(SNAPSHOT_INDEX);
        let staged = |arguments: &[&str]| -> Result<String, WorktreeError> {
            let output = git::command(repo.executable(), &self.root)
                .env("GIT_INDEX_FILE", &index)
                .args(arguments)
                .output()
                .map_err(|error| failed(&error))?;
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
            } else {
                Err(failed(&git::message(&output)))
            }
        };
        staged(&["read-tree", "HEAD"])?;
        staged(&["add", "--all", "."])?;
        let tree = staged(&["write-tree"])?;
        let head = staged(&["rev-parse", "HEAD"])?;
        let message = format!(
            "vibe: snapshot of worktree {} taken before its removal",
            self.name
        );
        let commit = staged(&[
            "commit-tree",
            tree.as_str(),
            "-p",
            head.as_str(),
            "-m",
            message.as_str(),
        ])?;
        repo.checked(
            ["update-ref", reference.as_str(), commit.as_str()],
            &self.name,
        )
        .map_err(|error| failed(&error))?;
        Ok(reference)
    }
}

impl ManagedWorktree {
    #[must_use]
    pub fn new(claim: WorktreeClaim) -> Self {
        Self { claim }
    }

    /// The managed worktree `cwd` sits in, or [`None`] outside every one.
    ///
    /// Deliberately not conditioned on the record existing: a caller that
    /// deletes the record with [`ManagedWorktree::forget`] and only then drops
    /// its holder still has to find the claim
    /// (`vibe/core/git/worktree/repository.py:690-696`).
    #[must_use]
    pub fn at(managed: &ManagedRoot, cwd: &Path) -> Option<Self> {
        WorktreeClaim::locate(managed, cwd).map(Self::new)
    }

    #[must_use]
    pub fn claim(&self) -> &WorktreeClaim {
        &self.claim
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.claim.name
    }

    /// The worktree's directory.
    #[must_use]
    pub fn root(&self) -> PathBuf {
        self.claim
            .managed()
            .path()
            .join(&self.claim.bucket)
            .join(&self.claim.name)
    }

    /// Where `cwd` sits in the repository a retained worktree came from, or
    /// [`None`] when the worktree was not retained
    /// (`vibe/core/git/worktree/repository.py:707-720`).
    #[must_use]
    pub fn retained_repository_mapping(&self, cwd: &Path) -> Option<RetainedRepositoryMapping> {
        let recovery = self.claim.read_recovery()?;
        let requested = resolve_lenient(&crate::workspace::tool_path::expanded(cwd));
        let relative = requested
            .strip_prefix(resolve_lenient(&self.root()))
            .ok()?
            .to_path_buf();
        let root = resolve_lenient(&crate::workspace::tool_path::expanded(&recovery.repo_root));
        Some(RetainedRepositoryMapping {
            cwd: root.join(relative),
            root,
        })
    }

    /// Keeps the sweep off this worktree until a resolved session installs its
    /// own holder, or [`None`] when there is no record to hold
    /// (`vibe/core/git/worktree/repository.py:722-729`).
    pub fn hold_for_attachment(&self) -> Result<Option<PendingSessionHold>, WorktreeError> {
        let holder_id = format!("attach-{}", random_hex());
        let _lock = PruneLock::acquire(self.claim.managed())?;
        if self.claim.read().is_none() {
            return Ok(None);
        }
        self.claim.add_holder(&holder_id)?;
        Ok(Some(PendingSessionHold {
            claim: self.claim.clone(),
            holder_id,
        }))
    }

    /// Registers `session_id` as standing in this worktree, turning the
    /// pending hold into it, and ends the preparation marker.
    ///
    /// A directory with no record is not Vibe's to hold, so holding it only
    /// releases the pending hold (`vibe/core/git/worktree/repository.py:731-749`).
    pub fn hold(
        &self,
        session_id: &str,
        pending_hold: Option<&PendingSessionHold>,
    ) -> Result<(), WorktreeError> {
        if let Some(pending) = pending_hold
            && pending.claim != self.claim
        {
            pending.release();
            return Err(WorktreeError::failed(
                &self.claim.name,
                "the pending session hold does not match the session directory",
            ));
        }
        if self.claim.read().is_none() {
            if let Some(pending) = pending_hold {
                pending.release();
            }
            return Ok(());
        }
        let added = self.claim.add_holder(session_id);
        if let Some(pending) = pending_hold {
            pending.release();
        }
        added?;
        self.claim.finish_starting();
        Ok(())
    }

    pub fn release_holder(&self, session_id: &str) -> Result<(), WorktreeError> {
        self.claim.remove_holder(session_id)
    }

    #[must_use]
    pub fn holders(&self) -> BTreeSet<String> {
        self.claim.holders()
    }

    /// Deletes the record, for a caller that removed the worktree itself.
    pub fn forget(&self) {
        self.claim.delete();
    }

    /// Snapshots and removes the oldest inactive managed worktrees until at
    /// most `limit` remain, answering how many it removed.
    ///
    /// Oldest by claim time. A worktree being prepared or held is skipped and
    /// still counts against the limit, and one whose snapshot or removal fails
    /// is kept and logged (`vibe/core/git/worktree/repository.py:751-800`).
    pub fn prune(managed: &ManagedRoot, limit: usize) -> Result<usize, WorktreeError> {
        let _lock = PruneLock::acquire(managed)?;
        Self::reclaim_abandoned_reservations(managed);
        let mut claimed = WorktreeClaim::all(managed)
            .into_iter()
            .filter_map(|claim| {
                let record = claim.read()?;
                record.base_commit.as_ref()?;
                Some((record.claimed_at, claim))
            })
            .collect::<Vec<_>>();
        claimed.sort_by(|(left_at, left), (right_at, right)| {
            (left_at, &left.bucket, &left.name).cmp(&(right_at, &right.bucket, &right.name))
        });
        let mut excess = claimed.len().saturating_sub(limit);
        let mut removed = 0;
        for (_, claim) in claimed {
            if excess == 0 {
                break;
            }
            if claim.is_starting() {
                continue;
            }
            let bucket = claim.bucket.clone();
            let name = claim.name.clone();
            match Self::new(claim).prune_with_snapshot() {
                Ok(release) => {
                    if matches!(
                        release.outcome,
                        WorktreeReleaseOutcome::Removed | WorktreeReleaseOutcome::NotFound
                    ) {
                        excess -= 1;
                    }
                    if release.outcome == WorktreeReleaseOutcome::Removed {
                        removed += 1;
                    }
                }
                Err(error) => observability::log(
                    LogLevel::Warning,
                    &format!(
                        "Keeping managed worktree {bucket}/{name}: retention cleanup failed: {error}"
                    ),
                ),
            }
        }
        Ok(removed)
    }

    /// Settles reservations whose preparation died: a directory that became a
    /// worktree gets its base commit recorded, and an empty one is deleted with
    /// the branch it created (`vibe/core/git/worktree/repository.py:802-841`).
    fn reclaim_abandoned_reservations(managed: &ManagedRoot) {
        for claim in WorktreeClaim::all(managed) {
            let Some(record) = claim.read() else {
                continue;
            };
            if record.base_commit.is_some() || claim.is_starting() || !claim.holders().is_empty() {
                continue;
            }
            let target = Self::new(claim.clone()).root();
            if target.is_symlink() || (target.exists() && !target.is_dir()) {
                continue;
            }
            if target.join(".git").is_file() {
                let Ok(base_commit) = GitRepo::at(&target).and_then(|repo| repo.head_commit())
                else {
                    continue;
                };
                let mut completed = record.clone();
                completed.base_commit = Some(base_commit);
                let _ = claim.write(&completed);
                continue;
            }
            let is_empty = match fs::read_dir(&target) {
                Ok(mut entries) => entries.next().is_none(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => continue,
            };
            if !is_empty {
                continue;
            }
            claim.delete();
            let _ = fs::remove_dir(&target);
            if record.branch_created
                && let Ok(repo) = GitRepo::open(&record.repo_root)
            {
                let _ = repo.delete_branch(&record.branch, false);
            }
        }
    }

    /// Releases this worktree for `session_id`, removing it when nobody else
    /// holds it.
    ///
    /// [`None`] is a release by a caller that never held it, such as a delete
    /// arriving after the session closed; every other holder still has to be
    /// gone. A retained worktree's release discards its snapshot
    /// (`vibe/core/git/worktree/repository.py:848-882`).
    pub fn release(&self, session_id: Option<&str>) -> Result<WorktreeRelease, WorktreeError> {
        let Some(record) = self.claim.read() else {
            if session_id.is_none()
                && let Some(recovery) = self.claim.read_recovery()
            {
                self.discard_retained_snapshot(&recovery);
                return Ok(WorktreeRelease::outcome(WorktreeReleaseOutcome::Removed)
                    .with_branch(&recovery.branch));
            }
            return Ok(WorktreeRelease::outcome(
                WorktreeReleaseOutcome::KeptUnmanaged,
            ));
        };
        if self.root().is_dir() && self.claim.is_starting() {
            return Ok(WorktreeRelease::outcome(WorktreeReleaseOutcome::KeptInUse)
                .with_branch(&record.branch));
        }
        if let Some(session_id) = session_id {
            self.claim.remove_holder(session_id)?;
        }
        let remaining = self.claim.holders();
        if !remaining.is_empty() {
            observability::log(
                LogLevel::Debug,
                &format!(
                    "Keeping worktree {}: still held by {} session(s)",
                    self.claim.name,
                    remaining.len()
                ),
            );
            return Ok(WorktreeRelease::outcome(WorktreeReleaseOutcome::KeptInUse)
                .with_branch(&record.branch));
        }
        self.release_unheld(&record)
    }

    fn discard_retained_snapshot(&self, recovery: &WorktreeRecoveryRecord) {
        self.claim.delete_recovery();
        let deleted = GitRepo::at(&recovery.repo_root).and_then(|repo| {
            repo.checked(
                ["update-ref", "-d", recovery.snapshot_ref.as_str()],
                &recovery.name,
            )
        });
        if let Err(error) = deleted {
            observability::log(
                LogLevel::Warning,
                &format!(
                    "Failed to delete retained snapshot {}: {error}",
                    recovery.snapshot_ref
                ),
            );
        }
    }

    fn prepared_from(&self, record: &WorktreeRecord, base_commit: &str) -> PreparedWorktree {
        let root = self.root();
        PreparedWorktree {
            name: self.claim.name.clone(),
            branch: record.branch.clone(),
            root: root.clone(),
            path: root,
            repo_root: record.repo_root.clone(),
            base_commit: base_commit.to_owned(),
            created: true,
            branch_created: record.branch_created,
            pending_hold: None,
        }
    }

    /// Retention's removal: always snapshot, record how to recover, then
    /// remove (`vibe/core/git/worktree/repository.py:899-955`).
    fn prune_with_snapshot(&self) -> Result<WorktreeRelease, WorktreeError> {
        let Some(record) = self.claim.read() else {
            return Ok(WorktreeRelease::outcome(
                WorktreeReleaseOutcome::KeptUnmanaged,
            ));
        };
        if self.claim.is_starting() || !self.claim.holders().is_empty() {
            return Ok(WorktreeRelease::outcome(WorktreeReleaseOutcome::KeptInUse)
                .with_branch(&record.branch));
        }
        let root = self.root();
        if !root.is_dir() {
            self.claim.delete();
            return Ok(WorktreeRelease::outcome(WorktreeReleaseOutcome::NotFound));
        }
        let Some(base_commit) = record.base_commit.clone() else {
            return Ok(WorktreeRelease::outcome(
                WorktreeReleaseOutcome::KeptUnmanaged,
            ));
        };
        let prepared = self.prepared_from(&record, &base_commit);
        let snapshot = match prepared.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                observability::log(
                    LogLevel::Warning,
                    &format!(
                        "Keeping worktree {}: retention snapshot failed: {error}",
                        root.display()
                    ),
                );
                return Ok(WorktreeRelease::outcome(WorktreeReleaseOutcome::KeptDirty)
                    .with_root(&root)
                    .with_branch(&record.branch));
            }
        };
        if self.claim.is_starting() || !self.claim.holders().is_empty() {
            observability::log(
                LogLevel::Info,
                &format!(
                    "Keeping worktree {}: claimed during inspection",
                    root.display()
                ),
            );
            return Ok(WorktreeRelease::outcome(WorktreeReleaseOutcome::KeptInUse)
                .with_root(&root)
                .with_branch(&record.branch));
        }
        self.claim
            .write_recovery(&WorktreeRecoveryRecord::new(&record, &snapshot)?)?;
        prepared.remove(record.branch_created)?;
        self.claim.delete();
        observability::log(
            LogLevel::Info,
            &format!("Pruned worktree {} after saving {snapshot}", root.display()),
        );
        Ok(WorktreeRelease {
            outcome: WorktreeReleaseOutcome::Removed,
            root: Some(root),
            branch: Some(record.branch.clone()),
            branch_deleted: record.branch_created,
            reasons: Vec::new(),
            snapshot_ref: Some(snapshot),
        })
    }

    /// Puts back the retained worktree `cwd` sits in, answering whether it
    /// recreated one.
    ///
    /// `cwd` has to lie inside the worktree. A worktree that is still there is
    /// validated and its recovery record consumed; one that is gone is
    /// recreated from its snapshot (`vibe/core/git/worktree/repository.py:957-1009`).
    pub fn restore(&self, cwd: &Path) -> Result<bool, WorktreeError> {
        let requested = resolve_lenient(&crate::workspace::tool_path::expanded(cwd));
        let root = self.root();
        if !requested.starts_with(&root) {
            return Err(WorktreeError::failed(
                &self.claim.name,
                format!(
                    "path `{}` is outside worktree `{}`",
                    requested.display(),
                    root.display()
                ),
            ));
        }
        let _lock = PruneLock::acquire(self.claim.managed())?;
        self.restore_locked(&requested, &root)
    }

    fn restore_locked(&self, requested: &Path, root: &Path) -> Result<bool, WorktreeError> {
        let Some(recovery) = self.claim.read_recovery() else {
            return Ok(false);
        };
        let occupied = || {
            WorktreeError::failed(
                &self.claim.name,
                format!("cannot restore into occupied path `{}`", root.display()),
            )
        };
        if self.claim.is_starting() {
            return Err(WorktreeError::failed(
                &self.claim.name,
                format!("a restore is already in progress for `{}`", root.display()),
            ));
        }
        // A restore interrupted between its mkdir and its record leaves only
        // the empty reservation, which is removed so the restore can be retried.
        if !root.is_symlink()
            && root.is_dir()
            && fs::read_dir(root).is_ok_and(|mut entries| entries.next().is_none())
        {
            fs::remove_dir(root).map_err(|_| occupied())?;
        }
        if root.exists() {
            let record = self.claim.read().ok_or_else(occupied)?;
            if record.base_commit.is_none() {
                return Err(occupied());
            }
            let repository = WorktreeRepository::open(&record.repo_root, self.claim.managed())?;
            validate_existing_worktree(root, &record.branch, &repository.paths()?.common_git_dir)?;
            ensure_saved_directory(&self.claim.name, requested)?;
            self.claim.delete_recovery();
            return Ok(false);
        }
        let repository = WorktreeRepository::open(&recovery.repo_root, self.claim.managed())?;
        repository.restore(&self.claim, &recovery)?;
        if let Err(error) = ensure_saved_directory(&self.claim.name, requested) {
            self.claim.finish_starting();
            return Err(error);
        }
        self.claim.delete_recovery();
        observability::log(
            LogLevel::Info,
            &format!(
                "Restored retained worktree {} from {}",
                root.display(),
                recovery.snapshot_ref
            ),
        );
        Ok(true)
    }

    /// The removal a release performs once nobody holds the worktree.
    ///
    /// Work left behind is a reason to save it rather than to keep the
    /// directory, so a dirty worktree is snapshotted and removed, and only a
    /// snapshot that will not write keeps it. Holders are checked again just
    /// before the removal, which narrows the window a session in another
    /// process could join in (`vibe/core/git/worktree/repository.py:1011-1099`).
    fn release_unheld(&self, record: &WorktreeRecord) -> Result<WorktreeRelease, WorktreeError> {
        let root = self.root();
        if !root.is_dir() {
            self.claim.delete();
            return Ok(WorktreeRelease::outcome(WorktreeReleaseOutcome::NotFound));
        }
        let Some(base_commit) = record.base_commit.clone() else {
            return Ok(WorktreeRelease::outcome(
                WorktreeReleaseOutcome::KeptUnmanaged,
            ));
        };
        let prepared = self.prepared_from(record, &base_commit);
        let state = prepared.inspect_for_cleanup()?;
        let mut snapshot = None;
        if !state.is_clean() {
            match prepared.snapshot() {
                Ok(saved) => snapshot = Some(saved),
                Err(error) => {
                    observability::log(
                        LogLevel::Warning,
                        &format!(
                            "Keeping worktree {} on branch {}: {} could not be saved ({error})",
                            root.display(),
                            record.branch,
                            state.reasons().join(", ")
                        ),
                    );
                    return Ok(WorktreeRelease {
                        outcome: WorktreeReleaseOutcome::KeptDirty,
                        root: Some(root),
                        branch: Some(record.branch.clone()),
                        branch_deleted: false,
                        reasons: state.reasons(),
                        snapshot_ref: None,
                    });
                }
            }
        }
        let late = self.claim.holders();
        if !late.is_empty() {
            observability::log(
                LogLevel::Info,
                &format!(
                    "Keeping worktree {}: {} session(s) joined during inspection",
                    root.display(),
                    late.len()
                ),
            );
            return Ok(WorktreeRelease::outcome(WorktreeReleaseOutcome::KeptInUse)
                .with_root(&root)
                .with_branch(&record.branch));
        }
        prepared.remove(record.branch_created)?;
        self.claim.delete();
        if let Some(saved) = &snapshot {
            observability::log(
                LogLevel::Info,
                &format!(
                    "Removed worktree {}, which still had {}. Recover it with: git -C {} switch -c {} {saved}",
                    root.display(),
                    state.reasons().join(", "),
                    record.repo_root.display(),
                    self.claim.name
                ),
            );
        }
        Ok(WorktreeRelease {
            outcome: WorktreeReleaseOutcome::Removed,
            root: Some(root),
            branch: Some(record.branch.clone()),
            branch_deleted: record.branch_created,
            reasons: Vec::new(),
            snapshot_ref: snapshot,
        })
    }
}

/// Recreates a saved working directory git did not restore, since git stores
/// neither empty directories nor ignored-only trees
/// (`vibe/core/git/worktree/repository.py:1170-1180`).
fn ensure_saved_directory(name: &str, requested: &Path) -> Result<(), WorktreeError> {
    if requested.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(requested).map_err(|_| {
        WorktreeError::failed(
            name,
            format!(
                "the restored worktree does not contain the saved directory `{}`",
                requested.display()
            ),
        )
    })
}

fn random_hex() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = crate::clock::now_nanos();
        return format!("{nanos:032x}");
    }
    hex::encode(bytes)
}

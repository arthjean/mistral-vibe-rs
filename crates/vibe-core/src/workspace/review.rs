use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::edit::{EditOperation, apply_edit_operations};
use super::{MutationResult, Workspace, WorkspaceError, path_display, text_file, unified_diff};
use crate::checkpoints::{
    CheckpointError, CheckpointFiles, CheckpointRecorder, Checkpointer, Decision, FileAccessError,
    FileState, FileStore, Owner, RecorderError, ReviewError, ReviewHunk as AnchoredHunk,
    ReviewState, ReviewTarget, TurnFileDiff, decide_target, project_scope_diff, project_state,
    state_text,
};

const MAX_REWIND_SNAPSHOT_BYTES: usize = 64 * 1_048_576;

/// The paths one scope captured for rewind, and the bytes their first capture
/// read.
///
/// The bytes themselves live in the log; this keeps only their count, which is
/// what the rewind budget is measured in. A scope keeps the first capture of a
/// path, so capturing it again adds nothing.
#[derive(Default)]
struct CapturedBytes {
    paths: BTreeSet<PathBuf>,
    total: usize,
}

impl CapturedBytes {
    fn contains(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }

    fn remaining(&self) -> usize {
        MAX_REWIND_SNAPSHOT_BYTES.saturating_sub(self.total)
    }

    fn record(&mut self, path: &Path, bytes: usize) {
        if self.paths.insert(path.to_path_buf()) {
            self.total = self.total.saturating_add(bytes);
        }
    }
}

#[derive(Default)]
struct SnapshotState {
    /// What the running turn captured, or [`None`] between turns.
    active_turn: Option<CapturedBytes>,
    /// What the session captured since its message list was last reset.
    session: CapturedBytes,
    /// Set once the log refuses a capture for want of room, and taken by
    /// whoever publishes it. The refusal is not an error the tool that
    /// triggered it should fail on: the write landed, only its history did not.
    retention_notice: Option<String>,
}

/// The checkpoint engine's filesystem port: the disk itself, reached by the
/// path the log keys a file by.
///
/// Reference `DiskFilesystem` (`vibe/core/checkpoints/fs.py`). Every file a
/// tool snapshots is keyed by its resolved absolute path, and a review client
/// may name any other path; a relative one resolves against the process's
/// working directory, as the reference's `Path(path)` does. Absence is
/// `NotFound` or `NotADirectory` and nothing else, so a file that is there but
/// will not open is never recorded as a deletion. A write lands in place and
/// creates the directories it needs, which is what lets a revert restore a file
/// whose directory was removed after it.
#[derive(Debug, Clone, Copy, Default)]
struct DiskFiles;

impl CheckpointFiles for DiskFiles {
    fn read_bytes(&self, path: &str) -> Result<Option<Vec<u8>>, FileAccessError> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(FileAccessError::new("reading", path, error)),
        }
    }

    fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), FileAccessError> {
        let target = Path::new(path);
        if let Some(parent) = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| FileAccessError::new("writing", path, error))?;
        }
        std::fs::write(target, data).map_err(|error| FileAccessError::new("writing", path, error))
    }

    fn remove(&self, path: &str) -> Result<(), FileAccessError> {
        std::fs::remove_file(path).map_err(|error| FileAccessError::new("deleting", path, error))
    }

    fn exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }
}

pub struct ReviewManager {
    workspace: Arc<Workspace>,
    state: Mutex<SnapshotState>,
    /// The checkpoint engine's log, driven by the turn boundaries the state
    /// beside it tracks: the state answers whether a turn runs and how much it
    /// may still capture, and the log answers what each turn and each hand edit
    /// changed and what a rewind restores.
    log: Mutex<Checkpointer>,
    recorder: CheckpointRecorder<DiskFiles>,
}

impl ReviewManager {
    #[must_use]
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self {
            recorder: CheckpointRecorder::new(FileStore::new(DiskFiles)),
            workspace,
            state: Mutex::new(SnapshotState::default()),
            log: Mutex::new(Checkpointer::new()),
        }
    }

    /// Runs `body` against the checkpoint log.
    ///
    /// # Errors
    ///
    /// Fails when another thread poisoned the log.
    pub fn with_log<T>(
        &self,
        body: impl FnOnce(&mut Checkpointer) -> T,
    ) -> Result<T, WorkspaceError> {
        let mut log = self.log.lock().map_err(|_| WorkspaceError::LockPoisoned {
            surface: "checkpoint log",
        })?;
        Ok(body(&mut log))
    }

    // -- The review surface ---------------------------------------------------
    //
    // Six answers over one log, each the reference's `ReviewManager`
    // (`vibe/core/review/manager.py`) step for step. A path is the log's key as
    // the client sent it: the absolute path every answer publishes, or any
    // other spelling, which the log does not know and the disk resolves on its
    // own terms. Reading and deciding are acting boundaries: a hand edit made
    // since the last look is folded in as its own slot first, so it is never
    // attributed to the next turn and never clobbered by a decision. While a
    // turn runs, the reads see its unsealed changes through
    // `Checkpointer::view`.

    /// Every file with something left to review, and every owner's slot.
    ///
    /// # Errors
    ///
    /// Fails when a tracked path exists but cannot be read, and when another
    /// thread poisoned the log.
    pub fn review_state(&self) -> Result<ReviewState, ReviewError> {
        let tracked = self.with_review_log(|log| log.history().tracked_paths())?;
        let current = self.read_states(&tracked)?;
        let (state, refused) = self.with_review_log(|log| {
            let mut refused = None;
            for (path, state) in &current {
                reconcile_tracked(log, path, state.clone(), &mut refused);
            }
            let view = log.view(&current);
            (
                project_state(&view.history(), &log.history(), &current),
                refused,
            )
        })?;
        self.note_retention(refused);
        Ok(state)
    }

    /// Accepts what `target` names into the accepted baseline, answering the
    /// paths it touched.
    ///
    /// Disk is left alone: approved content is what already sits there.
    ///
    /// # Errors
    ///
    /// Fails when the log refuses the decision and when a path cannot be read.
    pub fn approve_review(&self, target: &ReviewTarget) -> Result<Vec<String>, ReviewError> {
        self.decide_review(target, Decision::Keep)
    }

    /// Rolls back what `target` names, writing each affected file, and answers
    /// the paths it touched.
    ///
    /// # Errors
    ///
    /// Fails when the log refuses the decision, when a path cannot be read, and
    /// when a write does not land, in which case the log is rolled back so the
    /// decision is never committed against a file that did not change.
    pub fn revert_review(&self, target: &ReviewTarget) -> Result<Vec<String>, ReviewError> {
        self.decide_review(target, Decision::Revert)
    }

    /// The decoded text of the accepted baseline of `path`: its kept regions
    /// and nothing else.
    ///
    /// A path with no kept region answers its original text, and a baseline that
    /// is absent answers an empty string, which is what a panel renders as an
    /// empty left-hand side.
    ///
    /// # Errors
    ///
    /// Fails when the path exists but cannot be read, and when another thread
    /// poisoned the log.
    pub fn baseline_text(&self, path: &str) -> Result<String, ReviewError> {
        let current = [(path.to_owned(), self.recorder.files().read(path)?)];
        self.with_review_log(|log| {
            state_text(&log.view(&current).history().accepted_baseline(path))
        })
    }

    /// One owner's own change to `path`: its kept regions against its kept plus
    /// pending ones.
    ///
    /// # Errors
    ///
    /// Fails when the path exists but cannot be read, and when another thread
    /// poisoned the log.
    pub fn scope_file_diff(&self, path: &str, owner: Owner) -> Result<TurnFileDiff, ReviewError> {
        let current = [(path.to_owned(), self.recorder.files().read(path)?)];
        self.with_review_log(|log| project_scope_diff(&log.view(&current).history(), path, owner))
    }

    /// Every pending change of `path`, located in a rendered diff.
    ///
    /// With no owner the diff is the accepted baseline against what belongs on
    /// disk, which is what a whole-file panel renders; with one it is that
    /// owner's own scope diff. Reconciling first is what keeps the anchors in
    /// line with the disk the panel renders, and it applies to a path the log
    /// never tracked too: its content becomes a hand edit of its own.
    ///
    /// # Errors
    ///
    /// Fails when the path exists but cannot be read, and when another thread
    /// poisoned the log.
    pub fn file_hunks(
        &self,
        path: &str,
        owner: Option<Owner>,
    ) -> Result<Vec<AnchoredHunk>, ReviewError> {
        let current = [(path.to_owned(), self.recorder.files().read(path)?)];
        let (hunks, refused) = self.with_review_log(|log| {
            let mut refused = None;
            reconcile_tracked(log, path, current[0].1.clone(), &mut refused);
            let hunks = log
                .view(&current)
                .history()
                .pending_hunks(path, owner)
                .into_iter()
                .map(|anchor| AnchoredHunk {
                    side: anchor.side,
                    line: anchor.line,
                    regions: anchor.regions,
                })
                .collect();
            (hunks, refused)
        })?;
        self.note_retention(refused);
        Ok(hunks)
    }

    /// Reconciles, decides and persists, with the log rolled back when a write
    /// fails.
    fn decide_review(
        &self,
        target: &ReviewTarget,
        decision: Decision,
    ) -> Result<Vec<String>, ReviewError> {
        let tracked = self.with_review_log(|log| log.history().tracked_paths())?;
        let current = self.read_states(&tracked)?;
        let mut refused = None;
        let decided = self.with_review_log(|log| {
            for (path, state) in &current {
                reconcile_tracked(log, path, state.clone(), &mut refused);
            }
            log.atomic(|log| {
                let mut affected: Vec<String> = Vec::new();
                for path in decide_target(log, target, decision)? {
                    if !affected.contains(&path) {
                        affected.push(path);
                    }
                }
                for path in &affected {
                    self.persist(log, path)?;
                }
                Ok(affected)
            })
        })?;
        self.note_retention(refused);
        decided
    }

    /// Keeps the log's refusal for want of room where a caller can publish it.
    ///
    /// A poisoned state lock loses the notice rather than failing the read that
    /// carried it: the notice is a warning about history, and the answer the
    /// caller asked for is still correct without it.
    fn note_retention(&self, refused: Option<String>) {
        let Some(refused) = refused else {
            return;
        };
        if let Ok(mut state) = self.state.lock() {
            state.retention_notice.get_or_insert(refused);
        }
    }

    /// Writes `path` to what the log now projects, unless disk already holds it.
    ///
    /// A path the log never tracked projects to nothing, so deciding one that
    /// exists deletes it, as the reference's `_persist` does.
    fn persist(&self, log: &Checkpointer, path: &str) -> Result<(), ReviewError> {
        let files = self.recorder.files();
        let current = files.read(path)?;
        let content = log.history().content(path);
        if content == current {
            return Ok(());
        }
        match files
            .apply(&[(path.to_owned(), content)])
            .errors
            .into_iter()
            .next()
        {
            Some(error) => Err(ReviewError::Persist(error)),
            None => Ok(()),
        }
    }

    /// What every path of `paths` holds now, in the order given.
    fn read_states(&self, paths: &[String]) -> Result<Vec<(String, FileState)>, ReviewError> {
        let files = self.recorder.files();
        paths
            .iter()
            .map(|path| Ok((path.clone(), files.read(path)?)))
            .collect()
    }

    /// Runs `body` against the log, in the review surface's error vocabulary.
    fn with_review_log<T>(
        &self,
        body: impl FnOnce(&mut Checkpointer) -> T,
    ) -> Result<T, ReviewError> {
        let mut log = self.log.lock().map_err(|_| ReviewError::LockPoisoned)?;
        Ok(body(&mut log))
    }

    /// The key the log files a located path under: its resolved absolute
    /// path, which is what the reference's `get_file_snapshot_for_path`
    /// (`vibe/core/tools/base.py:466-478`) records and what every review answer
    /// publishes.
    fn log_key(&self, located: &Path) -> String {
        let absolute = if located.is_absolute() {
            located.to_path_buf()
        } else {
            self.workspace.canonical_root.join(located)
        };
        let absolute = if absolute.ends_with(".") {
            absolute
                .parent()
                .map_or_else(|| absolute.clone(), Path::to_path_buf)
        } else {
            absolute
        };
        absolute.to_string_lossy().into_owned()
    }

    pub fn begin_turn_at(&self, message_index: usize) -> Result<(), WorkspaceError> {
        let mut state = self.lock_state()?;
        if state.active_turn.is_some() {
            return Err(WorkspaceError::ReviewBusy);
        }
        // The engine numbers its turns by the message index, which is what a
        // rewind addresses and what a restore plan is cut at.
        let engine_turn =
            u64::try_from(message_index).map_err(|_| WorkspaceError::LimitOverflow)?;
        self.with_log(|log| self.recorder.create_checkpoint(log, engine_turn))?
            .map_err(|error| WorkspaceError::Checkpoint(error.to_string()))?;
        state.active_turn = Some(CapturedBytes::default());
        Ok(())
    }

    pub fn write(
        &self,
        path: impl AsRef<Path>,
        content: impl AsRef<[u8]>,
    ) -> Result<MutationResult, WorkspaceError> {
        let relative = self.workspace.located(path.as_ref(), false)?;
        self.capture_baseline(&relative)?;
        self.workspace.write_new(&relative, content.as_ref())
    }

    /// Applies one edit, preserving what the file was written in.
    ///
    /// The read, the replacement and the write-back sit inside one per-path
    /// lock, so two turns editing the same file serialize instead of both
    /// reading the same original and one of the two writes disappearing. The
    /// codec and the line ending come back out of the decode and go into the
    /// encode, which is what keeps a one-line change to a CRLF file written in
    /// a single-byte codec from rewriting the whole file.
    pub fn edit(
        &self,
        path: impl AsRef<Path>,
        operations: &[EditOperation],
    ) -> Result<MutationResult, WorkspaceError> {
        let relative = self.workspace.located(path.as_ref(), true)?;
        let lock = self.workspace.write_lock(&relative)?;
        let _guard = lock.lock().map_err(|_| WorkspaceError::LockPoisoned {
            surface: "file write locks",
        })?;
        self.capture_baseline(&relative)?;
        let original = self.workspace.read_raw(&relative)?;
        if original.contains(&0) {
            return Err(WorkspaceError::Binary(relative.clone()));
        }
        let decoded = text_file::decode(&original);
        let edited = apply_edit_operations(&relative, &decoded.text, operations)?;
        let updated = edited.text;
        let bytes = text_file::encode(&updated, decoded.encoding, decoded.newline);
        if bytes != original {
            self.workspace.atomic_replace(&relative, &bytes)?;
        }
        Ok(MutationResult {
            path: path_display(&relative),
            bytes_written: bytes.len(),
            files_changed: 1,
            diff: unified_diff(&decoded.text, &updated),
            occurrences: edited.occurrences,
        })
    }

    pub fn seal_turn(&self) -> Result<(), WorkspaceError> {
        let mut state = self.lock_state()?;
        if state.active_turn.is_none() {
            return Err(WorkspaceError::NoActiveTurn);
        }
        // A path that went unreadable while the turn ran keeps its change out
        // of the log rather than sealing it as unchanged; the turn closes
        // either way.
        let _unreadable = self.with_log(|log| self.recorder.seal_turn(log))?;
        state.active_turn = None;
        Ok(())
    }

    /// The paths a rewind to the turn numbered `message_index` would change on
    /// disk, in the order the log names them.
    ///
    /// A message index the log carries no turn for restores nothing, which is
    /// what a rewind into a conversation this process never ran answers.
    ///
    /// # Errors
    ///
    /// Fails when a planned path exists but cannot be read, and when another
    /// thread poisoned the log.
    pub fn restorable_paths_at(&self, message_index: usize) -> Result<Vec<String>, WorkspaceError> {
        let plan = self.restoration_plan(message_index)?;
        let files = self.recorder.files();
        let mut paths = Vec::new();
        for (path, target) in plan {
            let current = files
                .read(&path)
                .map_err(|error| WorkspaceError::Checkpoint(error.to_string()))?;
            if current != target {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    /// Applies the restore plan for `message_index`, keeping what was there so
    /// the caller can put it back.
    ///
    /// Every path is attempted: one that could not be written is reported and
    /// the rest still land, because a plan that stopped halfway would leave the
    /// workspace in a state no version of it ever had. That is the reference's
    /// rule, and it is why the failures come back beside the transaction rather
    /// than instead of it.
    ///
    /// # Errors
    ///
    /// Fails when a planned path exists but cannot be read, and when another
    /// thread poisoned the log.
    pub fn stage_restore_to_message(
        &self,
        message_index: usize,
    ) -> Result<StagedRestore, WorkspaceError> {
        let plan = self.restoration_plan(message_index)?;
        let files = self.recorder.files();
        let mut previous = Vec::new();
        for (path, _target) in &plan {
            let state = files
                .read(path)
                .map_err(|error| WorkspaceError::Checkpoint(error.to_string()))?;
            previous.push((path.clone(), state));
        }
        let outcome = files.apply(&plan);
        Ok(StagedRestore {
            errors: outcome
                .errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            transaction: RestoreTransaction {
                files: self.recorder.files().clone(),
                previous: Some(previous),
                changed_paths: outcome.restored,
            },
        })
    }

    /// Forgets every turn from the one numbered `message_index` on, which is
    /// what a rewind does to the log it keeps.
    ///
    /// Reference `RewindManager._truncate` drops those turns from the shared
    /// checkpointer rather than clearing it, so a later rewind can still reach
    /// an earlier turn and restore its files.
    ///
    /// # Errors
    ///
    /// Fails when another thread poisoned the log or the review state.
    pub fn drop_turns_from(&self, message_index: usize) -> Result<(), WorkspaceError> {
        self.lock_state()?.active_turn = None;
        self.with_log(|log| {
            if let Ok(turn) = u64::try_from(message_index) {
                log.drop_turns_from(turn);
            }
        })
    }

    /// Clears the log because the message list it is numbered against was
    /// reset, reopening a turn at `message_index` when one was running.
    ///
    /// A clear or a compaction invalidates every turn identifier in the log,
    /// since each one is a position in the list that just changed. Reopening
    /// matters for a compaction that happens mid-turn: the tool loop keeps
    /// running afterward, and without a turn to record against its remaining
    /// writes would be refused. A rewind does not come through here; its own
    /// truncation owns the log.
    ///
    /// # Errors
    ///
    /// Fails when the message index does not fit a turn identifier and when
    /// another thread poisoned the log.
    pub fn reset_messages(&self, message_index: usize) -> Result<(), WorkspaceError> {
        let turn_id = u64::try_from(message_index).map_err(|_| WorkspaceError::LimitOverflow)?;
        let mut state = self.lock_state()?;
        let was_open = state.active_turn.take().is_some();
        state.session = CapturedBytes::default();
        drop(state);
        self.with_log(|log| {
            let reopen = log.has_open_turn();
            log.clear();
            if reopen {
                log.begin_turn(turn_id)
            } else {
                Ok(())
            }
        })?
        .map_err(|error| WorkspaceError::Checkpoint(error.to_string()))?;
        if was_open {
            self.lock_state()?.active_turn = Some(CapturedBytes::default());
        }
        Ok(())
    }

    /// The warning the log raised when it ran out of room, taken once.
    ///
    /// # Errors
    ///
    /// Fails when another thread poisoned the review state.
    pub fn take_retention_notice(&self) -> Result<Option<String>, WorkspaceError> {
        Ok(self.lock_state()?.retention_notice.take())
    }

    /// What each file a rewind to `message_index` touches should hold once the
    /// cut lands, straight from the log.
    fn restoration_plan(
        &self,
        message_index: usize,
    ) -> Result<Vec<(String, FileState)>, WorkspaceError> {
        let Ok(turn_id) = u64::try_from(message_index) else {
            return Ok(Vec::new());
        };
        self.with_log(|log| log.history().restore_plan_to_turn(turn_id))
    }

    /// Records `path` as it stands before a tool that may change it runs.
    ///
    /// The reference snapshots the file ahead of the tool's own checks, so a
    /// write refused for an existing file, or one a client performs, still
    /// leaves the file tracked for review. A path outside the workspace, or a
    /// call outside a turn, has nothing to record. Reference `_invoke_tool`
    /// (`vibe/core/agent_loop/_loop.py`) through `get_file_snapshot`.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or the log refuses the capture.
    pub fn capture_before_mutation(&self, path: impl AsRef<Path>) -> Result<(), WorkspaceError> {
        let Ok(relative) = self.workspace.located(path.as_ref(), false) else {
            return Ok(());
        };
        match self.capture_baseline(&relative) {
            Err(WorkspaceError::NoActiveTurn) => Ok(()),
            other => other,
        }
    }

    fn capture_baseline(&self, relative: &Path) -> Result<(), WorkspaceError> {
        let mut state = self.lock_state()?;
        let Some(turn) = state.active_turn.as_ref() else {
            return Err(WorkspaceError::NoActiveTurn);
        };
        if turn.contains(relative) {
            return Ok(());
        }
        // A path the session already holds adds nothing to the session's
        // total, so only the turn's own budget bounds it.
        let session_remaining = if state.session.contains(relative) {
            MAX_REWIND_SNAPSHOT_BYTES
        } else {
            state.session.remaining()
        };
        let snapshot_limit = session_remaining.min(turn.remaining());
        let baseline = self
            .workspace
            .exists(relative)
            .then(|| self.workspace.read_raw_bounded(relative, snapshot_limit))
            .transpose()?;
        let captured = baseline.as_ref().map_or(0, Vec::len);
        // The tool has read the file and is about to write it, so this is the
        // state its change is computed against. Handing the bytes already read
        // to the engine keeps the two readings from disagreeing about what was
        // there.
        let snapshot = baseline.map_or_else(FileState::absent, FileState::from_bytes);
        if let Err(refusal) = self.with_log(|log| {
            self.recorder
                .add_snapshot(log, &self.log_key(relative), snapshot)
        })? {
            // A log with no room left refuses the capture rather than dropping
            // an earlier event to make space, and the write itself still
            // happens: the file changes, only its history stops being tracked.
            // Anything else the log refused is a state error the caller has to
            // see.
            if !matches!(
                refusal,
                RecorderError::Log(CheckpointError::RetentionExhausted { .. })
            ) {
                return Err(WorkspaceError::Checkpoint(refusal.to_string()));
            }
            state
                .retention_notice
                .get_or_insert_with(|| refusal.to_string());
        }
        state.session.record(relative, captured);
        if let Some(turn) = state.active_turn.as_mut() {
            turn.record(relative, captured);
        }
        Ok(())
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, SnapshotState>, WorkspaceError> {
        self.state
            .lock()
            .map_err(|_| WorkspaceError::LockPoisoned { surface: "review" })
    }
}

/// A restore that landed, with what it could not write.
///
/// The two travel together because a partial failure does not undo the rest:
/// the caller reports the failures and still owns the decision to keep or roll
/// back what did land.
pub struct StagedRestore {
    /// The applied restore, uncommitted.
    pub transaction: RestoreTransaction,
    /// One entry per path the plan could not put in the state it asked for.
    pub errors: Vec<String>,
}

pub struct RestoreTransaction {
    files: FileStore<DiskFiles>,
    previous: Option<Vec<(String, FileState)>>,
    changed_paths: Vec<String>,
}

impl RestoreTransaction {
    #[must_use]
    pub fn commit(mut self) -> Vec<String> {
        self.previous = None;
        std::mem::take(&mut self.changed_paths)
    }

    pub fn rollback(mut self) -> Result<(), WorkspaceError> {
        let Some(previous) = self.previous.take() else {
            return Ok(());
        };
        match self.files.apply(&previous).errors.first() {
            Some(error) => Err(WorkspaceError::Checkpoint(error.to_string())),
            None => Ok(()),
        }
    }
}

impl Drop for RestoreTransaction {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            let _ = self.files.apply(&previous);
        }
    }
}

/// Folds `state` into `log`, keeping a refusal for want of room in `refused`.
///
/// A refusal is not a failure of the read it precedes: the drift stays on disk
/// and stays unattributed, and the surface answers what the log does carry.
fn reconcile_tracked(
    log: &mut Checkpointer,
    path: &str,
    state: FileState,
    refused: &mut Option<String>,
) {
    if let Err(error) = log.reconcile(path, state) {
        refused.get_or_insert_with(|| error.to_string());
    }
}

#[cfg(test)]
mod review_tests;

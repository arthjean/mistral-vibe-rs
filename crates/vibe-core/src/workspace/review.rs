use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{
    Checkpoint, EditOccurrence, EditOperation, MutationResult, ReviewHunk, ReviewView, Workspace,
    WorkspaceError, path_display, text_file, unified_diff,
};
use crate::checkpoints::{
    CheckpointError, CheckpointFiles, CheckpointRecorder, Checkpointer, Decision, FileAccessError,
    FileState, FileStore, Owner, RecorderError, ReviewError, ReviewHunk as AnchoredHunk,
    ReviewState, ReviewTarget, TurnFileDiff, decide_target, project_scope_diff, project_state,
    state_text,
};

/// Applies one edit's operations to the text it was written against.
///
/// The rules are the file's, not the storage's, which is what lets an edit whose
/// content came from a client-owned buffer refuse a stale or ambiguous needle on
/// the same terms as one read from disk.
pub(crate) fn apply_edit_operations(
    path: &Path,
    original: &str,
    operations: &[EditOperation],
) -> Result<Edited, WorkspaceError> {
    let mut updated = original.to_owned();
    let mut occurrences = Vec::new();
    for operation in operations {
        let matches = updated.matches(&operation.old_text).count();
        if matches == 0 {
            return Err(WorkspaceError::StaleEdit {
                path: path.to_path_buf(),
                needle: operation.old_text.clone(),
            });
        }
        if matches > 1 && !operation.replace_all {
            return Err(WorkspaceError::AmbiguousEdit {
                path: path.to_path_buf(),
                matches,
            });
        }
        // The contexts are read off the text as it stands before this
        // operation, so a start line names the file the operator was looking
        // at rather than the one the replacement produced.
        let contexts = line_contexts(&updated, &operation.old_text);
        let kept = if operation.replace_all {
            contexts.as_slice()
        } else {
            &contexts[..contexts.len().min(1)]
        };
        occurrences.extend(
            kept.iter()
                .map(|(start_line, prefix, suffix)| EditOccurrence {
                    start_line: *start_line,
                    old_text: format!("{prefix}{}{suffix}", operation.old_text),
                    new_text: format!("{prefix}{}{suffix}", operation.new_text),
                }),
        );
        updated = if operation.replace_all {
            updated.replace(&operation.old_text, &operation.new_text)
        } else {
            updated.replacen(&operation.old_text, &operation.new_text, 1)
        };
    }
    Ok(Edited {
        text: updated,
        occurrences,
    })
}

/// The text one edit produced and the replacements it performed.
pub(crate) struct Edited {
    pub(crate) text: String,
    pub(crate) occurrences: Vec<EditOccurrence>,
}

/// Every occurrence of `snippet` in `content`, as the whole lines it sits on.
///
/// Reference `line_contexts`: each hit is reported as its one-based start line
/// and the two halves of the lines around it, so a caller can widen the anchor
/// to whole lines without searching the file again. A snippet made of newlines
/// alone anchors nothing and reports nothing.
fn line_contexts(content: &str, snippet: &str) -> Vec<(usize, String, String)> {
    if snippet.trim_matches('\n').is_empty() {
        return Vec::new();
    }
    let mut contexts = Vec::new();
    let mut from = 0;
    while let Some(offset) = content[from..].find(snippet) {
        let start = from + offset;
        let end = start + snippet.len();
        let line_start = content[..start].rfind('\n').map_or(0, |index| index + 1);
        let suffix = if content[..end].ends_with('\n') {
            ""
        } else {
            let line_end = content[end..]
                .find('\n')
                .map_or(content.len(), |index| end + index);
            &content[end..line_end]
        };
        contexts.push((
            content[..start].matches('\n').count() + 1,
            content[line_start..start].to_owned(),
            suffix.to_owned(),
        ));
        from = end;
    }
    contexts
}

const MAX_REWIND_SNAPSHOT_BYTES: usize = 64 * 1_048_576;

#[derive(Clone)]
struct ActiveReviewTurn {
    turn_id: String,
    baseline: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

#[derive(Default)]
struct SnapshotState {
    active_turn: Option<ActiveReviewTurn>,
    baseline: BTreeMap<PathBuf, Option<Vec<u8>>>,
    /// Set once the log refuses a capture for want of room, and taken by
    /// whoever publishes it. The refusal is not an error the tool that
    /// triggered it should fail on: the write landed, only its history did not.
    retention_notice: Option<String>,
}

/// The checkpoint engine's filesystem port, backed by the workspace.
///
/// Every path arrives in the display form the engine keys its log by and is
/// confined before it is touched, so the engine cannot address anything the
/// rest of the workspace would refuse. A path that resolves outside the root
/// fails rather than reading as absent, because the engine would otherwise
/// record a deletion for a file it was never allowed to look at.
#[derive(Clone)]
struct WorkspaceFiles {
    workspace: Arc<Workspace>,
}

impl WorkspaceFiles {
    fn confined(&self, path: &str, operation: &'static str) -> Result<PathBuf, FileAccessError> {
        self.workspace
            .confined(Path::new(path), false)
            .map_err(|error| FileAccessError::new(operation, path, error))
    }
}

impl CheckpointFiles for WorkspaceFiles {
    fn read_bytes(&self, path: &str) -> Result<Option<Vec<u8>>, FileAccessError> {
        let relative = self.confined(path, "reading")?;
        // One read, classified by what it failed with, rather than a presence
        // check followed by a read: the check answers for a moment that has
        // passed by the time the read lands, and a file deleted in between
        // would fail the turn it was carried into. These two kinds are what
        // absence means; anything else is a file that is there and would not
        // open, which must never be recorded as a deletion.
        match self.workspace.read_raw(&relative) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(WorkspaceError::Io { source, .. })
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(FileAccessError::new("reading", path, error)),
        }
    }

    fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), FileAccessError> {
        let relative = self.confined(path, "writing")?;
        self.workspace
            .atomic_replace(&relative, data)
            .map_err(|error| FileAccessError::new("writing", path, error))
    }

    fn remove(&self, path: &str) -> Result<(), FileAccessError> {
        let relative = self.confined(path, "deleting")?;
        self.workspace
            .remove(&relative)
            .map_err(|error| FileAccessError::new("deleting", path, error))
    }

    fn exists(&self, path: &str) -> bool {
        self.confined(path, "reading")
            .is_ok_and(|relative| self.workspace.exists(&relative))
    }
}

pub struct ReviewManager {
    workspace: Arc<Workspace>,
    state: Mutex<SnapshotState>,
    /// The checkpoint engine's log, driven by the same turn boundaries as the
    /// snapshot state beside it. The two are captured together because they
    /// answer different questions: the snapshots answer what to restore on a
    /// rewind, and the log answers which region of which file each turn and
    /// each hand edit produced.
    log: Mutex<Checkpointer>,
    recorder: CheckpointRecorder<WorkspaceFiles>,
}

impl ReviewManager {
    #[must_use]
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self {
            recorder: CheckpointRecorder::new(FileStore::new(WorkspaceFiles {
                workspace: workspace.clone(),
            })),
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
    // Six answers over one log. Every one of them reads disk before it reads the
    // log, because a hand edit made since the last look is a change the log does
    // not carry yet, and projecting or deciding without folding it in would
    // attribute it to whichever turn touched the file next. The reference draws
    // the same boundary in `vibe/core/review/manager.py`: rendering and deciding
    // are both acting boundaries.

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
            (project_state(&log.history(), &current), refused)
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
    /// Fails when another thread poisoned the log. The file is not read: the
    /// baseline is the log's answer, not disk's.
    pub fn baseline_text(&self, path: &str) -> Result<String, ReviewError> {
        self.with_review_log(|log| state_text(&log.history().accepted_baseline(path)))
    }

    /// One owner's own change to `path`: its kept regions against its kept plus
    /// pending ones.
    ///
    /// # Errors
    ///
    /// Fails when another thread poisoned the log.
    pub fn scope_file_diff(&self, path: &str, owner: Owner) -> Result<TurnFileDiff, ReviewError> {
        self.with_review_log(|log| project_scope_diff(&log.history(), path, owner))
    }

    /// Every pending change of `path`, located in a rendered diff.
    ///
    /// With no owner the diff is the accepted baseline against what belongs on
    /// disk, which is what a whole-file panel renders; with one it is that
    /// owner's own scope diff.
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
        let current = self.recorder.files().read(path)?;
        let (hunks, refused) = self.with_review_log(|log| {
            let mut refused = None;
            reconcile_tracked(log, path, current, &mut refused);
            let hunks = log
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
            Some(error) => Err(ReviewError::File(error)),
            None => Ok(()),
        }
    }

    /// What every path of `paths` holds now.
    fn read_states(&self, paths: &[String]) -> Result<BTreeMap<String, FileState>, ReviewError> {
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

    pub fn begin_turn(&self, turn_id: impl Into<String>) -> Result<(), WorkspaceError> {
        self.begin_turn_at(turn_id, 0)
    }

    pub fn begin_turn_at(
        &self,
        turn_id: impl Into<String>,
        message_index: usize,
    ) -> Result<(), WorkspaceError> {
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
        state.active_turn = Some(ActiveReviewTurn {
            turn_id: turn_id.into(),
            baseline: BTreeMap::new(),
        });
        Ok(())
    }

    pub fn write(
        &self,
        path: impl AsRef<Path>,
        content: impl AsRef<[u8]>,
    ) -> Result<MutationResult, WorkspaceError> {
        let relative = self.workspace.confined(path.as_ref(), false)?;
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
        let relative = self.workspace.confined(path.as_ref(), true)?;
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

    pub fn seal_turn(&self) -> Result<Checkpoint, WorkspaceError> {
        let mut state = self.lock_state()?;
        let active = state
            .active_turn
            .as_ref()
            .ok_or(WorkspaceError::NoActiveTurn)?;
        // A path that went unreadable while the turn ran keeps its change out
        // of the log rather than sealing it as unchanged; the turn closes
        // either way.
        let _unreadable = self.with_log(|log| self.recorder.seal_turn(log))?;
        let hunks = reconcile_hunks(&self.workspace, &active.baseline)?;
        let active = state
            .active_turn
            .take()
            .ok_or(WorkspaceError::NoActiveTurn)?;
        Ok(Checkpoint {
            turn_id: active.turn_id,
            hunks,
        })
    }

    pub fn view(&self) -> Result<ReviewView, WorkspaceError> {
        let state = self.lock_state()?;
        Ok(ReviewView {
            active_turn: state
                .active_turn
                .as_ref()
                .map(|active| active.turn_id.clone()),
            pending_hunks: reconcile_hunks(&self.workspace, &state.baseline)?,
        })
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

    pub fn fork_at(&self, message_index: usize) -> Result<Self, WorkspaceError> {
        let state = self.lock_state()?;
        let forked_log = self.with_log(|log| {
            let mut forked = log.clone();
            if let Ok(turn) = u64::try_from(message_index) {
                forked.drop_turns_from(turn);
            }
            forked
        })?;
        Ok(Self {
            workspace: self.workspace.clone(),
            state: Mutex::new(SnapshotState {
                active_turn: None,
                baseline: state.baseline.clone(),
                retention_notice: None,
            }),
            log: Mutex::new(forked_log),
            recorder: self.recorder.clone(),
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
        let was_open = state.active_turn.take();
        state.baseline.clear();
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
        if let Some(active) = was_open {
            let mut state = self.lock_state()?;
            state.active_turn = Some(ActiveReviewTurn {
                turn_id: active.turn_id,
                baseline: BTreeMap::new(),
            });
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

    pub fn approve(&self) -> Result<ReviewView, WorkspaceError> {
        let mut state = self.lock_state()?;
        if state.active_turn.is_some() {
            return Err(WorkspaceError::ReviewBusy);
        }
        clear_review(&mut state);
        self.with_log(Checkpointer::clear)?;
        drop(state);
        self.view()
    }

    pub fn revert(&self) -> Result<ReviewView, WorkspaceError> {
        let mut state = self.lock_state()?;
        if state.active_turn.is_some() {
            return Err(WorkspaceError::ReviewBusy);
        }
        apply_file_states_atomic(&self.workspace, &state.baseline)?;
        clear_review(&mut state);
        self.with_log(Checkpointer::clear)?;
        drop(state);
        self.view()
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

    fn capture_baseline(&self, relative: &Path) -> Result<(), WorkspaceError> {
        let mut state = self.lock_state()?;
        let Some(active) = state.active_turn.as_ref() else {
            return Err(WorkspaceError::NoActiveTurn);
        };
        if active.baseline.contains_key(relative) {
            return Ok(());
        }
        let baseline_bytes = snapshot_bytes(&state.baseline);
        let active_bytes = snapshot_bytes(&active.baseline);
        let global_remaining = if state.baseline.contains_key(relative) {
            MAX_REWIND_SNAPSHOT_BYTES
        } else {
            MAX_REWIND_SNAPSHOT_BYTES.saturating_sub(baseline_bytes)
        };
        let snapshot_limit =
            global_remaining.min(MAX_REWIND_SNAPSHOT_BYTES.saturating_sub(active_bytes));
        let baseline = current_file_state_bounded(&self.workspace, relative, snapshot_limit)?;
        // The tool has read the file and is about to write it, so this is the
        // state its change is computed against. Handing the bytes already read
        // to the engine keeps the two captures from disagreeing about what was
        // there.
        let snapshot = baseline
            .clone()
            .map_or_else(FileState::absent, FileState::from_bytes);
        if let Err(refusal) = self.with_log(|log| {
            self.recorder
                .add_snapshot(log, &path_display(relative), snapshot)
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
        state
            .baseline
            .entry(relative.to_path_buf())
            .or_insert_with(|| baseline.clone());
        if let Some(active) = state.active_turn.as_mut() {
            active
                .baseline
                .entry(relative.to_path_buf())
                .or_insert(baseline);
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
    files: FileStore<WorkspaceFiles>,
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

fn clear_review(state: &mut SnapshotState) {
    state.baseline.clear();
}

fn snapshot_bytes(snapshot: &BTreeMap<PathBuf, Option<Vec<u8>>>) -> usize {
    snapshot
        .values()
        .filter_map(Option::as_ref)
        .map(Vec::len)
        .fold(0, usize::saturating_add)
}

fn current_file_state(
    workspace: &Workspace,
    path: &Path,
) -> Result<Option<Vec<u8>>, WorkspaceError> {
    current_file_state_bounded(workspace, path, MAX_REWIND_SNAPSHOT_BYTES)
}

fn current_file_state_bounded(
    workspace: &Workspace,
    path: &Path,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, WorkspaceError> {
    workspace
        .exists(path)
        .then(|| workspace.read_raw_bounded(path, max_bytes))
        .transpose()
}

fn snapshot_file_states(
    workspace: &Workspace,
    paths: &[PathBuf],
) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>, WorkspaceError> {
    let mut remaining = MAX_REWIND_SNAPSHOT_BYTES;
    let mut snapshot = BTreeMap::new();
    for path in paths {
        let state = current_file_state_bounded(workspace, path, remaining)?;
        remaining = remaining.saturating_sub(state.as_ref().map_or(0, Vec::len));
        snapshot.insert(path.clone(), state);
    }
    Ok(snapshot)
}

fn apply_file_state(
    workspace: &Workspace,
    path: &Path,
    state: &Option<Vec<u8>>,
) -> Result<(), WorkspaceError> {
    match state {
        Some(bytes) => workspace.atomic_replace(path, bytes),
        None if workspace.exists(path) => workspace.remove(path),
        None => Ok(()),
    }
}

fn apply_file_states_atomic(
    workspace: &Workspace,
    target: &BTreeMap<PathBuf, Option<Vec<u8>>>,
) -> Result<(), WorkspaceError> {
    let paths = target.keys().cloned().collect::<Vec<_>>();
    let previous = snapshot_file_states(workspace, &paths)?;
    let mut applied: Vec<PathBuf> = Vec::new();
    for (path, state) in target {
        if let Err(error) = apply_file_state(workspace, path, state) {
            let rollback = applied
                .into_iter()
                .rev()
                .filter_map(|restored_path| {
                    previous
                        .get(&restored_path)
                        .and_then(|prior| apply_file_state(workspace, &restored_path, prior).err())
                })
                .map(|rollback| rollback.to_string())
                .collect::<Vec<_>>();
            if rollback.is_empty() {
                return Err(error);
            }
            return Err(WorkspaceError::RestoreRollback {
                cause: error.to_string(),
                rollback: rollback.join("; "),
            });
        }
        applied.push(path.clone());
    }
    Ok(())
}

fn reconcile_hunks(
    workspace: &Workspace,
    baseline: &BTreeMap<PathBuf, Option<Vec<u8>>>,
) -> Result<Vec<ReviewHunk>, WorkspaceError> {
    baseline
        .iter()
        .filter_map(|(path, before)| {
            let after = current_file_state(workspace, path);
            match after {
                Err(error) => Some(Err(error)),
                Ok(after) if &after == before => None,
                Ok(after) => {
                    let before = before
                        .as_deref()
                        .map(String::from_utf8_lossy)
                        .unwrap_or_default();
                    let after = after
                        .as_deref()
                        .map(String::from_utf8_lossy)
                        .unwrap_or_default();
                    Some(Ok(ReviewHunk {
                        path: path_display(path),
                        added: after.lines().count(),
                        removed: before.lines().count(),
                        diff: unified_diff(&before, &after),
                    }))
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::checkpoints::{HunkSide, RETAINED_BYTES_LIMIT, ReviewFileStatus};

    /// A log is never shortened behind the caller it answers. The port used to
    /// drop the oldest checkpoint once 64 had accumulated, which silently
    /// retired identities a client was still holding; the ceiling replaced it,
    /// and the ceiling refuses rather than trims.
    #[test]
    fn the_log_is_never_shortened_behind_the_caller() {
        let root = tempdir().expect("workspace");
        std::fs::write(root.path().join("bounded.txt"), "0").expect("seed file");
        let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));

        let turns = 80;
        for turn in 1..=turns {
            review
                .begin_turn_at(format!("turn-{turn}"), turn)
                .expect("begin turn");
            review
                .edit(
                    "bounded.txt",
                    &[EditOperation {
                        old_text: (turn - 1).to_string(),
                        new_text: turn.to_string(),
                        replace_all: false,
                    }],
                )
                .expect("edit");
            review.seal_turn().expect("seal turn");
        }

        let (retained, exhausted) = review
            .with_log(|log| {
                (
                    (1..=turns)
                        .filter(|turn| log.history().has_turn(u64::try_from(*turn).unwrap_or(0)))
                        .count(),
                    log.retained_bytes(),
                )
            })
            .expect("log");
        assert_eq!(retained, turns, "every turn is still addressable");
        assert!(
            exhausted < RETAINED_BYTES_LIMIT,
            "this fixture is nowhere near the ceiling"
        );
        assert!(
            review.take_retention_notice().expect("notice").is_none(),
            "nothing was refused, so nothing is published"
        );
    }

    /// Reaching the ceiling refuses the capture and says so once. The write
    /// itself still lands: the file changes, only its history stops being
    /// tracked, and the operator is told rather than left to discover it.
    #[test]
    fn a_full_log_refuses_the_capture_and_publishes_it() {
        let root = tempdir().expect("workspace");
        std::fs::write(root.path().join("full.txt"), "one\n").expect("seed file");
        let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));
        review
            .with_log(|log| *log = Checkpointer::with_retention_limit(2))
            .expect("log");

        review.begin_turn_at("turn-1", 1).expect("begin turn");
        review
            .edit(
                "full.txt",
                &[EditOperation {
                    old_text: "one".to_owned(),
                    new_text: "two".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("the write still lands");
        review.seal_turn().expect("seal turn");

        assert_eq!(
            std::fs::read_to_string(root.path().join("full.txt")).expect("read"),
            "two\n",
            "the file changed even though its history could not be kept"
        );
        assert!(
            review
                .with_log(|log| log.history().tracked_paths())
                .expect("log")
                .is_empty(),
            "nothing was captured, and nothing already captured was dropped to make room"
        );
        assert!(
            review.restorable_paths_at(1).expect("plan").is_empty(),
            "a plan is built from what the log holds, never from what it refused"
        );
        let notice = review
            .take_retention_notice()
            .expect("notice")
            .expect("the refusal is published");
        assert!(
            notice.contains("limit"),
            "the notice names the limit: {notice}"
        );
        assert!(
            review.take_retention_notice().expect("notice").is_none(),
            "the notice is published once"
        );
    }

    /// A clear or a compaction replaces the message list every turn identifier
    /// is a position in, so the log is emptied rather than left pointing at
    /// positions that moved. A turn that was running is reopened at the new
    /// length, because the tool loop keeps writing after a mid-turn compaction
    /// and would otherwise have nothing to record against.
    #[test]
    fn a_message_list_reset_clears_the_log_and_reopens_a_running_turn() {
        let root = tempdir().expect("workspace");
        let review = seeded(root.path(), "one\n");
        edited_turn(&review, 1, "one\n", "one\ntwo\n");
        assert!(!review.review_state().expect("state").files.is_empty());

        review.reset_messages(0).expect("history cleared");
        let state = review.review_state().expect("state");
        assert!(state.files.is_empty() && state.scopes.is_empty());
        assert!(
            !review.with_log(|log| log.has_open_turn()).expect("log"),
            "no turn was running, so none is reopened"
        );

        review.begin_turn_at("turn-2", 2).expect("begin turn");
        review.reset_messages(7).expect("compacted mid-turn");
        assert!(
            review.with_log(|log| log.has_open_turn()).expect("log"),
            "the turn that was running is reopened"
        );
        review
            .edit(
                "main.txt",
                &[EditOperation {
                    old_text: "two\n".to_owned(),
                    new_text: "three\n".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("the remaining tool loop still records");
        review.seal_turn().expect("seal turn");
        assert_eq!(
            review
                .with_log(|log| log.history().turn_ids())
                .expect("log"),
            vec![7],
            "the reopened turn is numbered against the list that replaced the old one"
        );
    }

    /// A restore that cannot write every path reports the ones it could not and
    /// keeps the ones it could: a plan stopped halfway would leave the
    /// workspace in a state no version of it ever had.
    #[cfg(unix)]
    #[test]
    fn a_partial_restore_reports_its_failures_and_keeps_the_rest() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().expect("workspace");
        std::fs::create_dir(root.path().join("locked")).expect("locked directory");
        std::fs::write(root.path().join("locked/held.txt"), "one\n").expect("seed file");
        std::fs::write(root.path().join("open.txt"), "one\n").expect("seed file");
        let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));
        review.begin_turn_at("turn-1", 1).expect("begin turn");
        for path in ["locked/held.txt", "open.txt"] {
            review
                .edit(
                    path,
                    &[EditOperation {
                        old_text: "one\n".to_owned(),
                        new_text: "one\ntwo\n".to_owned(),
                        replace_all: false,
                    }],
                )
                .expect("edit");
        }
        review.seal_turn().expect("seal turn");

        let locked = root.path().join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555))
            .expect("seal the directory");
        let staged = review.stage_restore_to_message(1);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("reopen the directory");
        let staged = staged.expect("the restore still answers");

        assert_eq!(staged.errors.len(), 1, "one path could not be written");
        assert!(
            staged.errors[0].contains("locked/held.txt"),
            "the failure names its path: {:?}",
            staged.errors
        );
        assert_eq!(
            staged.transaction.commit(),
            vec!["open.txt"],
            "the path that could be written was"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("open.txt")).expect("read"),
            "one\n"
        );
    }

    /// The one distinction the port carries, on the implementation the server
    /// actually runs: nothing there is absence, and something there that will
    /// not open is a failure. Collapsing the second into the first would have
    /// the engine record a deletion nobody performed and offer to revert it.
    #[test]
    fn the_workspace_port_tells_an_absent_path_apart_from_an_unreadable_one() {
        let root = tempdir().expect("workspace");
        std::fs::write(root.path().join("there.txt"), "body\n").expect("seed file");
        std::fs::create_dir(root.path().join("a-directory")).expect("seed directory");
        let files = WorkspaceFiles {
            workspace: Arc::new(Workspace::open(root.path()).expect("open")),
        };

        assert_eq!(files.read_bytes("there.txt"), Ok(Some(b"body\n".to_vec())));
        assert_eq!(files.read_bytes("gone.txt"), Ok(None));
        assert_eq!(files.read_bytes("gone/deeper.txt"), Ok(None));
        assert!(
            files.read_bytes("a-directory").is_err(),
            "something that is there and will not open is never absence"
        );
        assert!(
            files.read_bytes("../outside.txt").is_err(),
            "a path the workspace refuses is never absence either"
        );
    }

    /// The engine is fed by the turn boundaries the server already drives, so a
    /// session that ran two turns and saw one hand edit carries three owners
    /// and can answer what each of them changed.
    #[test]
    fn the_checkpoint_engine_records_the_turns_the_manager_runs() {
        let root = tempdir().expect("workspace");
        std::fs::write(root.path().join("wired.txt"), "one\n").expect("seed file");
        let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));

        review.begin_turn_at("turn-1", 1).expect("begin turn");
        review
            .edit(
                "wired.txt",
                &[EditOperation {
                    old_text: "one\n".to_owned(),
                    new_text: "one\ntwo\n".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("edit");
        review.seal_turn().expect("seal turn");

        // The user edits the file between the turns, and nothing announces it.
        std::fs::write(root.path().join("wired.txt"), "one\ntwo\nby hand\n").expect("hand edit");

        review.begin_turn_at("turn-2", 2).expect("begin turn");
        review
            .edit(
                "wired.txt",
                &[EditOperation {
                    old_text: "by hand\n".to_owned(),
                    new_text: "by hand\nthree\n".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("edit");
        review.seal_turn().expect("seal turn");

        let owners = review
            .with_log(|log| {
                log.history()
                    .regions("wired.txt")
                    .into_iter()
                    .map(|region| region.owner)
                    .collect::<Vec<_>>()
            })
            .expect("log");
        assert_eq!(
            owners,
            vec![
                crate::checkpoints::Owner::Agent { turn_id: 1 },
                crate::checkpoints::Owner::Manual { index: 1 },
                crate::checkpoints::Owner::Agent { turn_id: 2 },
            ],
            "the hand edit keeps its own slot between the two turns"
        );

        let restorable = review
            .with_log(|log| {
                let plan = log.history().restore_plan_to_turn(2);
                plan.iter()
                    .map(|(path, _state)| path.clone())
                    .collect::<Vec<_>>()
            })
            .expect("log");
        assert_eq!(restorable, vec!["wired.txt"]);
    }

    /// Forking drops the turns at and after the fork point from the log, the
    /// way it already drops their snapshots.
    #[test]
    fn forking_truncates_the_engine_log_at_the_fork_point() {
        let root = tempdir().expect("workspace");
        std::fs::write(root.path().join("forked.txt"), "one\n").expect("seed file");
        let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));
        for turn in 1..=3 {
            review
                .begin_turn_at(format!("turn-{turn}"), turn)
                .expect("begin turn");
            review
                .edit(
                    "forked.txt",
                    &[EditOperation {
                        old_text: "one\n".to_owned(),
                        new_text: format!("one\nturn {turn}\n"),
                        replace_all: false,
                    }],
                )
                .expect("edit");
            review.seal_turn().expect("seal turn");
        }

        let forked = review.fork_at(2).expect("fork");

        let turns = forked
            .with_log(|log| {
                let history = log.history();
                (1..=3)
                    .filter(|turn| history.has_turn(*turn))
                    .collect::<Vec<_>>()
            })
            .expect("log");
        assert_eq!(turns, vec![1]);
    }

    // -- The review surface ---------------------------------------------------

    /// A manager over a fresh workspace holding `seed` at `main.txt`.
    fn seeded(root: &std::path::Path, seed: &str) -> ReviewManager {
        std::fs::write(root.join("main.txt"), seed).expect("seed file");
        ReviewManager::new(Arc::new(Workspace::open(root).expect("open")))
    }

    /// One turn replacing `from` with `to` in `main.txt`.
    fn edited_turn(review: &ReviewManager, message_index: usize, from: &str, to: &str) {
        review
            .begin_turn_at(format!("turn-{message_index}"), message_index)
            .expect("begin turn");
        review
            .edit(
                "main.txt",
                &[EditOperation {
                    old_text: from.to_owned(),
                    new_text: to.to_owned(),
                    replace_all: false,
                }],
            )
            .expect("edit");
        review.seal_turn().expect("seal turn");
    }

    #[test]
    fn the_review_surface_lists_the_file_a_turn_changed_with_its_regions() {
        let root = tempdir().expect("workspace");
        let review = seeded(root.path(), "one\n");
        edited_turn(&review, 1, "one\n", "one\ntwo\n");

        let state = review.review_state().expect("review state");
        assert_eq!(state.files.len(), 1);
        assert_eq!(state.files[0].path, "main.txt");
        assert_eq!(state.files[0].status, ReviewFileStatus::Modified);
        assert_eq!(state.files[0].regions.len(), 1);
        assert_eq!(
            state.files[0].regions[0].owner(),
            Owner::Agent { turn_id: 1 }
        );
        assert_eq!(state.files[0].regions[0].decision(), Decision::Pending);
        assert_eq!(
            state
                .scopes
                .iter()
                .map(|scope| scope.owner)
                .collect::<Vec<_>>(),
            vec![Owner::Agent { turn_id: 1 }]
        );
        assert_eq!(state.scopes[0].files[0].region_count, 1);
    }

    /// Rendering is an acting boundary. A hand edit made between two turns is
    /// captured when the panel is projected, in its own slot, rather than being
    /// absorbed into whichever turn touches the file next.
    #[test]
    fn rendering_reconciles_a_hand_edit_into_its_own_slot() {
        let root = tempdir().expect("workspace");
        let review = seeded(root.path(), "one\n");
        edited_turn(&review, 1, "one\n", "one\ntwo\n");
        std::fs::write(root.path().join("main.txt"), "one\ntwo\nby hand\n").expect("hand edit");

        let state = review.review_state().expect("review state");
        assert_eq!(
            state
                .scopes
                .iter()
                .map(|scope| scope.owner)
                .collect::<Vec<_>>(),
            vec![Owner::Agent { turn_id: 1 }, Owner::Manual { index: 1 }]
        );
        let again = review.review_state().expect("second render");
        assert_eq!(
            again.scopes.len(),
            2,
            "reconciliation is idempotent, so a second render adds no slot"
        );
    }

    #[test]
    fn approving_leaves_disk_alone_and_reverting_writes_the_file_back() {
        let root = tempdir().expect("workspace");
        let review = seeded(root.path(), "one\n");
        edited_turn(&review, 1, "one\n", "one\ntwo\n");

        let approved = review
            .approve_review(&ReviewTarget::File {
                path: "main.txt".to_owned(),
            })
            .expect("approve");
        assert_eq!(approved, vec!["main.txt"]);
        assert_eq!(
            std::fs::read_to_string(root.path().join("main.txt")).expect("read"),
            "one\ntwo\n",
            "approved content already sits on disk"
        );
        assert!(
            review.review_state().expect("state").files.is_empty(),
            "an approved file has nothing left to review"
        );

        let second = tempdir().expect("workspace");
        let review = seeded(second.path(), "one\n");
        edited_turn(&review, 1, "one\n", "one\ntwo\n");
        let reverted = review
            .revert_review(&ReviewTarget::File {
                path: "main.txt".to_owned(),
            })
            .expect("revert");
        assert_eq!(reverted, vec!["main.txt"]);
        assert_eq!(
            std::fs::read_to_string(second.path().join("main.txt")).expect("read"),
            "one\n",
            "a revert is persisted immediately"
        );
    }

    /// Deciding is an acting boundary too. A hand edit made since the last look
    /// is sealed into its own slot before the decision lands, so reverting the
    /// turn the operator was reviewing does not take their work with it.
    #[test]
    fn deciding_reconciles_a_hand_edit_before_the_decision_lands() {
        let root = tempdir().expect("workspace");
        let review = seeded(root.path(), "one\ntail\n");
        edited_turn(&review, 1, "one\n", "one\ntwo\n");
        // A hand edit nowhere near the turn's line, so nothing drags it.
        std::fs::write(root.path().join("main.txt"), "one\ntwo\ntail\nby hand\n")
            .expect("hand edit");

        review
            .revert_review(&ReviewTarget::Scope {
                owner: Owner::Agent { turn_id: 1 },
            })
            .expect("revert the turn");

        assert_eq!(
            std::fs::read_to_string(root.path().join("main.txt")).expect("read"),
            "one\ntail\nby hand\n",
            "the turn's line is gone and the hand edit is still there"
        );
        let state = review.review_state().expect("review state");
        assert_eq!(
            state
                .scopes
                .iter()
                .map(|scope| scope.owner)
                .collect::<Vec<_>>(),
            vec![Owner::Agent { turn_id: 1 }, Owner::Manual { index: 1 }],
            "the hand edit kept its own slot rather than being reverted with the turn"
        );
    }

    /// Anchoring is an acting boundary for the same reason: the whole-file
    /// anchors have to line up with the content the panel renders, which is
    /// disk, so a drift is folded in before the diff is taken.
    #[test]
    fn anchoring_reconciles_before_it_locates_the_pending_hunks() {
        let root = tempdir().expect("workspace");
        let review = seeded(root.path(), "one\n");
        edited_turn(&review, 1, "one\n", "one\ntwo\n");
        std::fs::write(root.path().join("main.txt"), "one\ntwo\nby hand\n").expect("hand edit");

        let hunks = review.file_hunks("main.txt", None).expect("hunks");
        assert_eq!(
            hunks.len(),
            1,
            "the turn's line and the hand edit render as one adjacent block"
        );
        assert_eq!(hunks[0].side, HunkSide::Additions);
        assert_eq!(
            hunks[0].line, 2,
            "the anchor sits on the last line of the rendered block, the hand edit's"
        );
        assert_eq!(
            hunks[0].regions.len(),
            2,
            "a control there decides the turn's hunk and the hand edit together"
        );
    }

    /// A decision is durable only once what it implies on disk has landed. When
    /// the write fails the log goes back to what it was, so the surface never
    /// reports a region decided against a file that did not change.
    #[cfg(unix)]
    #[test]
    fn a_write_that_does_not_land_rolls_the_decision_back() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().expect("workspace");
        std::fs::create_dir(root.path().join("locked")).expect("locked directory");
        std::fs::write(root.path().join("locked/main.txt"), "one\n").expect("seed file");
        let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));
        review.begin_turn_at("turn-1", 1).expect("begin turn");
        review
            .edit(
                "locked/main.txt",
                &[EditOperation {
                    old_text: "one\n".to_owned(),
                    new_text: "one\ntwo\n".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("edit");
        review.seal_turn().expect("seal turn");

        let locked = root.path().join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555))
            .expect("seal the directory");
        let failure = review.revert_review(&ReviewTarget::File {
            path: "locked/main.txt".to_owned(),
        });
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("reopen the directory");

        assert!(
            matches!(failure, Err(ReviewError::File(_))),
            "the write failure is reported: {failure:?}"
        );
        let state = review.review_state().expect("review state");
        assert_eq!(
            state.files.len(),
            1,
            "the file is still pending, so the decision did not stick"
        );
        assert_eq!(
            state.files[0].regions[0].decision(),
            Decision::Pending,
            "the log was rolled back to before the refused decision"
        );
    }

    #[test]
    fn the_baseline_the_scope_diff_and_the_hunks_answer_from_the_log() {
        let root = tempdir().expect("workspace");
        let review = seeded(root.path(), "one\n");
        edited_turn(&review, 1, "one\n", "one\ntwo\n");

        assert_eq!(
            review.baseline_text("main.txt").expect("baseline"),
            "one\n",
            "nothing is kept yet, so the accepted baseline is the original"
        );
        assert_eq!(
            review.baseline_text("untracked.txt").expect("baseline"),
            "",
            "a path the log never saw answers empty rather than failing"
        );

        let diff = review
            .scope_file_diff("main.txt", Owner::Agent { turn_id: 1 })
            .expect("scope diff");
        assert_eq!(diff.status, ReviewFileStatus::Modified);
        assert_eq!(diff.baseline, "one\n");
        assert_eq!(diff.current, "one\ntwo\n");

        let hunks = review.file_hunks("main.txt", None).expect("hunks");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].side, HunkSide::Additions);
        assert_eq!(hunks[0].line, 1);
        assert_eq!(hunks[0].regions.len(), 1);

        let scoped = review
            .file_hunks("main.txt", Some(Owner::Agent { turn_id: 1 }))
            .expect("scoped hunks");
        assert_eq!(scoped, hunks, "the one turn's scope is the whole diff");
        assert!(
            review
                .file_hunks("main.txt", Some(Owner::Agent { turn_id: 9 }))
                .expect("absent owner")
                .is_empty(),
            "an owner with nothing pending anchors nothing"
        );
    }
}

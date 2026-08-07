use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{
    Checkpoint, EditOperation, MutationResult, ReviewHunk, ReviewView, Workspace, WorkspaceError,
    path_display, text_file, unified_diff,
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
) -> Result<String, WorkspaceError> {
    let mut updated = original.to_owned();
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
        updated = if operation.replace_all {
            updated.replace(&operation.old_text, &operation.new_text)
        } else {
            updated.replacen(&operation.old_text, &operation.new_text, 1)
        };
    }
    Ok(updated)
}

const MAX_REWIND_CHECKPOINTS: usize = 64;
const MAX_REWIND_SNAPSHOT_BYTES: usize = 64 * 1_048_576;

#[derive(Clone)]
struct ActiveReviewTurn {
    turn_id: String,
    message_index: usize,
    baseline: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

#[derive(Clone)]
struct StoredCheckpoint {
    public: Checkpoint,
    message_index: usize,
    before: BTreeMap<PathBuf, Option<Vec<u8>>>,
    snapshot_bytes: usize,
}

#[derive(Default)]
struct ReviewState {
    active_turn: Option<ActiveReviewTurn>,
    baseline: BTreeMap<PathBuf, Option<Vec<u8>>>,
    checkpoints: VecDeque<StoredCheckpoint>,
    checkpoint_bytes: usize,
}

pub struct ReviewManager {
    workspace: Arc<Workspace>,
    state: Mutex<ReviewState>,
}

impl ReviewManager {
    #[must_use]
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self {
            workspace,
            state: Mutex::new(ReviewState::default()),
        }
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
        state.active_turn = Some(ActiveReviewTurn {
            turn_id: turn_id.into(),
            message_index,
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
        let updated = apply_edit_operations(&relative, &decoded.text, operations)?;
        let bytes = text_file::encode(&updated, decoded.encoding, decoded.newline);
        if bytes != original {
            self.workspace.atomic_replace(&relative, &bytes)?;
        }
        Ok(MutationResult {
            path: path_display(&relative),
            bytes_written: bytes.len(),
            files_changed: 1,
            diff: unified_diff(&decoded.text, &updated),
        })
    }

    pub fn seal_turn(&self) -> Result<Checkpoint, WorkspaceError> {
        let mut state = self.lock_state()?;
        let active = state
            .active_turn
            .as_ref()
            .ok_or(WorkspaceError::NoActiveTurn)?;
        let hunks = reconcile_hunks(&self.workspace, &active.baseline)?;
        let active = state
            .active_turn
            .take()
            .ok_or(WorkspaceError::NoActiveTurn)?;
        let checkpoint = Checkpoint {
            turn_id: active.turn_id,
            hunks,
        };
        let snapshot_bytes = snapshot_bytes(&active.baseline);
        state.checkpoint_bytes = state.checkpoint_bytes.saturating_add(snapshot_bytes);
        state.checkpoints.push_back(StoredCheckpoint {
            public: checkpoint.clone(),
            message_index: active.message_index,
            before: active.baseline,
            snapshot_bytes,
        });
        trim_checkpoints(&mut state);
        Ok(checkpoint)
    }

    pub fn view(&self) -> Result<ReviewView, WorkspaceError> {
        let state = self.lock_state()?;
        Ok(ReviewView {
            active_turn: state
                .active_turn
                .as_ref()
                .map(|active| active.turn_id.clone()),
            checkpoints: state
                .checkpoints
                .iter()
                .map(|checkpoint| checkpoint.public.clone())
                .collect(),
            pending_hunks: reconcile_hunks(&self.workspace, &state.baseline)?,
        })
    }

    pub fn restorable_paths_at(&self, message_index: usize) -> Result<Vec<String>, WorkspaceError> {
        let plan = self.restoration_plan(message_index)?;
        let mut paths = Vec::new();
        for (path, target) in plan {
            if current_file_state(&self.workspace, &path)? != target {
                paths.push(path_display(&path));
            }
        }
        Ok(paths)
    }

    pub fn stage_restore_to_message(
        &self,
        message_index: usize,
    ) -> Result<RestoreTransaction, WorkspaceError> {
        let plan = self.restoration_plan(message_index)?;
        let paths = plan.keys().cloned().collect::<Vec<_>>();
        let previous = snapshot_file_states(&self.workspace, &paths)?;
        let changed_paths = plan
            .iter()
            .filter(|(path, target)| previous.get(*path) != Some(*target))
            .map(|(path, _)| path_display(path))
            .collect();
        apply_file_states_atomic(&self.workspace, &plan)?;
        Ok(RestoreTransaction {
            workspace: self.workspace.clone(),
            previous: Some(previous),
            changed_paths,
        })
    }

    pub fn fork_at(&self, message_index: usize) -> Result<Self, WorkspaceError> {
        let state = self.lock_state()?;
        let checkpoints = state
            .checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.message_index < message_index)
            .cloned()
            .collect::<VecDeque<_>>();
        let checkpoint_bytes = checkpoints
            .iter()
            .map(|checkpoint| checkpoint.snapshot_bytes)
            .sum();
        Ok(Self {
            workspace: self.workspace.clone(),
            state: Mutex::new(ReviewState {
                active_turn: None,
                baseline: state.baseline.clone(),
                checkpoints,
                checkpoint_bytes,
            }),
        })
    }

    pub fn approve(&self) -> Result<ReviewView, WorkspaceError> {
        let mut state = self.lock_state()?;
        if state.active_turn.is_some() {
            return Err(WorkspaceError::ReviewBusy);
        }
        clear_review(&mut state);
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
        drop(state);
        self.view()
    }

    fn restoration_plan(
        &self,
        message_index: usize,
    ) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>, WorkspaceError> {
        let state = self.lock_state()?;
        let mut plan = BTreeMap::new();
        for checkpoint in state
            .checkpoints
            .iter()
            .rev()
            .filter(|checkpoint| checkpoint.message_index >= message_index)
        {
            for (path, before) in &checkpoint.before {
                plan.insert(path.clone(), before.clone());
            }
        }
        Ok(plan)
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

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, ReviewState>, WorkspaceError> {
        self.state
            .lock()
            .map_err(|_| WorkspaceError::LockPoisoned { surface: "review" })
    }
}

pub struct RestoreTransaction {
    workspace: Arc<Workspace>,
    previous: Option<BTreeMap<PathBuf, Option<Vec<u8>>>>,
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
        apply_file_states_atomic(&self.workspace, &previous)
    }
}

impl Drop for RestoreTransaction {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            let _ = apply_file_states_atomic(&self.workspace, &previous);
        }
    }
}

fn clear_review(state: &mut ReviewState) {
    state.baseline.clear();
    state.checkpoints.clear();
    state.checkpoint_bytes = 0;
}

fn trim_checkpoints(state: &mut ReviewState) {
    while state.checkpoints.len() > MAX_REWIND_CHECKPOINTS
        || state.checkpoint_bytes > MAX_REWIND_SNAPSHOT_BYTES
    {
        let Some(checkpoint) = state.checkpoints.pop_front() else {
            break;
        };
        state.checkpoint_bytes = state
            .checkpoint_bytes
            .saturating_sub(checkpoint.snapshot_bytes);
    }
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

    #[test]
    fn checkpoint_retention_is_bounded() {
        let root = tempdir().expect("workspace");
        std::fs::write(root.path().join("bounded.txt"), "0").expect("seed file");
        let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));

        for turn in 1..=MAX_REWIND_CHECKPOINTS + 1 {
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

        let checkpoints = review.view().expect("view").checkpoints;
        assert_eq!(checkpoints.len(), MAX_REWIND_CHECKPOINTS);
        assert_eq!(checkpoints[0].turn_id, "turn-2");
    }
}

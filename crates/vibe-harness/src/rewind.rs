//! Per-turn file snapshots that answer a rewind's restore.
//!
//! Reference `RewindFileSnapshots` (`vibe/app_server/_unified_harness_snapshots.py`):
//! the harness writes files without snapshotting them, so the host records
//! disk states at the turn boundaries it observes in the published state.
//! A turn is anchored on its opening user entry, which is what a rewind
//! names. A file's state before a turn is the content a `write_file` replaced
//! when its result says so, else what the file held when the turn began
//! (for a path the previous turn touched) or when the turn first touched it.
//! Snapshots live in memory: turns from before this process restore nothing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// A file's content, or `None` when it does not exist.
pub type FileState = Option<Vec<u8>>;

/// One recorded pre-state: the turn's checkpoint, the path, and what the
/// path held before the turn wrote it.
#[derive(Debug, Clone)]
struct PreState {
    checkpoint: u64,
    path: String,
    state: FileState,
}

/// What rewinding to one anchor would restore, and how a restore went.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Restored {
    pub errors: Vec<String>,
    pub paths: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct RewindSnapshots {
    next_checkpoint: u64,
    turns: Vec<(String, u64)>,
    anchors: Vec<(String, u64)>,
    log: Vec<PreState>,
    seen_effects: BTreeSet<String>,
    begun_turn: Option<String>,
    touched: BTreeSet<String>,
    pending: Vec<(String, FileState)>,
    observed_active: Option<String>,
    status_known: bool,
}

impl RewindSnapshots {
    /// Reference `observe`: folds one published state (its wire shape) into
    /// the log. `cwd` resolves the relative paths a write names.
    pub fn observe(&mut self, state: &Value, cwd: &str) {
        let active = active_turn(state);
        if active != self.observed_active {
            self.seal_open_turn();
            if let Some(turn) = &active
                && !self.turns.iter().any(|(known, _)| known == turn)
            {
                // A turn first seen already running is joined mid-flight: a
                // read at its start would capture its own writes.
                let clean = self.status_known && self.observed_active.is_none();
                self.begin_turn(turn, state, clean);
            }
            self.observed_active = active;
        }
        self.status_known = true;
        self.record_file_effects(state, cwd);
    }

    /// Reference `restorable_paths`: what a rewind to `anchor` would change.
    #[must_use]
    pub fn restorable_paths(&self, anchor: &str) -> Vec<String> {
        self.plan(anchor)
            .into_iter()
            .filter(|(path, target)| read(path).is_ok_and(|current| current != *target))
            .map(|(path, _)| path)
            .collect()
    }

    /// Reference `restore`: the disk as it was before `anchor`'s turn.
    #[must_use]
    pub fn restore(&self, anchor: &str) -> Restored {
        let mut restored = Restored::default();
        for (path, target) in self.plan(anchor) {
            let Ok(current) = read(&path) else {
                restored.errors.push(format!("Failed to read file: {path}"));
                continue;
            };
            if current == target {
                continue;
            }
            match write(&path, target.as_deref()) {
                Ok(()) => restored.paths.push(path),
                Err(error) => restored
                    .errors
                    .push(format!("Failed to restore {path}: {error}")),
            }
        }
        restored
    }

    /// Reference `drop_from`: forgets the turns a rewind to `anchor` cut.
    pub fn drop_from(&mut self, anchor: &str) {
        let Some(cut) = self.anchor(anchor) else {
            *self = Self {
                next_checkpoint: self.next_checkpoint,
                seen_effects: std::mem::take(&mut self.seen_effects),
                observed_active: self.observed_active.take(),
                status_known: self.status_known,
                ..Self::default()
            };
            return;
        };
        if let Some(turn) = self.begun_turn.clone() {
            if self
                .checkpoint_of(&turn)
                .is_some_and(|checkpoint| checkpoint >= cut)
            {
                self.close_open_turn();
            } else {
                self.seal_open_turn();
            }
        }
        self.log.retain(|pre| pre.checkpoint < cut);
        self.turns.retain(|(_, checkpoint)| *checkpoint < cut);
        self.anchors.retain(|(_, checkpoint)| *checkpoint < cut);
    }

    /// Reference `forked`: what a fork cut before `anchor` inherits, its
    /// anchors renamed onto the fork's own entries.
    #[must_use]
    pub fn forked(&self, anchor: &str, renamed: &[(String, String)]) -> Self {
        let mut forked = Self {
            next_checkpoint: self.next_checkpoint,
            status_known: true,
            ..Self::default()
        };
        let Some(cut) = self.anchor(anchor) else {
            return forked;
        };
        forked.log = self
            .log
            .iter()
            .filter(|pre| pre.checkpoint < cut)
            .cloned()
            .collect();
        forked.anchors = self
            .anchors
            .iter()
            .filter(|(_, checkpoint)| *checkpoint < cut)
            .filter_map(|(entry, checkpoint)| {
                renamed
                    .iter()
                    .find(|(source, _)| source == entry)
                    .map(|(_, own)| (own.clone(), *checkpoint))
            })
            .collect();
        forked
    }

    fn anchor(&self, anchor: &str) -> Option<u64> {
        self.anchors
            .iter()
            .find(|(entry, _)| entry == anchor)
            .map(|(_, checkpoint)| *checkpoint)
    }

    fn checkpoint_of(&self, turn: &str) -> Option<u64> {
        self.turns
            .iter()
            .find(|(known, _)| known == turn)
            .map(|(_, checkpoint)| *checkpoint)
    }

    /// Reference `_plan`: for each path a turn from `anchor`'s on wrote, the
    /// state before the earliest of them; the open turn's carried reads too.
    fn plan(&self, anchor: &str) -> Vec<(String, FileState)> {
        let Some(cut) = self.anchor(anchor) else {
            return Vec::new();
        };
        let mut plan: Vec<(String, FileState)> = Vec::new();
        let mut log: Vec<&PreState> = self
            .log
            .iter()
            .filter(|pre| pre.checkpoint >= cut)
            .collect();
        log.sort_by_key(|pre| pre.checkpoint);
        for pre in log {
            if !plan.iter().any(|(path, _)| *path == pre.path) {
                plan.push((pre.path.clone(), pre.state.clone()));
            }
        }
        for (path, state) in &self.pending {
            if !plan.iter().any(|(known, _)| known == path) {
                plan.push((path.clone(), state.clone()));
            }
        }
        plan
    }

    /// The paths the latest recorded turn wrote.
    fn last_turn_paths(&self) -> Vec<String> {
        let Some(last) = self.log.iter().map(|pre| pre.checkpoint).max() else {
            return Vec::new();
        };
        self.log
            .iter()
            .filter(|pre| pre.checkpoint == last)
            .map(|pre| pre.path.clone())
            .collect()
    }

    fn begin_turn(&mut self, turn: &str, state: &Value, clean: bool) {
        self.next_checkpoint += 1;
        let checkpoint = self.next_checkpoint;
        self.turns.push((turn.to_owned(), checkpoint));
        if let Some(anchor) = turn_anchor(state, turn) {
            self.anchors.push((anchor, checkpoint));
        }
        let carried = self.last_turn_paths();
        self.begun_turn = Some(turn.to_owned());
        if !clean {
            return;
        }
        // Held back until a write names its exact replaced text, which wins,
        // or the turn seals: this read can lag the turn's first write.
        for path in carried {
            if let Ok(read) = read(&path) {
                self.pending.push((path, read));
            }
        }
    }

    fn seal_open_turn(&mut self) {
        if self.begun_turn.is_none() {
            return;
        }
        for (path, state) in std::mem::take(&mut self.pending) {
            self.record_pre(&path, state);
        }
        self.close_open_turn();
    }

    fn close_open_turn(&mut self) {
        self.begun_turn = None;
        self.touched.clear();
        self.pending.clear();
    }

    fn record_pre(&mut self, path: &str, state: FileState) {
        let Some(checkpoint) = self
            .begun_turn
            .as_deref()
            .and_then(|turn| self.checkpoint_of(turn))
        else {
            return;
        };
        if self
            .log
            .iter()
            .any(|pre| pre.checkpoint == checkpoint && pre.path == path)
        {
            return;
        }
        self.log.push(PreState {
            checkpoint,
            path: path.to_owned(),
            state,
        });
    }

    fn record_file_effects(&mut self, state: &Value, cwd: &str) {
        let Some(history) = state["history"].as_array() else {
            return;
        };
        for entry in history {
            let Some(id) = entry["id"].as_str() else {
                continue;
            };
            if entry["type"] != "effect" || self.seen_effects.contains(id) {
                continue;
            }
            let Some((path, replaced)) = file_touch(entry, cwd) else {
                continue;
            };
            self.seen_effects.insert(id.to_owned());
            // Inherited fork entries carry no turn, so an idle session would
            // otherwise match them against the absent open turn.
            if self.begun_turn.is_none()
                || entry["turnId"].as_str() != self.begun_turn.as_deref()
                || self.touched.contains(&path)
            {
                continue;
            }
            self.touched.insert(path.clone());
            let carried = self
                .pending
                .iter()
                .position(|(known, _)| *known == path)
                .map(|index| self.pending.remove(index).1);
            let pre = match (replaced, carried) {
                (Some(replaced), _) => Some(replaced),
                (None, Some(carried)) => Some(carried),
                // A first touch with no known state before it: tracked from
                // here on.
                (None, None) => read(&path).ok(),
            };
            if let Some(pre) = pre {
                self.record_pre(&path, pre);
            }
        }
    }
}

/// Reference `_active_turn`.
fn active_turn(state: &Value) -> Option<String> {
    let status = &state["session"]["status"];
    matches!(status["type"].as_str(), Some("running" | "blocked"))
        .then(|| status["activeTurnId"].as_str().map(ToOwned::to_owned))
        .flatten()
}

/// Reference `_turn_anchor`: the user entry that opened `turn`.
fn turn_anchor(state: &Value, turn: &str) -> Option<String> {
    state["history"].as_array()?.iter().find_map(|entry| {
        (entry["type"] == "message" && entry["role"] == "user" && entry["turnId"] == turn)
            .then(|| entry["id"].as_str().map(ToOwned::to_owned))
            .flatten()
    })
}

/// Reference `_file_touch`: a completed write's resolved path, and the exact
/// state it replaced when its result says.
fn file_touch(entry: &Value, cwd: &str) -> Option<(String, Option<FileState>)> {
    if entry["state"]["status"] != "completed" {
        return None;
    }
    let detail = &entry["detail"];
    let path = detail["input"]["filePath"].as_str()?;
    match detail["kind"].as_str()? {
        "file_write" => Some((
            resolve(cwd, path),
            replaced_state(&entry["state"]["output"]),
        )),
        "file_edit" => Some((resolve(cwd, path), None)),
        _ => None,
    }
}

/// Reference `_replaced_state`.
fn replaced_state(output: &Value) -> Option<FileState> {
    if !output["filePath"].is_string() || !output["content"].is_string() {
        return None;
    }
    if output["fileExisted"] != true {
        return Some(None);
    }
    output["previousContent"]
        .as_str()
        .map(|text| Some(text.as_bytes().to_vec()))
}

fn resolve(cwd: &str, path: &str) -> String {
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        PathBuf::from(cwd).join(path)
    };
    std::fs::canonicalize(&absolute)
        .unwrap_or(absolute)
        .to_string_lossy()
        .into_owned()
}

fn read(path: &str) -> std::io::Result<FileState> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write(path: &str, target: Option<&[u8]>) -> std::io::Result<()> {
    match target {
        Some(bytes) => {
            if let Some(parent) = Path::new(path).parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)
        }
        None => match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        },
    }
}

#[cfg(test)]
mod rewind_tests;

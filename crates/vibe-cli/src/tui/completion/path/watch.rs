//! Filesystem watching for the `@` completion index.
//!
//! The reference runs `watchfiles.watch` on a daemon thread with a 200 ms
//! step, waits half a second for the thread to report readiness, and joins it
//! for at most a second on stop
//! (`vibe/cli/autocompletion/file_indexer/watcher.py`). `watchfiles` is itself
//! built on the `notify` crate, so the platform backends and their event
//! categories are the same ones this controller reads.
//!
//! Batches are delivered the way `watchfiles.watch` yields them: the changes
//! seen are kept as a set, polled every step, and handed over once a step adds
//! nothing new or the 1 600 ms debounce has passed since the first one; the
//! set then goes through `watchfiles.DefaultFilter`, and an empty result is not
//! delivered at all.
//!
//! One difference is deliberate and invisible to the index: upstream the watch
//! thread calls back into the store under a lock, while here it hands each
//! batch to a channel the index drains before it answers a query. A batch
//! therefore reaches the store at the same point in the query sequence either
//! way, and the boundary stays single-threaded.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{RecursiveMode, Watcher};
use vibe_core::sync::lock;

/// The reference's `step=200`: how often the change set is polled.
const WATCH_STEP: Duration = Duration::from_millis(200);
/// `watchfiles.watch`'s default `debounce=1600`: the longest a growing change
/// set is held after it was first seen.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(1_600);
/// `watchfiles.DefaultFilter.ignore_dirs`: a change under any of these is
/// dropped.
const IGNORED_DIRECTORIES: [&str; 11] = [
    "__pycache__",
    ".git",
    ".hg",
    ".svn",
    ".tox",
    ".venv",
    ".idea",
    "node_modules",
    ".mypy_cache",
    ".pytest_cache",
    ".hypothesis",
];
/// How long `start` waits for the backend to report that it is watching,
/// matching the reference's readiness wait.
const READY_TIMEOUT: Duration = Duration::from_millis(500);
/// How long `stop` waits for the watch thread to finish, matching the
/// reference's join timeout. A thread that outlives it is detached rather than
/// waited on, so process exit is never blocked past this bound.
const JOIN_TIMEOUT: Duration = Duration::from_secs(1);

/// The three change categories the index applies. Every other event the
/// platform reports is dropped before it reaches a batch, which is what the
/// reference's membership test does with the categories `watchfiles` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// One delivered batch: the root it was observed under, and its changes. The
/// root travels with the batch so the index can drop a batch left over from a
/// watcher it has already replaced.
pub(super) type ChangeBatch = (PathBuf, Vec<(ChangeKind, PathBuf)>);

/// A one-way flag a thread can wait on, which is what the reference's
/// `threading.Event` gives it.
#[derive(Debug, Default)]
struct Flag {
    raised: Mutex<bool>,
    changed: Condvar,
}

impl Flag {
    fn set(&self) {
        *lock(&self.raised) = true;
        self.changed.notify_all();
    }

    fn is_set(&self) -> bool {
        *lock(&self.raised)
    }

    /// Waits until the flag is raised or `timeout` elapses, reporting whether
    /// it was raised.
    fn wait(&self, timeout: Duration) -> bool {
        let (raised, _) = self
            .changed
            .wait_timeout_while(lock(&self.raised), timeout, |raised| !*raised)
            .unwrap_or_else(PoisonError::into_inner);
        *raised
    }
}

#[derive(Debug)]
struct Watch {
    thread: JoinHandle<()>,
    stop: Arc<Flag>,
    finished: Arc<Flag>,
    root: PathBuf,
}

/// Owns at most one watch thread and the channel its batches arrive on.
#[derive(Debug)]
pub(super) struct WatchController {
    active: Option<Watch>,
    batches: Sender<ChangeBatch>,
    delivered: Receiver<ChangeBatch>,
}

impl Default for WatchController {
    fn default() -> Self {
        let (batches, delivered) = mpsc::channel();
        Self {
            active: None,
            batches,
            delivered,
        }
    }
}

impl WatchController {
    /// Starts watching `root`, or leaves an equivalent watch running.
    ///
    /// `root` must already be resolved: the backend reports paths built from
    /// the path it was handed, and the index keys its entries off the resolved
    /// root.
    pub(super) fn start(&mut self, root: &Path) -> Result<(), String> {
        if self
            .active
            .as_ref()
            .is_some_and(|watch| watch.root == root && !watch.thread.is_finished())
        {
            return Ok(());
        }
        self.stop();

        let stop = Arc::new(Flag::default());
        let finished = Arc::new(Flag::default());
        let ready = Arc::new(Flag::default());
        let failure = Arc::new(Mutex::new(None::<String>));
        let watched = root.to_path_buf();
        let thread = {
            let stop = Arc::clone(&stop);
            let finished = Arc::clone(&finished);
            let ready = Arc::clone(&ready);
            let failure = Arc::clone(&failure);
            let batches = self.batches.clone();
            let root = watched.clone();
            std::thread::Builder::new()
                .name("vibe-file-index-watch".to_owned())
                .spawn(move || {
                    watch_loop(&root, &stop, &ready, &failure, &batches);
                    finished.set();
                    ready.set();
                })
                .map_err(|error| error.to_string())?
        };
        ready.wait(READY_TIMEOUT);
        let failed = lock(&failure).clone();
        if let Some(reason) = failed {
            stop.set();
            drop(thread);
            return Err(reason);
        }
        self.active = Some(Watch {
            thread,
            stop,
            finished,
            root: watched,
        });
        Ok(())
    }

    /// Whether a watch thread is still running, which is what the reference's
    /// `is_watching` property reports. Nothing in the running client reads it;
    /// it exists so the start and stop paths are observable.
    #[cfg(test)]
    pub(super) fn is_watching(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|watch| !watch.thread.is_finished())
    }

    /// Stops the running watch, waiting at most [`JOIN_TIMEOUT`] for its
    /// thread. A thread still running past the bound is detached, so a wedged
    /// backend cannot hold the process open.
    pub(super) fn stop(&mut self) {
        let Some(watch) = self.active.take() else {
            return;
        };
        watch.stop.set();
        if watch.finished.wait(JOIN_TIMEOUT) {
            let _ = watch.thread.join();
        }
    }

    /// Takes the next delivered batch, if the watch thread has produced one.
    pub(super) fn next_batch(&self) -> Option<ChangeBatch> {
        self.delivered.try_recv().ok()
    }
}

impl Drop for WatchController {
    fn drop(&mut self) {
        self.stop();
    }
}

fn watch_loop(
    root: &Path,
    stop: &Arc<Flag>,
    ready: &Arc<Flag>,
    failure: &Arc<Mutex<Option<String>>>,
    batches: &Sender<ChangeBatch>,
) {
    let (events, incoming) = mpsc::channel();
    let mut watcher = match notify::recommended_watcher(events) {
        Ok(watcher) => watcher,
        Err(error) => return report(failure, ready, &error.to_string()),
    };
    if let Err(error) = watcher.watch(root, RecursiveMode::Recursive) {
        return report(failure, ready, &error.to_string());
    }
    ready.set();

    let mut pending = Vec::new();
    let mut seen = HashSet::new();
    let mut last_size = 0usize;
    let mut deadline: Option<Instant> = None;
    loop {
        // One step of `RustNotify.watch`: sleep, collecting what arrives.
        let step_end = Instant::now().checked_add(WATCH_STEP);
        while let Some(remaining) = step_end
            .and_then(|end| end.checked_duration_since(Instant::now()))
            .filter(|remaining| !remaining.is_zero())
        {
            match incoming.recv_timeout(remaining) {
                Ok(Ok(event)) => {
                    let mut changes = Vec::new();
                    collect(&event, &mut changes);
                    for change in changes {
                        if seen.insert(change.clone()) {
                            pending.push(change);
                        }
                    }
                }
                // A backend error after the watch started is not fatal
                // upstream either: the loop keeps reporting what still arrives.
                Ok(Err(_)) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if stop.is_set() {
            break;
        }
        let size = pending.len();
        if size == 0 {
            continue;
        }
        if size != last_size {
            last_size = size;
            let now = Instant::now();
            match deadline {
                Some(limit) if now > limit => {}
                Some(_) => continue,
                None => {
                    deadline = now.checked_add(WATCH_DEBOUNCE);
                    continue;
                }
            }
        }
        let batch = std::mem::take(&mut pending)
            .into_iter()
            .filter(|(_, path)| default_filter_allows(path))
            .collect::<Vec<_>>();
        seen.clear();
        last_size = 0;
        deadline = None;
        if batch.is_empty() {
            continue;
        }
        if batches.send((root.to_path_buf(), batch)).is_err() {
            break;
        }
    }
}

/// `watchfiles.DefaultFilter.__call__`: a change is dropped when any segment
/// of its path is an ignored directory or its last segment is an editor,
/// bytecode or system file.
pub(super) fn default_filter_allows(path: &Path) -> bool {
    let segments = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if segments
        .iter()
        .any(|segment| IGNORED_DIRECTORIES.contains(&segment.as_ref()))
    {
        return false;
    }
    let name = segments.last().map_or("", |segment| segment.as_ref());
    !is_ignored_entity(name)
}

/// `DefaultFilter.ignore_entity_patterns`, searched in the entity's name:
/// `\.py[cod]$`, `\.___jb_...___$`, `\.sw.$`, `~$`, `^\.\#`,
/// `^\.DS_Store$` and `^flycheck_`. A `$` also matches before a final newline.
fn is_ignored_entity(name: &str) -> bool {
    let at_end = |test: fn(&[char]) -> bool| {
        let characters = name.chars().collect::<Vec<_>>();
        test(&characters)
            || characters
                .split_last()
                .is_some_and(|(last, rest)| *last == '\n' && test(rest))
    };
    at_end(|name| matches!(name, [.., '.', 'p', 'y', 'c' | 'o' | 'd']))
        || at_end(|name| {
            matches!(
                name,
                [.., '.', '_', '_', '_', 'j', 'b', '_', a, b, c, '_', '_', '_']
                    if ![a, b, c].contains(&&'\n')
            )
        })
        || at_end(|name| matches!(name, [.., '.', 's', 'w', last] if *last != '\n'))
        || at_end(|name| matches!(name, [.., '~']))
        || at_end(|name| name == ['.', 'D', 'S', '_', 'S', 't', 'o', 'r', 'e'])
        || name.starts_with(".#")
        || name.starts_with("flycheck_")
}

fn report(failure: &Arc<Mutex<Option<String>>>, ready: &Arc<Flag>, reason: &str) {
    *lock(failure) = Some(reason.to_owned());
    ready.set();
}

/// Maps one platform event onto the three categories the store applies.
///
/// A rename is reported as a pair by `watchfiles` and by the inotify backend
/// alike: the old path is gone and the new one appeared. Where the backend
/// reports a rename without naming which side a path is on, existence decides.
fn collect(event: &notify::Event, batch: &mut Vec<(ChangeKind, PathBuf)>) {
    match event.kind {
        EventKind::Create(_) => extend(batch, ChangeKind::Added, &event.paths),
        EventKind::Remove(_) => extend(batch, ChangeKind::Deleted, &event.paths),
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            extend(batch, ChangeKind::Deleted, &event.paths);
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            extend(batch, ChangeKind::Added, &event.paths);
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            if let Some((from, to)) = event.paths.split_first() {
                batch.push((ChangeKind::Deleted, from.clone()));
                extend(batch, ChangeKind::Added, to);
            }
        }
        EventKind::Modify(ModifyKind::Name(_)) => {
            for path in &event.paths {
                let kind = if path.exists() {
                    ChangeKind::Added
                } else {
                    ChangeKind::Deleted
                };
                batch.push((kind, path.clone()));
            }
        }
        EventKind::Modify(_) => extend(batch, ChangeKind::Modified, &event.paths),
        EventKind::Access(_) | EventKind::Any | EventKind::Other => {}
    }
}

fn extend(batch: &mut Vec<(ChangeKind, PathBuf)>, kind: ChangeKind, paths: &[PathBuf]) {
    batch.extend(paths.iter().map(|path| (kind, path.clone())));
}

#[cfg(test)]
mod watch_tests;

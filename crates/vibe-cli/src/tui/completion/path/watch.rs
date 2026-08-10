//! Filesystem watching for the `@` completion index.
//!
//! The reference runs `watchfiles.watch` on a daemon thread with a 200 ms
//! step, waits half a second for the thread to report readiness, and joins it
//! for at most a second on stop
//! (`vibe/cli/autocompletion/file_indexer/watcher.py`). `watchfiles` is itself
//! built on the `notify` crate, so the platform backends and their event
//! categories are the same ones this controller reads.
//!
//! One difference is deliberate and invisible to the index: upstream the watch
//! thread calls back into the store under a lock, while here it hands each
//! batch to a channel the index drains before it answers a query. A batch
//! therefore reaches the store at the same point in the query sequence either
//! way, and the boundary stays single-threaded.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{RecursiveMode, Watcher};

/// The reference's `step=200`: changes are accumulated for this long after the
/// first one before the batch is delivered.
const WATCH_STEP: Duration = Duration::from_millis(200);
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        if let Ok(mut raised) = self.raised.lock() {
            *raised = true;
            self.changed.notify_all();
        }
    }

    fn is_set(&self) -> bool {
        self.raised.lock().is_ok_and(|raised| *raised)
    }

    /// Waits until the flag is raised or `timeout` elapses, reporting whether
    /// it was raised.
    fn wait(&self, timeout: Duration) -> bool {
        let Ok(raised) = self.raised.lock() else {
            return false;
        };
        self.changed
            .wait_timeout_while(raised, timeout, |raised| !*raised)
            .is_ok_and(|(raised, _)| *raised)
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
        if let Some(reason) = failure.lock().ok().and_then(|reason| reason.clone()) {
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

    let mut batch = Vec::new();
    loop {
        if stop.is_set() {
            break;
        }
        match incoming.recv_timeout(WATCH_STEP) {
            Ok(Ok(event)) => collect(&event, &mut batch),
            // A backend error after the watch started is not fatal upstream
            // either: the loop keeps reporting whatever still arrives.
            Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if batch.is_empty() {
            continue;
        }
        // The first change opens a step-long window, so a burst reaches the
        // store as one batch and the mass-change threshold sees the burst.
        let deadline = Instant::now().checked_add(WATCH_STEP);
        while let Some(remaining) = deadline
            .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
            .filter(|remaining| !remaining.is_zero())
        {
            match incoming.recv_timeout(remaining) {
                Ok(Ok(event)) => collect(&event, &mut batch),
                Ok(Err(_)) => {}
                Err(_) => break,
            }
        }
        if stop.is_set() {
            break;
        }
        if batches
            .send((root.to_path_buf(), std::mem::take(&mut batch)))
            .is_err()
        {
            break;
        }
    }
}

fn report(failure: &Arc<Mutex<Option<String>>>, ready: &Arc<Flag>, reason: &str) {
    if let Ok(mut failure) = failure.lock() {
        *failure = Some(reason.to_owned());
    }
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
mod tests {
    use super::*;

    use notify::event::{CreateKind, ModifyKind, RemoveKind};

    fn event(kind: EventKind, paths: &[&str]) -> notify::Event {
        notify::Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: notify::event::EventAttributes::default(),
        }
    }

    #[test]
    fn platform_events_map_onto_the_three_applied_categories() {
        let mut batch = Vec::new();
        collect(
            &event(EventKind::Create(CreateKind::File), &["/w/a"]),
            &mut batch,
        );
        collect(
            &event(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                &["/w/b"],
            ),
            &mut batch,
        );
        collect(
            &event(EventKind::Remove(RemoveKind::File), &["/w/c"]),
            &mut batch,
        );
        collect(
            &event(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &["/w/d"],
            ),
            &mut batch,
        );
        collect(
            &event(
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                &["/w/e"],
            ),
            &mut batch,
        );
        collect(
            &event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &["/w/f", "/w/g"],
            ),
            &mut batch,
        );
        collect(
            &event(
                EventKind::Access(notify::event::AccessKind::Read),
                &["/w/h"],
            ),
            &mut batch,
        );
        collect(&event(EventKind::Any, &["/w/i"]), &mut batch);

        assert_eq!(
            batch,
            vec![
                (ChangeKind::Added, PathBuf::from("/w/a")),
                (ChangeKind::Modified, PathBuf::from("/w/b")),
                (ChangeKind::Deleted, PathBuf::from("/w/c")),
                (ChangeKind::Deleted, PathBuf::from("/w/d")),
                (ChangeKind::Added, PathBuf::from("/w/e")),
                (ChangeKind::Deleted, PathBuf::from("/w/f")),
                (ChangeKind::Added, PathBuf::from("/w/g")),
            ]
        );
    }

    #[test]
    fn a_started_watch_is_not_restarted_for_the_same_root() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let root = std::fs::canonicalize(workspace.path()).expect("resolved root");
        let mut controller = WatchController::default();

        controller.start(&root).expect("the backend is available");
        assert!(controller.is_watching());
        let first = controller
            .active
            .as_ref()
            .map(|watch| watch.thread.thread().id());
        controller.start(&root).expect("the same root is kept");
        let second = controller
            .active
            .as_ref()
            .map(|watch| watch.thread.thread().id());
        assert_eq!(first, second, "the same root keeps the same thread");

        controller.stop();
        assert!(!controller.is_watching());
    }

    #[test]
    fn a_second_root_replaces_the_first_and_stop_returns_within_the_join_bound() {
        let first = tempfile::tempdir().expect("first workspace");
        let second = tempfile::tempdir().expect("second workspace");
        let first = std::fs::canonicalize(first.path()).expect("resolved root");
        let second = std::fs::canonicalize(second.path()).expect("resolved root");
        let mut controller = WatchController::default();

        controller.start(&first).expect("the backend is available");
        controller.start(&second).expect("the backend is available");
        assert_eq!(
            controller.active.as_ref().map(|watch| watch.root.clone()),
            Some(second)
        );

        let started = Instant::now();
        controller.stop();
        assert!(
            started.elapsed() < JOIN_TIMEOUT.saturating_mul(2),
            "stop returns within the join bound"
        );
        assert!(!controller.is_watching());
    }

    #[test]
    fn an_unwatchable_root_reports_its_cause_and_starts_nothing() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let missing = workspace.path().join("absent");
        let mut controller = WatchController::default();

        let failure = controller.start(&missing);

        assert!(failure.is_err(), "an absent root cannot be watched");
        assert!(!controller.is_watching());
        assert!(controller.next_batch().is_none());
    }

    #[test]
    fn a_written_file_reaches_a_delivered_batch() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let root = std::fs::canonicalize(workspace.path()).expect("resolved root");
        let mut controller = WatchController::default();
        controller.start(&root).expect("the backend is available");

        std::fs::write(root.join("appeared.txt"), "fixture").expect("watched write");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut observed = Vec::new();
        while Instant::now() < deadline && observed.is_empty() {
            if let Some((batch_root, changes)) = controller.next_batch() {
                assert_eq!(batch_root, root, "a batch names the root it was seen under");
                observed = changes;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        controller.stop();

        assert!(
            observed
                .iter()
                .any(|(kind, path)| *kind == ChangeKind::Added
                    && path.file_name().is_some_and(|name| name == "appeared.txt")),
            "the written file is reported as an addition: {observed:?}"
        );
    }
}

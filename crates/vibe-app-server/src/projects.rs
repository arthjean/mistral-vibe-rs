//! The Vibe Code project a session runs against, and the scheduled loops the
//! wire routes to the same family.
//!
//! [`store`] is the saved association between a directory and a project,
//! `projects.toml` under the vibe home, and [`links`] the session-less
//! `projectLinks/*` surface over it. Both read the vibe home of the workspace
//! that serves the call, so the store holds no state of its own here. The
//! `vibeCode/*` methods are listed here because the wire routes them to this
//! family, and are served by the session's own controller
//! (`crate::vibe_code`), which reads and writes the same store.
//!
//! [`loops`] is the exception, and stays here for one reason: the wire routes
//! `loops/*` to this same service, which owns their store and their schedule.
//! A scheduled loop touches a session only through the identifier a fire is
//! attributed to, and knows nothing about a project.

use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::params::{self, optional_u64, required_string};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

pub(crate) mod links;
mod loops;
pub(crate) mod store;

pub use loops::{LoopFire, LoopState, ScheduledLoop};
use loops::{default_loop_store, load_loops, next_loop_sequence};

pub const PROJECTS_METHODS: &[&str] = &[
    "loops/clear",
    "loops/create",
    "loops/delete",
    "loops/list",
    "projectLinks/create",
    "projectLinks/inspectRoot",
    "projectLinks/link",
    "projectLinks/list",
    "projectLinks/picker/load",
    "projectLinks/picker/loadMore",
    "projectLinks/resolveRoot",
    "projectLinks/save",
    "projectLinks/unlink",
    "vibeCode/projects/cancel",
    "vibeCode/projects/create",
    "vibeCode/projects/loadMore",
    "vibeCode/projects/open",
    "vibeCode/projects/recover",
    "vibeCode/projects/select",
    "vibeCode/projects/unlink",
    "vibeCode/teleport/cancel",
    "vibeCode/teleport/push/respond",
    "vibeCode/teleport/start",
];

/// Methods that read a directory's checkout or reach Vibe Code. They are
/// always dispatched on the asynchronous path, where the server answers them
/// against its workspace ([`links::dispatch`]), so a slow Git or cloud call
/// never blocks the caller's loop.
const DEFERRED_PROJECTS_METHODS: &[&str] = &[
    "projectLinks/create",
    "projectLinks/inspectRoot",
    "projectLinks/link",
    "projectLinks/list",
    "projectLinks/picker/load",
    "projectLinks/picker/loadMore",
    "projectLinks/resolveRoot",
    "projectLinks/save",
    "projectLinks/unlink",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectsNotification {
    pub method: String,
    pub params: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectsDispatch {
    pub result: BTreeMap<String, Value>,
    pub notifications: Vec<ProjectsNotification>,
}

impl ProjectsDispatch {
    fn result(entries: impl IntoIterator<Item = (impl Into<String>, Value)>) -> Self {
        Self {
            result: entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
            notifications: Vec::new(),
        }
    }
}

pub struct ProjectsSessionRemoval {
    session_id: String,
    loops: BTreeMap<String, ScheduledLoop>,
}

impl ProjectsSessionRemoval {
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn removed_loop_count(&self) -> usize {
        self.loops.len()
    }
}

#[derive(Clone)]
pub struct ProjectsService {
    loops: Arc<Mutex<BTreeMap<String, ScheduledLoop>>>,
    loop_store: PathBuf,
    loop_store_error: Option<String>,
    next_loop: Arc<AtomicU64>,
}

impl Default for ProjectsService {
    fn default() -> Self {
        let loop_store = default_loop_store();
        let (loops, loop_store_error) = match load_loops(&loop_store) {
            Ok(mut loops) => {
                for scheduled in loops.values_mut() {
                    if scheduled.state == LoopState::Running {
                        scheduled.state = LoopState::Scheduled;
                    }
                }
                (loops, None)
            }
            Err(error) => (BTreeMap::new(), Some(error.to_string())),
        };
        let next_loop = next_loop_sequence(&loops);
        Self {
            loops: Arc::new(Mutex::new(loops)),
            loop_store,
            loop_store_error,
            next_loop: Arc::new(AtomicU64::new(next_loop)),
        }
    }
}

impl ProjectsService {
    pub fn remove_session(&self, session_id: &str) -> Result<usize, ProjectsServiceError> {
        let removal = self.remove_session_transactional(session_id)?;
        Ok(removal.removed_loop_count())
    }

    pub fn remove_session_transactional(
        &self,
        session_id: &str,
    ) -> Result<ProjectsSessionRemoval, ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        let mut loops = self.lock_loops()?;
        let loops_before = loops.clone();

        let loop_ids = loops
            .iter()
            .filter(|(_, scheduled)| scheduled.session_id == session_id)
            .map(|(loop_id, _)| loop_id.clone())
            .collect::<Vec<_>>();
        let removed_loops = loop_ids
            .into_iter()
            .filter_map(|loop_id| loops.remove_entry(&loop_id))
            .collect();
        if let Err(error) = self.persist_loops(&loops) {
            *loops = loops_before;
            return Err(error);
        }
        Ok(ProjectsSessionRemoval {
            session_id: session_id.to_owned(),
            loops: removed_loops,
        })
    }

    pub fn restore_session(
        &self,
        removal: &ProjectsSessionRemoval,
    ) -> Result<(), ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        let mut loops = self.lock_loops()?;
        if removal
            .loops
            .keys()
            .any(|loop_id| loops.contains_key(loop_id))
        {
            return Err(ProjectsServiceError::Conflict(
                "projects session rollback collides with newer session state".to_owned(),
            ));
        }
        let loops_before = loops.clone();

        loops.extend(removal.loops.clone());
        if let Err(error) = self.persist_loops(&loops) {
            *loops = loops_before;
            return Err(error);
        }
        Ok(())
    }

    pub fn rebind_session(
        &self,
        old_session_id: &str,
        new_session_id: &str,
    ) -> Result<(), ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        if old_session_id == new_session_id {
            return Ok(());
        }
        let mut loops = self.lock_loops()?;
        let loops_before = loops.clone();

        for scheduled in loops.values_mut() {
            if scheduled.session_id == old_session_id {
                scheduled.session_id = new_session_id.to_owned();
            }
        }
        if let Err(error) = self.persist_loops(&loops) {
            *loops = loops_before;
            return Err(error);
        }
        Ok(())
    }

    /// Dispatches the methods that only touch the loop store.
    ///
    /// The `projectLinks/*` methods are answered by the server against its
    /// workspace; calling them here is a routing mistake.
    pub fn dispatch(
        &self,
        method: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        if DEFERRED_PROJECTS_METHODS.contains(&method) {
            return Err(ProjectsServiceError::Conflict(format!(
                "`{method}` reads a checkout and must be dispatched asynchronously"
            )));
        }
        match method {
            "loops/create" => self.loop_create(params),
            "loops/list" => self.loop_list(params),
            "loops/clear" => self.loop_clear(params),
            "loops/delete" => self.loop_delete(params),
            _ => Err(ProjectsServiceError::MethodNotFound(method.to_owned())),
        }
    }

    #[must_use]
    pub fn requires_deferred_dispatch(&self, method: &str) -> bool {
        DEFERRED_PROJECTS_METHODS.contains(&method)
    }
}

fn notification<const N: usize>(method: &str, entries: [(&str, Value); N]) -> ProjectsNotification {
    ProjectsNotification {
        method: method.to_owned(),
        params: entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    }
}

fn persist_json_atomically<T: Serialize>(
    path: &Path,
    value: &T,
    sequence: &AtomicU64,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let sequence = sequence.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    let temporary = path.with_file_name(format!(
        ".{file_name}.tmp-{}-{sequence}",
        std::process::id()
    ));
    let contents = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write_result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(&contents)?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = replace_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(source, destination)
}

impl From<params::ParamError> for ProjectsServiceError {
    fn from(error: params::ParamError) -> Self {
        Self::InvalidParams(error.message())
    }
}

#[derive(Debug, Error)]
pub enum ProjectsServiceError {
    #[error("unknown projects method `{0}`")]
    MethodNotFound(String),
    #[error("invalid parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    /// A scheduled loop the request cannot make or name: reference
    /// `LoopError`, which its handler answers as `invalid_params` with no
    /// issue list.
    #[error("{0}")]
    Loop(String),
    #[error("scheduled-loop persistence failed: {0}")]
    Persistence(std::io::Error),
    #[error("scheduled-loop persistence is unavailable: {0}")]
    PersistenceState(String),
    #[error("projects background task stopped unexpectedly")]
    BackgroundTask,
    #[error("projects state lock is poisoned")]
    StatePoisoned,
    #[error("JSON conversion failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod projects_tests;

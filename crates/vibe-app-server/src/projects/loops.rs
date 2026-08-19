//! Scheduled loops: a prompt a session re-runs on an interval.
//!
//! The feature is independent of everything else the service does. It keeps its
//! own store, its own identifiers and its own notion of what is due, and touches
//! a session only through the identifier a fire is attributed to.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    ProjectsDispatch, ProjectsNotification, ProjectsService, ProjectsServiceError, notification,
    optional_u64, persist_json_atomically, required_string,
};
use crate::host::{self, now_seconds};

pub(super) const MIN_LOOP_INTERVAL_SECONDS: u64 = 30;

pub(super) const MAX_LOOPS_PER_SESSION: usize = 50;

pub(super) static NEXT_LOOP_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopState {
    Scheduled,
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduledLoop {
    pub id: String,
    pub session_id: String,
    pub prompt: String,
    pub interval_seconds: u64,
    pub next_fire_at: u64,
    pub state: LoopState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopFire {
    pub loop_id: String,
    pub session_id: String,
    pub prompt: String,
    pub notice: ProjectsNotification,
}

pub(super) fn public_loop_value(scheduled: &ScheduledLoop) -> Value {
    json!({
        "id": scheduled.id,
        "prompt": scheduled.prompt,
        "intervalSeconds": scheduled.interval_seconds,
        "nextFireAt": scheduled.next_fire_at as f64,
    })
}

pub(super) fn parse_interval(value: &str) -> Result<u64, ProjectsServiceError> {
    let mut normalized = value.trim().to_ascii_lowercase();
    let unit = normalized.pop().ok_or_else(|| {
        ProjectsServiceError::InvalidParams(
            "interval must use `<digits><s|m|h|d>` syntax".to_owned(),
        )
    })?;
    let digits = normalized;
    let amount = digits.parse::<u64>().map_err(|_| {
        ProjectsServiceError::InvalidParams(
            "interval must use `<digits><s|m|h|d>` syntax".to_owned(),
        )
    })?;
    let multiplier = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        _ => {
            return Err(ProjectsServiceError::InvalidParams(
                "interval must use `<digits><s|m|h|d>` syntax".to_owned(),
            ));
        }
    };
    amount.checked_mul(multiplier).ok_or_else(|| {
        ProjectsServiceError::InvalidParams("interval exceeds the supported range".to_owned())
    })
}

pub(super) fn load_loops(
    path: &Path,
) -> Result<BTreeMap<String, ScheduledLoop>, ProjectsServiceError> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents).map_err(ProjectsServiceError::Json),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(ProjectsServiceError::Persistence(error)),
    }
}

pub(super) fn next_loop_sequence(loops: &BTreeMap<String, ScheduledLoop>) -> u64 {
    loops
        .keys()
        .filter_map(|id| {
            u64::from_str_radix(id, 16).ok().or_else(|| {
                id.strip_prefix("loop-")
                    .and_then(|suffix| suffix.parse::<u64>().ok())
            })
        })
        .max()
        .unwrap_or_default()
        .saturating_add(1)
}

pub(super) fn default_loop_store() -> PathBuf {
    host::vibe_home().join("scheduled-loops.json")
}

impl ProjectsService {
    pub fn with_loop_store(mut self, path: PathBuf) -> Result<Self, ProjectsServiceError> {
        let mut loops = load_loops(&path)?;
        for scheduled in loops.values_mut() {
            if scheduled.state == LoopState::Running {
                scheduled.state = LoopState::Scheduled;
            }
        }
        let next = next_loop_sequence(&loops);
        self.loops = Arc::new(Mutex::new(loops));
        self.loop_store = path;
        self.loop_store_error = None;
        self.next_loop = Arc::new(AtomicU64::new(next));
        Ok(self)
    }

    pub fn fire_loop(
        &self,
        loop_id: &str,
        now_seconds: u64,
        session_idle: bool,
    ) -> Result<LoopFire, ProjectsServiceError> {
        self.fire_loop_owned(loop_id, None, now_seconds, session_idle)
    }

    pub fn fire_loop_for_session(
        &self,
        loop_id: &str,
        session_id: &str,
        now_seconds: u64,
        session_idle: bool,
    ) -> Result<LoopFire, ProjectsServiceError> {
        self.fire_loop_owned(loop_id, Some(session_id), now_seconds, session_idle)
    }

    pub(super) fn fire_loop_owned(
        &self,
        loop_id: &str,
        session_id: Option<&str>,
        now_seconds: u64,
        session_idle: bool,
    ) -> Result<LoopFire, ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        if !session_idle {
            return Err(ProjectsServiceError::Conflict(
                "scheduled loop cannot fire while its session has active work".to_owned(),
            ));
        }
        let mut loops = self.lock_loops()?;
        let before = loops.clone();
        let scheduled = loops.get_mut(loop_id).ok_or_else(|| {
            ProjectsServiceError::NotFound(format!("loop `{loop_id}` was not found"))
        })?;
        if session_id.is_some_and(|session_id| scheduled.session_id != session_id) {
            return Err(ProjectsServiceError::NotFound(format!(
                "loop `{loop_id}` is not owned by session `{}`",
                session_id.unwrap_or_default()
            )));
        }
        if scheduled.state == LoopState::Running {
            return Err(ProjectsServiceError::Conflict(format!(
                "loop `{loop_id}` is already running"
            )));
        }
        if now_seconds < scheduled.next_fire_at {
            return Err(ProjectsServiceError::Conflict(format!(
                "loop `{loop_id}` is not due"
            )));
        }
        scheduled.state = LoopState::Running;
        scheduled.next_fire_at = now_seconds.saturating_add(scheduled.interval_seconds);
        let fire = LoopFire {
            loop_id: scheduled.id.clone(),
            session_id: scheduled.session_id.clone(),
            prompt: scheduled.prompt.clone(),
            notice: notification(
                "history/entryAdded",
                [(
                    "entry",
                    json!({
                        "id": format!("scheduled-loop:{loop_id}"),
                        "sessionId": scheduled.session_id,
                        "turnId": Value::Null,
                        "createdAt": now_seconds.saturating_mul(1_000),
                        "updatedAt": now_seconds.saturating_mul(1_000),
                        "generationStatus": "completed",
                        "relatedEntryId": Value::Null,
                        "type": "notice",
                        "level": "info",
                        "message": format!("Loop `{loop_id}` fired"),
                        "detail": {
                            "kind": "scheduled_loop_fired",
                            "loopId": loop_id,
                        },
                    }),
                )],
            ),
        };
        if let Err(error) = self.persist_loops(&loops) {
            *loops = before;
            return Err(error);
        }
        Ok(fire)
    }

    pub fn next_due_loop_id(
        &self,
        session_id: &str,
        now_seconds: u64,
    ) -> Result<Option<String>, ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        let loops = self.lock_loops()?;
        Ok(loops
            .values()
            .filter(|scheduled| {
                scheduled.session_id == session_id
                    && scheduled.state == LoopState::Scheduled
                    && scheduled.next_fire_at <= now_seconds
            })
            .min_by_key(|scheduled| (scheduled.next_fire_at, &scheduled.id))
            .map(|scheduled| scheduled.id.clone()))
    }

    pub fn finish_loop_fire(
        &self,
        loop_id: &str,
        _completed_at_seconds: u64,
    ) -> Result<(), ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        let mut loops = self.lock_loops()?;
        let before = loops.clone();
        let scheduled = loops.get_mut(loop_id).ok_or_else(|| {
            ProjectsServiceError::NotFound(format!("loop `{loop_id}` was not found"))
        })?;
        if scheduled.state != LoopState::Running {
            return Err(ProjectsServiceError::Conflict(format!(
                "loop `{loop_id}` is not running"
            )));
        }
        scheduled.state = LoopState::Scheduled;
        if let Err(error) = self.persist_loops(&loops) {
            *loops = before;
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn loop_create(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        let session_id = required_string(params, "sessionId")?;
        let prompt = required_string(params, "prompt")?;
        if prompt.starts_with('/') {
            return Err(ProjectsServiceError::InvalidParams(
                "scheduled-loop prompts cannot start with `/`".to_owned(),
            ));
        }
        let interval_seconds = parse_interval(required_string(params, "interval")?)?;
        let now_seconds = optional_u64(params, "nowSeconds")?.unwrap_or_else(now_seconds);
        if interval_seconds < MIN_LOOP_INTERVAL_SECONDS {
            return Err(ProjectsServiceError::InvalidParams(format!(
                "intervalSeconds must be at least {MIN_LOOP_INTERVAL_SECONDS}"
            )));
        }
        let mut loops = self.lock_loops()?;
        if loops
            .values()
            .filter(|scheduled| scheduled.session_id == session_id)
            .count()
            >= MAX_LOOPS_PER_SESSION
        {
            return Err(ProjectsServiceError::Conflict(format!(
                "session `{session_id}` already owns {MAX_LOOPS_PER_SESSION} scheduled loops"
            )));
        }
        let before = loops.clone();
        let id = format!(
            "{:08x}",
            self.next_loop.fetch_add(1, Ordering::Relaxed) & u64::from(u32::MAX)
        );
        let scheduled = ScheduledLoop {
            id: id.clone(),
            session_id: session_id.to_owned(),
            prompt: prompt.to_owned(),
            interval_seconds,
            next_fire_at: now_seconds.saturating_add(interval_seconds),
            state: LoopState::Scheduled,
        };
        loops.insert(id, scheduled.clone());
        if let Err(error) = self.persist_loops(&loops) {
            *loops = before;
            return Err(error);
        }
        Ok(ProjectsDispatch::result([(
            "loop",
            public_loop_value(&scheduled),
        )]))
    }

    pub(super) fn loop_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        let session_id = required_string(params, "sessionId")?;
        let loops = self.lock_loops()?;
        let items = loops
            .values()
            .filter(|scheduled| scheduled.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        Ok(ProjectsDispatch::result([(
            "loops",
            Value::Array(items.iter().map(public_loop_value).collect()),
        )]))
    }

    pub(super) fn loop_clear(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        let session_id = required_string(params, "sessionId")?;
        let mut loops = self.lock_loops()?;
        let before = loops.clone();
        let removed = loops
            .values()
            .filter(|scheduled| scheduled.session_id == session_id)
            .count();
        loops.retain(|_, scheduled| scheduled.session_id != session_id);
        if let Err(error) = self.persist_loops(&loops) {
            *loops = before;
            return Err(error);
        }
        Ok(ProjectsDispatch::result([("count", json!(removed))]))
    }

    pub(super) fn loop_delete(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<ProjectsDispatch, ProjectsServiceError> {
        self.ensure_loop_store_ready()?;
        let session_id = required_string(params, "sessionId")?;
        let loop_id = required_string(params, "loopId")?;
        let mut loops = self.lock_loops()?;
        let before = loops.clone();
        let scheduled = loops.get(loop_id).ok_or_else(|| {
            ProjectsServiceError::NotFound(format!("loop `{loop_id}` was not found"))
        })?;
        if scheduled.session_id != session_id {
            return Err(ProjectsServiceError::NotFound(format!(
                "loop `{loop_id}` is not owned by session `{session_id}`"
            )));
        }
        if scheduled.state == LoopState::Running {
            return Err(ProjectsServiceError::Conflict(format!(
                "loop `{loop_id}` cannot be deleted while running"
            )));
        }
        let removed = loops
            .remove(loop_id)
            .ok_or_else(|| ProjectsServiceError::NotFound(loop_id.to_owned()))?;
        if let Err(error) = self.persist_loops(&loops) {
            *loops = before;
            return Err(error);
        }
        Ok(ProjectsDispatch::result([(
            "loop",
            public_loop_value(&removed),
        )]))
    }

    pub(super) fn lock_loops(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, ScheduledLoop>>, ProjectsServiceError>
    {
        self.loops
            .lock()
            .map_err(|_| ProjectsServiceError::StatePoisoned)
    }

    pub(super) fn persist_loops(
        &self,
        loops: &BTreeMap<String, ScheduledLoop>,
    ) -> Result<(), ProjectsServiceError> {
        persist_json_atomically(&self.loop_store, loops, &NEXT_LOOP_TEMP_FILE)
            .map_err(ProjectsServiceError::Persistence)
    }

    pub(super) fn ensure_loop_store_ready(&self) -> Result<(), ProjectsServiceError> {
        self.loop_store_error.as_ref().map_or(Ok(()), |error| {
            Err(ProjectsServiceError::PersistenceState(error.clone()))
        })
    }
}

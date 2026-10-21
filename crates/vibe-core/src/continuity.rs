use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::events::ModelMessage;
use crate::storage::{HydratedSession, SessionStore, StorageError};

const CONTINUITY_KEY: &str = "__vibe_continuity";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallbackRoute {
    pub callback_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuitySnapshot {
    pub session_id: String,
    pub aliases: BTreeSet<String>,
    pub parent_session_id: Option<String>,
    pub child_session_ids: BTreeSet<String>,
    pub callback_routes: BTreeMap<String, CallbackRoute>,
    pub resources: BTreeSet<String>,
    pub watermark: u64,
    pub completed_operations: BTreeSet<String>,
    pub messages: Vec<ModelMessage>,
    pub statistics: BTreeMap<String, Value>,
}

/// The part of a snapshot a session's metadata carries under
/// [`CONTINUITY_KEY`]; the rest is the session's own metadata and log.
///
/// Every field defaults, so a session saved before anything was attached to it
/// reads as fresh state. A value that does not read as this shape reads the
/// same way rather than failing the session it belongs to.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PersistedContinuity {
    watermark: u64,
    completed_operations: BTreeSet<String>,
    child_session_ids: BTreeSet<String>,
    aliases: BTreeSet<String>,
    callback_routes: BTreeMap<String, CallbackRoute>,
    resources: BTreeSet<String>,
}

impl PersistedContinuity {
    /// The value `snapshot` persists under [`CONTINUITY_KEY`].
    fn value(snapshot: &ContinuitySnapshot) -> Result<Value, ContinuityError> {
        Ok(serde_json::to_value(Self {
            watermark: snapshot.watermark,
            completed_operations: snapshot.completed_operations.clone(),
            child_session_ids: snapshot.child_session_ids.clone(),
            aliases: snapshot.aliases.clone(),
            callback_routes: snapshot.callback_routes.clone(),
            resources: snapshot.resources.clone(),
        })?)
    }
}

#[derive(Clone)]
pub struct SessionContinuity {
    store: SessionStore,
    sessions: Arc<Mutex<BTreeMap<String, ContinuitySnapshot>>>,
}

impl SessionContinuity {
    #[must_use]
    pub fn new(store: SessionStore) -> Self {
        Self {
            store,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn attach(&self, hydrated: HydratedSession) -> Result<ContinuitySnapshot, ContinuityError> {
        let snapshot = snapshot_from_hydrated(hydrated);
        let mut sessions = self.lock_sessions()?;
        if sessions.contains_key(&snapshot.session_id) {
            return Err(ContinuityError::SessionConflict(snapshot.session_id));
        }
        sessions.insert(snapshot.session_id.clone(), snapshot.clone());
        Ok(snapshot)
    }

    pub fn refresh(
        &self,
        hydrated: HydratedSession,
    ) -> Result<ContinuitySnapshot, ContinuityError> {
        let incoming = snapshot_from_hydrated(hydrated);
        let mut sessions = self.lock_sessions()?;
        if let Some(snapshot) =
            snapshot_key(&sessions, &incoming.session_id).and_then(|key| sessions.get_mut(&key))
        {
            snapshot.parent_session_id = incoming.parent_session_id;
            snapshot.messages = incoming.messages;
            snapshot.statistics = incoming.statistics;
            return Ok(snapshot.clone());
        }
        sessions.insert(incoming.session_id.clone(), incoming.clone());
        Ok(incoming)
    }

    pub fn remove(&self, selector: &str) -> Result<bool, ContinuityError> {
        let mut sessions = self.lock_sessions()?;
        Ok(snapshot_key(&sessions, selector)
            .and_then(|key| sessions.remove(&key))
            .is_some())
    }

    pub fn reconnect(&self, selector: &str) -> Result<ContinuitySnapshot, ContinuityError> {
        if let Some(snapshot) = self.find_snapshot(selector)? {
            return Ok(snapshot);
        }
        // A saved session answers any selector; one held before its first
        // save answers only its exact identifier.
        let hydrated = match self.store.load(selector) {
            Err(StorageError::SessionNotFound(_)) => self.store.open(selector)?,
            loaded => loaded?,
        };
        self.attach(hydrated)
    }

    pub fn snapshot(&self, selector: &str) -> Result<ContinuitySnapshot, ContinuityError> {
        self.find_snapshot(selector)?
            .ok_or_else(|| ContinuityError::SessionNotAttached(selector.to_owned()))
    }

    pub fn bind_callback(
        &self,
        selector: &str,
        callback: CallbackRoute,
    ) -> Result<(), ContinuityError> {
        let mut sessions = self.lock_sessions()?;
        let snapshot = attached_mut(&mut sessions, selector)?;
        if snapshot
            .callback_routes
            .insert(callback.callback_id.clone(), callback)
            .is_some()
        {
            return Err(ContinuityError::CallbackConflict);
        }
        self.persist(snapshot)
    }

    pub fn add_child(&self, selector: &str, child_id: &str) -> Result<(), ContinuityError> {
        let mut sessions = self.lock_sessions()?;
        let snapshot = attached_mut(&mut sessions, selector)?;
        snapshot.child_session_ids.insert(child_id.to_owned());
        self.persist(snapshot)
    }

    pub fn add_resource(&self, selector: &str, resource: &str) -> Result<(), ContinuityError> {
        let mut sessions = self.lock_sessions()?;
        let snapshot = attached_mut(&mut sessions, selector)?;
        snapshot.resources.insert(resource.to_owned());
        self.persist(snapshot)
    }

    pub fn handoff(
        &self,
        selector: &str,
        new_session_id: &str,
        current_system_prompt: &str,
        current_config: BTreeMap<String, Value>,
        now_ms: u64,
    ) -> Result<ContinuitySnapshot, ContinuityError> {
        let mut sessions = self.lock_sessions()?;
        if sessions.contains_key(new_session_id) {
            return Err(ContinuityError::SessionConflict(new_session_id.to_owned()));
        }
        let (source_key, source) = snapshot_key(&sessions, selector)
            .and_then(|key| sessions.remove_entry(&key))
            .ok_or_else(|| ContinuityError::SessionNotAttached(selector.to_owned()))?;
        // The new session carries everything the old one held and answers
        // every name it did, plus the old identifier and the selector it was
        // reached by.
        let mut handed_off = source.clone();
        handed_off.session_id = new_session_id.to_owned();
        handed_off.aliases.insert(source.session_id.clone());
        handed_off.aliases.insert(selector.to_owned());
        handed_off.parent_session_id = Some(source.session_id.clone());
        let forked = PersistedContinuity::value(&handed_off).and_then(|persisted| {
            self.store
                .fork_with_config_overlay(
                    &source.session_id,
                    new_session_id,
                    current_system_prompt,
                    current_config,
                    BTreeMap::from([(CONTINUITY_KEY.to_owned(), persisted)]),
                    now_ms,
                )
                .map_err(ContinuityError::from)
        });
        let hydrated = match forked {
            Ok(hydrated) => hydrated,
            Err(error) => {
                sessions.insert(source_key, source);
                return Err(error);
            }
        };
        // The fork opened the new log with the current system prompt, so the
        // log and the statistics are read back from what it wrote.
        handed_off.messages = hydrated.messages;
        handed_off.statistics = hydrated.metadata.statistics;
        sessions.insert(new_session_id.to_owned(), handed_off.clone());
        Ok(handed_off)
    }

    pub fn accept_operation(
        &self,
        selector: &str,
        operation_id: &str,
        event_id: u64,
    ) -> Result<bool, ContinuityError> {
        let mut sessions = self.lock_sessions()?;
        let snapshot = attached_mut(&mut sessions, selector)?;
        if snapshot.completed_operations.contains(operation_id) {
            return Ok(false);
        }
        if event_id != snapshot.watermark.saturating_add(1) {
            return Err(ContinuityError::EventGap {
                expected: snapshot.watermark.saturating_add(1),
                actual: event_id,
            });
        }
        let mut accepted = snapshot.clone();
        accepted.watermark = event_id;
        accepted
            .completed_operations
            .insert(operation_id.to_owned());
        self.persist(&accepted)?;
        *snapshot = accepted;
        Ok(true)
    }

    pub fn rewind(
        &self,
        selector: &str,
        keep_messages: usize,
        statistics: BTreeMap<String, Value>,
        now_ms: u64,
    ) -> Result<ContinuitySnapshot, ContinuityError> {
        let current = self.snapshot(selector)?;
        let hydrated = self
            .store
            .rewind(&current.session_id, keep_messages, statistics, now_ms)?;
        let mut sessions = self.lock_sessions()?;
        let snapshot = attached_mut(&mut sessions, selector)?;
        snapshot.messages = hydrated.messages;
        snapshot.statistics = hydrated.metadata.statistics;
        Ok(snapshot.clone())
    }

    pub fn clear_history(
        &self,
        selector: &str,
        now_ms: u64,
    ) -> Result<ContinuitySnapshot, ContinuityError> {
        self.rewind(selector, 0, BTreeMap::new(), now_ms)
    }

    /// Writes `snapshot`'s continuity into its session's saved metadata.
    fn persist(&self, snapshot: &ContinuitySnapshot) -> Result<(), ContinuityError> {
        let mut hydrated = self.store.open(&snapshot.session_id)?;
        hydrated.metadata.config.insert(
            CONTINUITY_KEY.to_owned(),
            PersistedContinuity::value(snapshot)?,
        );
        self.store.update_metadata(&hydrated.metadata)?;
        Ok(())
    }

    fn find_snapshot(&self, selector: &str) -> Result<Option<ContinuitySnapshot>, ContinuityError> {
        let sessions = self.lock_sessions()?;
        Ok(snapshot_key(&sessions, selector)
            .and_then(|key| sessions.get(&key))
            .cloned())
    }

    fn lock_sessions(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, ContinuitySnapshot>>, ContinuityError>
    {
        self.sessions
            .lock()
            .map_err(|_| ContinuityError::StatePoisoned)
    }
}

fn snapshot_from_hydrated(hydrated: HydratedSession) -> ContinuitySnapshot {
    let persisted = hydrated
        .metadata
        .config
        .get(CONTINUITY_KEY)
        .and_then(|value| PersistedContinuity::deserialize(value).ok())
        .unwrap_or_default();
    ContinuitySnapshot {
        session_id: hydrated.metadata.id,
        aliases: persisted.aliases,
        parent_session_id: hydrated.metadata.parent_session_id,
        child_session_ids: persisted.child_session_ids,
        callback_routes: persisted.callback_routes,
        resources: persisted.resources,
        watermark: persisted.watermark,
        completed_operations: persisted.completed_operations,
        messages: hydrated.messages,
        statistics: hydrated.metadata.statistics,
    }
}

/// The key a selector resolves under: the session's own identifier, or any
/// alias it answers to.
fn snapshot_key(sessions: &BTreeMap<String, ContinuitySnapshot>, selector: &str) -> Option<String> {
    if sessions.contains_key(selector) {
        return Some(selector.to_owned());
    }
    sessions
        .iter()
        .find(|(_, snapshot)| snapshot.aliases.contains(selector))
        .map(|(key, _)| key.clone())
}

/// The attached session `selector` names, or the error saying it is not.
fn attached_mut<'a>(
    sessions: &'a mut BTreeMap<String, ContinuitySnapshot>,
    selector: &str,
) -> Result<&'a mut ContinuitySnapshot, ContinuityError> {
    snapshot_key(sessions, selector)
        .and_then(move |key| sessions.get_mut(&key))
        .ok_or_else(|| ContinuityError::SessionNotAttached(selector.to_owned()))
}

#[derive(Debug, Error)]
pub enum ContinuityError {
    #[error("continuity state lock is poisoned")]
    StatePoisoned,
    #[error("session `{0}` is already attached")]
    SessionConflict(String),
    #[error("session `{0}` is not attached")]
    SessionNotAttached(String),
    #[error("callback route already exists")]
    CallbackConflict,
    #[error("event gap: expected {expected}, received {actual}")]
    EventGap { expected: u64, actual: u64 },
    #[error("continuity state cannot be encoded: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ModelMessage;

    fn user(content: &str) -> ModelMessage {
        ModelMessage::user(content.to_owned())
    }

    #[test]
    fn handoff_preserves_routes_children_resources_and_durable_deduplication() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path()).with_pointer_key("test");
        let mut metadata = store
            .create("root-session", "/workspace", None, 1)
            .expect("root session");
        store
            .append_message(&mut metadata, &user("before"), 2)
            .expect("message appends");
        let continuity = SessionContinuity::new(store.clone());
        continuity
            .attach(store.load("root-session").expect("root loads"))
            .expect("root attaches");
        continuity
            .bind_callback(
                "root-session",
                CallbackRoute {
                    callback_id: "callback-1".to_owned(),
                    turn_id: "turn-1".to_owned(),
                },
            )
            .expect("callback binds");
        continuity
            .add_child("root-session", "child-1")
            .expect("child registers");
        continuity
            .add_resource("root-session", "terminal-1")
            .expect("resource registers");
        assert!(
            continuity
                .accept_operation("root-session", "operation-1", 1)
                .expect("operation accepted")
        );

        let handoff = continuity
            .handoff(
                "root-session",
                "next-session",
                "current prompt",
                BTreeMap::new(),
                10,
            )
            .expect("handoff commits");
        assert_eq!(handoff.parent_session_id.as_deref(), Some("root-session"));
        assert!(handoff.child_session_ids.contains("child-1"));
        assert!(handoff.callback_routes.contains_key("callback-1"));
        assert!(handoff.resources.contains("terminal-1"));
        assert_eq!(
            continuity
                .snapshot("root-session")
                .expect("old alias still resolves")
                .session_id,
            "next-session"
        );
        assert!(
            !continuity
                .accept_operation("root-session", "operation-1", 2)
                .expect("duplicate ignored")
        );

        let restarted = SessionContinuity::new(store.clone());
        let recovered = restarted
            .reconnect("next-session")
            .expect("new session reconnects");
        assert!(recovered.completed_operations.contains("operation-1"));
        assert_eq!(recovered.watermark, 1);
    }

    #[test]
    fn concurrent_operation_acceptance_is_serialized_and_deduplicated() {
        let temporary = tempfile::tempdir().expect("temporary sessions");
        let store = SessionStore::new(temporary.path());
        let metadata = store
            .create("root", "/workspace", None, 1)
            .expect("root session");
        let continuity = SessionContinuity::new(store);
        continuity
            .attach(HydratedSession {
                metadata,
                messages: Vec::new(),
                current_config: BTreeMap::new(),
            })
            .expect("continuity attaches");
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let continuity = continuity.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                continuity.accept_operation("root", "same-operation", 1)
            }));
        }
        barrier.wait();
        let mut outcomes = workers
            .into_iter()
            .map(|worker| worker.join().expect("worker joins").expect("operation"))
            .collect::<Vec<_>>();
        outcomes.sort_unstable();
        assert_eq!(outcomes, [false, true]);
        let reconnected = SessionContinuity::new(continuity.store.clone())
            .reconnect("root")
            .expect("durable reconnect");
        assert_eq!(reconnected.watermark, 1);
        assert!(reconnected.completed_operations.contains("same-operation"));
    }

    #[test]
    fn rewind_and_clear_match_persistence_after_restart() {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path());
        let mut metadata = store
            .create("root-session", "/workspace", None, 1)
            .expect("root session");
        for (index, content) in ["one", "two", "three"].into_iter().enumerate() {
            store
                .append_message(
                    &mut metadata,
                    &user(content),
                    u64::try_from(index).unwrap_or_default().saturating_add(2),
                )
                .expect("message appends");
        }
        let continuity = SessionContinuity::new(store.clone());
        continuity
            .attach(store.load("root-session").expect("session loads"))
            .expect("session attaches");
        let rewind = continuity
            .rewind("root-session", 1, BTreeMap::new(), 10)
            .expect("rewind persists");
        assert_eq!(rewind.messages, [user("one")]);
        continuity
            .clear_history("root-session", 11)
            .expect("history clears");
        assert!(
            store
                .load("root-session")
                .expect("restart loads")
                .messages
                .is_empty()
        );
    }
}

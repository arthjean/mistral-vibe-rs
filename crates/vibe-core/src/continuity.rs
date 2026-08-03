use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContinuityState {
    snapshot: ContinuitySnapshot,
    aliases: BTreeSet<String>,
}

#[derive(Clone)]
pub struct SessionContinuity {
    store: SessionStore,
    sessions: Arc<Mutex<BTreeMap<String, ContinuityState>>>,
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
        sessions.insert(
            snapshot.session_id.clone(),
            ContinuityState {
                snapshot: snapshot.clone(),
                aliases: snapshot.aliases.clone(),
            },
        );
        Ok(snapshot)
    }

    pub fn refresh(
        &self,
        hydrated: HydratedSession,
    ) -> Result<ContinuitySnapshot, ContinuityError> {
        let incoming = snapshot_from_hydrated(hydrated);
        let mut sessions = self.lock_sessions()?;
        if let Some(state) = find_state_mut(&mut sessions, &incoming.session_id) {
            state.snapshot.parent_session_id = incoming.parent_session_id;
            state.snapshot.messages = incoming.messages;
            state.snapshot.statistics = incoming.statistics;
            return Ok(state.snapshot.clone());
        }
        sessions.insert(
            incoming.session_id.clone(),
            ContinuityState {
                aliases: incoming.aliases.clone(),
                snapshot: incoming.clone(),
            },
        );
        Ok(incoming)
    }

    pub fn remove(&self, selector: &str) -> Result<bool, ContinuityError> {
        let mut sessions = self.lock_sessions()?;
        let Some(key) = state_key(&sessions, selector) else {
            return Ok(false);
        };
        sessions.remove(&key);
        Ok(true)
    }

    pub fn reconnect(&self, selector: &str) -> Result<ContinuitySnapshot, ContinuityError> {
        if let Some(snapshot) = self.find_snapshot(selector)? {
            return Ok(snapshot);
        }
        let hydrated = self.store.load(selector)?;
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
        let state = find_state_mut(&mut sessions, selector)
            .ok_or_else(|| ContinuityError::SessionNotAttached(selector.to_owned()))?;
        if state
            .snapshot
            .callback_routes
            .insert(callback.callback_id.clone(), callback)
            .is_some()
        {
            return Err(ContinuityError::CallbackConflict);
        }
        persist_continuity_state(&self.store, state)?;
        Ok(())
    }

    pub fn add_child(&self, selector: &str, child_id: &str) -> Result<(), ContinuityError> {
        let mut sessions = self.lock_sessions()?;
        let state = find_state_mut(&mut sessions, selector)
            .ok_or_else(|| ContinuityError::SessionNotAttached(selector.to_owned()))?;
        state.snapshot.child_session_ids.insert(child_id.to_owned());
        persist_continuity_state(&self.store, state)?;
        Ok(())
    }

    pub fn add_resource(&self, selector: &str, resource: &str) -> Result<(), ContinuityError> {
        let mut sessions = self.lock_sessions()?;
        let state = find_state_mut(&mut sessions, selector)
            .ok_or_else(|| ContinuityError::SessionNotAttached(selector.to_owned()))?;
        state.snapshot.resources.insert(resource.to_owned());
        persist_continuity_state(&self.store, state)?;
        Ok(())
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
        let source_key = state_key(&sessions, selector)
            .ok_or_else(|| ContinuityError::SessionNotAttached(selector.to_owned()))?;
        let source = sessions
            .get(&source_key)
            .ok_or_else(|| ContinuityError::SessionNotAttached(selector.to_owned()))?
            .snapshot
            .clone();
        let mut state = sessions
            .remove(&source_key)
            .ok_or_else(|| ContinuityError::SessionNotAttached(selector.to_owned()))?;
        let original_state = state.clone();
        state.aliases.insert(source.session_id.clone());
        state.aliases.insert(selector.to_owned());
        let mut messages = Vec::with_capacity(source.messages.len().saturating_add(1));
        messages.push(ModelMessage::System {
            content: current_system_prompt.to_owned(),
        });
        messages.extend(source.messages.clone());
        let prospective = ContinuitySnapshot {
            session_id: new_session_id.to_owned(),
            aliases: state.aliases.clone(),
            parent_session_id: Some(source.session_id.clone()),
            child_session_ids: source.child_session_ids.clone(),
            callback_routes: source.callback_routes.clone(),
            resources: source.resources.clone(),
            watermark: source.watermark,
            completed_operations: source.completed_operations.clone(),
            messages,
            statistics: source.statistics.clone(),
        };
        let hydrated = match self.store.fork_with_config_overlay(
            &source.session_id,
            new_session_id,
            current_system_prompt,
            current_config,
            BTreeMap::from([(
                CONTINUITY_KEY.to_owned(),
                continuity_value(&prospective, &state.aliases),
            )]),
            now_ms,
        ) {
            Ok(hydrated) => hydrated,
            Err(error) => {
                sessions.insert(source_key, original_state);
                return Err(error.into());
            }
        };
        state.snapshot = ContinuitySnapshot {
            session_id: new_session_id.to_owned(),
            aliases: state.aliases.clone(),
            parent_session_id: Some(source.session_id),
            child_session_ids: source.child_session_ids,
            callback_routes: source.callback_routes,
            resources: source.resources,
            watermark: source.watermark,
            completed_operations: source.completed_operations,
            messages: hydrated.messages.clone(),
            statistics: hydrated.metadata.statistics.clone(),
        };
        let snapshot = state.snapshot.clone();
        sessions.insert(new_session_id.to_owned(), state);
        Ok(snapshot)
    }

    pub fn accept_operation(
        &self,
        selector: &str,
        operation_id: &str,
        event_id: u64,
    ) -> Result<bool, ContinuityError> {
        let mut sessions = self.lock_sessions()?;
        let state = find_state_mut(&mut sessions, selector)
            .ok_or_else(|| ContinuityError::SessionNotAttached(selector.to_owned()))?;
        if state.snapshot.completed_operations.contains(operation_id) {
            return Ok(false);
        }
        if event_id != state.snapshot.watermark.saturating_add(1) {
            return Err(ContinuityError::EventGap {
                expected: state.snapshot.watermark.saturating_add(1),
                actual: event_id,
            });
        }
        let mut hydrated = self.store.load(&state.snapshot.session_id)?;
        let mut persisted = state.snapshot.clone();
        persisted.watermark = event_id;
        persisted
            .completed_operations
            .insert(operation_id.to_owned());
        hydrated.metadata.config.insert(
            CONTINUITY_KEY.to_owned(),
            continuity_value(&persisted, &state.aliases),
        );
        self.store.update_metadata(&hydrated.metadata)?;
        state.snapshot = persisted;
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
        let state = find_state_mut(&mut sessions, selector)
            .ok_or_else(|| ContinuityError::SessionNotAttached(selector.to_owned()))?;
        state.snapshot.messages = hydrated.messages;
        state.snapshot.statistics = hydrated.metadata.statistics;
        Ok(state.snapshot.clone())
    }

    pub fn clear_history(
        &self,
        selector: &str,
        now_ms: u64,
    ) -> Result<ContinuitySnapshot, ContinuityError> {
        self.rewind(selector, 0, BTreeMap::new(), now_ms)
    }

    fn find_snapshot(&self, selector: &str) -> Result<Option<ContinuitySnapshot>, ContinuityError> {
        let sessions = self.lock_sessions()?;
        Ok(find_state(&sessions, selector).map(|state| state.snapshot.clone()))
    }

    fn lock_sessions(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, ContinuityState>>, ContinuityError> {
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
        .cloned()
        .unwrap_or(Value::Null);
    let watermark = persisted
        .get("watermark")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let completed_operations = string_set(persisted.get("completedOperations"));
    let child_session_ids = string_set(persisted.get("childSessionIds"));
    let aliases = string_set(persisted.get("aliases"));
    let callback_routes = persisted
        .get("callbackRoutes")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    let resources = string_set(persisted.get("resources"));
    ContinuitySnapshot {
        session_id: hydrated.metadata.id,
        aliases,
        parent_session_id: hydrated.metadata.parent_session_id,
        child_session_ids,
        callback_routes,
        resources,
        watermark,
        completed_operations,
        messages: hydrated.messages,
        statistics: hydrated.metadata.statistics,
    }
}

fn string_set(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect()
}

fn continuity_value(snapshot: &ContinuitySnapshot, aliases: &BTreeSet<String>) -> Value {
    json!({
        "watermark": snapshot.watermark,
        "completedOperations": snapshot.completed_operations,
        "childSessionIds": snapshot.child_session_ids,
        "aliases": aliases,
        "callbackRoutes": snapshot.callback_routes,
        "resources": snapshot.resources,
    })
}

fn persist_continuity_state(
    store: &SessionStore,
    state: &ContinuityState,
) -> Result<(), ContinuityError> {
    let mut hydrated = store.load(&state.snapshot.session_id)?;
    hydrated.metadata.config.insert(
        CONTINUITY_KEY.to_owned(),
        continuity_value(&state.snapshot, &state.aliases),
    );
    store.update_metadata(&hydrated.metadata)?;
    Ok(())
}

fn state_key(sessions: &BTreeMap<String, ContinuityState>, selector: &str) -> Option<String> {
    if sessions.contains_key(selector) {
        Some(selector.to_owned())
    } else {
        sessions
            .iter()
            .find(|(_, state)| state.aliases.contains(selector))
            .map(|(key, _)| key.clone())
    }
}

fn find_state<'a>(
    sessions: &'a BTreeMap<String, ContinuityState>,
    selector: &str,
) -> Option<&'a ContinuityState> {
    sessions.get(selector).or_else(|| {
        sessions
            .values()
            .find(|state| state.aliases.contains(selector))
    })
}

fn find_state_mut<'a>(
    sessions: &'a mut BTreeMap<String, ContinuityState>,
    selector: &str,
) -> Option<&'a mut ContinuityState> {
    if sessions.contains_key(selector) {
        sessions.get_mut(selector)
    } else {
        sessions
            .values_mut()
            .find(|state| state.aliases.contains(selector))
    }
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
    #[error(transparent)]
    Storage(#[from] StorageError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ModelMessage;

    fn user(content: &str) -> ModelMessage {
        ModelMessage::User {
            content: content.to_owned(),
        }
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

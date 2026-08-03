//! Session storage keyed by a stable identifier.
//!
//! A session's public identifier changes when it is compacted or handed off, so
//! the map key never is: it stays the identifier the session was created with.
//! Every identifier a client may still hold, including superseded ones, resolves
//! to that key through the alias index.

use super::*;

#[derive(Default)]
pub(super) struct SessionRegistry {
    sessions: BTreeMap<String, SessionRuntime>,
    aliases: BTreeMap<String, String>,
}

impl SessionRegistry {
    /// Resolves any identifier the client may hold to the stable storage key.
    pub(super) fn key<'a>(&'a self, session_id: &'a str) -> Option<&'a str> {
        if self.sessions.contains_key(session_id) {
            return Some(session_id);
        }
        self.aliases
            .get(session_id)
            .map(String::as_str)
            .filter(|key| self.sessions.contains_key(*key))
    }

    pub(super) fn contains(&self, session_id: &str) -> bool {
        self.key(session_id).is_some()
    }

    pub(super) fn get(&self, session_id: &str) -> Option<&SessionRuntime> {
        let key = self.key(session_id)?.to_owned();
        self.sessions.get(&key)
    }

    pub(super) fn get_mut(&mut self, session_id: &str) -> Option<&mut SessionRuntime> {
        let key = self.key(session_id)?.to_owned();
        self.sessions.get_mut(&key)
    }

    /// Inserts a session under its current identifier.
    ///
    /// Any alias the session already carries is indexed, so a resumed session
    /// stays reachable through the selector the client used.
    pub(super) fn insert(&mut self, session: SessionRuntime) {
        let key = session.id.clone();
        for alias in &session.aliases {
            self.aliases.insert(alias.clone(), key.clone());
        }
        self.sessions.insert(key, session);
    }

    pub(super) fn remove(&mut self, session_id: &str) -> Option<SessionRuntime> {
        let key = self.key(session_id)?.to_owned();
        let session = self.sessions.remove(&key)?;
        self.aliases.retain(|_, target| target != &key);
        Some(session)
    }

    /// Gives a session a new public identifier without moving it.
    ///
    /// The previous identifier stays resolvable as an alias, which is what lets
    /// a client keep talking to a session it saw before a compaction.
    pub(super) fn rename(&mut self, session_id: &str, new_id: &str) -> Result<(), ServerError> {
        let key = self
            .key(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?
            .to_owned();
        if let Some(existing) = self.key(new_id)
            && existing != key
        {
            return Err(ServerError::SessionConflict(new_id.to_owned()));
        }
        let session = self
            .sessions
            .get_mut(&key)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        let previous_id = std::mem::replace(&mut session.id, new_id.to_owned());
        session.aliases.insert(previous_id.clone());
        self.aliases.insert(previous_id, key.clone());
        if new_id != key {
            self.aliases.insert(new_id.to_owned(), key);
        }
        Ok(())
    }

    /// Records an extra identifier that resolves to an existing session.
    pub(super) fn alias(&mut self, session_id: &str, alias: &str) {
        let Some(key) = self.key(session_id).map(ToOwned::to_owned) else {
            return;
        };
        if alias == key {
            return;
        }
        if let Some(session) = self.sessions.get_mut(&key) {
            session.aliases.insert(alias.to_owned());
        }
        self.aliases.insert(alias.to_owned(), key);
    }
}

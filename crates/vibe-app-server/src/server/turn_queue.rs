//! The turns a session holds for later, as reference `TurnQueue`
//! (`vibe/app_server/_turn_queue.py`) keeps them.
//!
//! A queued turn is the entries a client sent: context to inject and at most
//! one user message, which starts the turn once the session is free. The queue
//! also remembers every idempotency key it accepted, for the life of the
//! session, so a client that retries an enqueue gets the item it already made.

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// Reference `TURN_QUEUE_MAX_ITEMS`.
pub(crate) const MAX_ITEMS: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QueuedTurn {
    pub(crate) id: String,
    pub(crate) created_at: u64,
    /// The entries as `PublicQueuedTurn` publishes them.
    pub(crate) entries: Vec<Value>,
    /// The request the item came from, which a retried key is compared with.
    request: Value,
}

impl QueuedTurn {
    /// The user entry that starts the turn, when the item has one.
    pub(crate) fn user_entry(&self) -> Option<&Value> {
        self.entries
            .iter()
            .find(|entry| entry.get("role").and_then(Value::as_str) == Some("user"))
    }

    /// The context entries injected before the turn starts.
    pub(crate) fn context_entries(&self) -> impl Iterator<Item = &Value> {
        self.entries
            .iter()
            .filter(|entry| entry.get("role").and_then(Value::as_str) == Some("context"))
    }
}

/// Why the queue refused a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QueueRefusal {
    Full,
    IdempotencyConflict(String),
    ItemNotFound(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TurnQueue {
    items: Vec<QueuedTurn>,
    paused: bool,
    idempotency: BTreeMap<String, QueuedTurn>,
}

impl TurnQueue {
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Reference `PublicTurnQueue`.
    pub(crate) fn public(&self) -> Value {
        json!({
            "items": self
                .items
                .iter()
                .map(|item| json!({
                    "id": item.id,
                    "createdAt": item.created_at,
                    "entries": item.entries,
                }))
                .collect::<Vec<_>>(),
            "paused": self.paused,
            "maxItems": MAX_ITEMS,
        })
    }

    /// Queues `entries`, or answers the item a repeated key already made.
    /// The flag says whether the item is new.
    pub(crate) fn enqueue(
        &mut self,
        request: Value,
        entries: Vec<Value>,
        key: Option<&str>,
        id: String,
        now: u64,
    ) -> Result<(QueuedTurn, bool), QueueRefusal> {
        if let Some(key) = key
            && let Some(existing) = self.idempotency.get(key)
        {
            if existing.request != request {
                return Err(QueueRefusal::IdempotencyConflict(key.to_owned()));
            }
            return Ok((existing.clone(), false));
        }
        if self.items.len() >= MAX_ITEMS {
            return Err(QueueRefusal::Full);
        }
        let item = QueuedTurn {
            id,
            created_at: now,
            entries,
            request,
        };
        self.items.push(item.clone());
        if let Some(key) = key {
            self.idempotency.insert(key.to_owned(), item.clone());
        }
        Ok((item, true))
    }

    /// Replaces the entries of a queued item, keeping its place, its
    /// identifier and its creation time.
    pub(crate) fn replace(
        &mut self,
        id: &str,
        request: Value,
        entries: Vec<Value>,
        key: Option<&str>,
    ) -> Result<(QueuedTurn, bool), QueueRefusal> {
        let index = self
            .items
            .iter()
            .position(|item| item.id == id)
            .ok_or_else(|| QueueRefusal::ItemNotFound(id.to_owned()))?;
        if let Some(key) = key
            && let Some(existing) = self.idempotency.get(key)
        {
            if existing.request != request || existing.id != id {
                return Err(QueueRefusal::IdempotencyConflict(key.to_owned()));
            }
            return Ok((existing.clone(), false));
        }
        let item = QueuedTurn {
            entries,
            request,
            ..self.items[index].clone()
        };
        self.items[index] = item.clone();
        if let Some(key) = key {
            self.idempotency.insert(key.to_owned(), item.clone());
        }
        Ok((item, true))
    }

    /// The item that runs next, unless the queue is paused.
    pub(crate) fn peek_next(&self) -> Option<&QueuedTurn> {
        if self.paused {
            return None;
        }
        self.items.first()
    }

    pub(crate) fn pop_next(&mut self) -> Option<QueuedTurn> {
        self.peek_next()?;
        let item = self.items.remove(0);
        self.reset_pause_if_empty();
        Some(item)
    }

    pub(crate) fn remove(&mut self, id: &str) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == id) else {
            return false;
        };
        self.items.remove(index);
        self.reset_pause_if_empty();
        true
    }

    pub(crate) fn pause(&mut self) -> bool {
        if self.items.is_empty() || self.paused {
            return false;
        }
        self.paused = true;
        true
    }

    pub(crate) fn resume(&mut self) -> bool {
        std::mem::replace(&mut self.paused, false)
    }

    fn reset_pause_if_empty(&mut self) {
        if self.items.is_empty() {
            self.paused = false;
        }
    }
}

#[cfg(test)]
mod turn_queue_tests;

//! Publishing configuration changes to interested components.
//!
//! A write reports which keys moved, so a component can react to the field it
//! cares about instead of reloading the whole runtime. Reference
//! `vibe/core/config/event_bus.py` for the bus and its key-matching rule, and
//! `ConfigOrchestrator._changed_keys_between` for the diff.
//!
//! The documents an event carries are redacted, so a subscriber, a log line and
//! a crash report never see a value from a key the sensitive-key rule covers.
//! Key names are safe: they are field names, not values.

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value as JsonValue;

/// What one applied change did to the effective configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChangeEvent {
    /// Slash-separated paths whose value differs, deepest differing node first
    /// by construction and sorted for a stable read.
    pub changed_keys: BTreeSet<String>,
    /// The redacted effective document before the change.
    pub before: JsonValue,
    /// The redacted effective document after it.
    pub after: JsonValue,
    /// Why the caller said it was writing.
    pub reason: String,
}

type Callback = Arc<dyn Fn(&ConfigChangeEvent) + Send + Sync>;

struct Subscriber {
    id: u64,
    /// `None` subscribes to every change.
    keys: Option<BTreeSet<String>>,
    callback: Callback,
}

/// Delivers [`ConfigChangeEvent`]s to the components that asked for them.
#[derive(Default)]
pub struct ConfigChangeBus {
    subscribers: Mutex<Vec<Subscriber>>,
    next_id: AtomicU64,
}

impl ConfigChangeBus {
    /// Registers `callback`, filtered to `keys` when one is given.
    ///
    /// Returns the handle that cancels it. Dropping the handle keeps the
    /// subscription, as dropping the reference's unsubscribe callable does;
    /// [`ConfigSubscription::unsubscribe`] is the only way to stop delivery.
    pub fn subscribe(
        self: &Arc<Self>,
        keys: Option<BTreeSet<String>>,
        callback: impl Fn(&ConfigChangeEvent) + Send + Sync + 'static,
    ) -> ConfigSubscription {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.push(Subscriber {
                id,
                keys,
                callback: Arc::new(callback),
            });
        }
        ConfigSubscription {
            bus: Arc::clone(self),
            id,
        }
    }

    /// Delivers `event` to every matching subscriber.
    ///
    /// A subscriber that panics is contained: the remaining ones are still
    /// notified, because one component's failure must not silence the others or
    /// take down the write that produced the event.
    pub fn publish(&self, event: &ConfigChangeEvent) {
        let Ok(subscribers) = self.subscribers.lock() else {
            return;
        };
        let matching: Vec<Callback> = subscribers
            .iter()
            .filter(|subscriber| matches_event(subscriber.keys.as_ref(), &event.changed_keys))
            .map(|subscriber| Arc::clone(&subscriber.callback))
            .collect();
        drop(subscribers);
        for callback in matching {
            let _ = catch_unwind(AssertUnwindSafe(|| callback(event)));
        }
    }

    fn remove(&self, id: u64) {
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.id != id);
        }
    }
}

/// A live subscription. Cancelling twice, or after the bus has no other
/// subscribers, is not an error.
pub struct ConfigSubscription {
    bus: Arc<ConfigChangeBus>,
    id: u64,
}

impl ConfigSubscription {
    /// Stops delivery to this subscriber.
    pub fn unsubscribe(self) {
        self.bus.remove(self.id);
    }
}

fn matches_event(keys: Option<&BTreeSet<String>>, changed: &BTreeSet<String>) -> bool {
    let Some(keys) = keys else {
        return true;
    };
    keys.iter()
        .any(|key| changed.iter().any(|change| key_matches(key, change)))
}

/// Whether a subscription key covers a changed key.
///
/// A key matches its own ancestors and descendants, and nothing else: `models`
/// matches `models/active` and the reverse, while `model` never matches
/// `models`. Reference `_key_matches`.
fn key_matches(subscription_key: &str, changed_key: &str) -> bool {
    subscription_key == changed_key
        || changed_key.starts_with(&format!("{subscription_key}/"))
        || subscription_key.starts_with(&format!("{changed_key}/"))
}

/// The slash-separated paths at which `before` and `after` differ.
///
/// Reference `_changed_keys_between`: the walk descends while both sides are
/// objects and records the path where they stop agreeing, so replacing a whole
/// table reports the table rather than each of its leaves.
#[must_use]
pub fn changed_keys(before: &JsonValue, after: &JsonValue) -> BTreeSet<String> {
    let mut changed = BTreeSet::new();
    collect_changed_keys(Some(before), Some(after), &mut Vec::new(), &mut changed);
    changed
}

fn collect_changed_keys(
    before: Option<&JsonValue>,
    after: Option<&JsonValue>,
    path: &mut Vec<String>,
    changed: &mut BTreeSet<String>,
) {
    if before == after {
        return;
    }
    if let (Some(JsonValue::Object(before)), Some(JsonValue::Object(after))) = (before, after) {
        let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
        for key in keys {
            path.push(key.clone());
            collect_changed_keys(before.get(key), after.get(key), path, changed);
            path.pop();
        }
        return;
    }
    changed.insert(path.join("/"));
}

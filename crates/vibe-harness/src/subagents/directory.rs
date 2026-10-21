//! The children a parent session publishes, as its history shows them.
//!
//! Reference `_state_with_child_summaries` and `_observe_child_metadata` in
//! the unified adapter (`vibe/app_server/_unified_harness_backend_adapter.py`).

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// The children summaries a parent publishes, from what its history shows
/// of each child.
#[derive(Debug, Default, Clone)]
pub struct ChildDirectory {
    /// Each child's public session, as it last published it.
    pub sessions: BTreeMap<String, Value>,
    identities: BTreeMap<String, Identity>,
    stopped: BTreeMap<String, u64>,
    observed_stops: Vec<String>,
}

#[derive(Debug, Clone)]
struct Identity {
    name: String,
    agent_type: Value,
    spawned_at: u64,
}

impl ChildDirectory {
    /// The children the observed history spawned.
    #[must_use]
    pub fn known(&self) -> Vec<String> {
        self.identities.keys().cloned().collect()
    }

    /// Reads the links `history` (wire entries) shows: spawns that name
    /// their child, and stops that close one.
    pub fn observe(&mut self, history: &[Value]) {
        let mut preceding: Vec<String> = Vec::new();
        for entry in history {
            if entry["type"] != "effect" {
                continue;
            }
            let detail = &entry["detail"];
            if detail["kind"] == "subagent"
                && detail["toolName"] == "subagent.spawn"
                && let Some(child) = detail["childSessionId"].as_str()
                && !detail["input"].is_null()
            {
                let name = detail["display"]["message"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .unwrap_or("subagent")
                    .to_owned();
                self.identities.insert(
                    child.to_owned(),
                    Identity {
                        name,
                        agent_type: detail["input"]["agent"].clone(),
                        spawned_at: entry["createdAt"].as_u64().unwrap_or_default(),
                    },
                );
                preceding.push(child.to_owned());
                continue;
            }
            let id = entry["id"].as_str().unwrap_or_default();
            if self.observed_stops.iter().any(|seen| seen == id)
                || detail["kind"] != "tool"
                || detail["toolName"] != "subagent.stop"
                || entry["state"]["status"] != "completed"
            {
                continue;
            }
            let Some(agent_name) = detail["input"]["agentName"].as_str() else {
                continue;
            };
            let created_at = entry["createdAt"].as_u64().unwrap_or_default();
            let candidate = self
                .identities
                .iter()
                .filter(|(child, identity)| {
                    identity.name == agent_name
                        && (identity.spawned_at < created_at || preceding.contains(child))
                        && !self.stopped.contains_key(*child)
                })
                .map(|(child, identity)| (identity.spawned_at, child.clone()))
                .max();
            if let Some((_, child)) = candidate {
                self.stopped
                    .insert(child, entry["updatedAt"].as_u64().unwrap_or_default());
                self.observed_stops.push(id.to_owned());
            }
        }
    }

    /// Every child's summary, oldest first.
    #[must_use]
    pub fn summaries(&self) -> Vec<Value> {
        let mut children: Vec<Value> = self
            .sessions
            .iter()
            .map(|(id, session)| {
                let identity = self.identities.get(id);
                let stopped_at = self.stopped.get(id).copied();
                let name = identity.map_or_else(
                    || {
                        session["title"]
                            .as_str()
                            .map_or_else(|| id.chars().take(8).collect(), ToOwned::to_owned)
                    },
                    |identity| identity.name.clone(),
                );
                let updated_at = session["updatedAt"].as_u64().unwrap_or_default();
                let token_usage = if session["tokenUsage"].is_null() {
                    json!({"inputTokens": 0, "outputTokens": 0, "totalTokens": 0})
                } else {
                    session["tokenUsage"].clone()
                };
                json!({
                    "agentType": identity.map_or_else(|| json!("subagent"), |identity| identity.agent_type.clone()),
                    "contextUsage": session["contextUsage"],
                    "createdAt": session["createdAt"],
                    "id": id,
                    "name": name,
                    "status": if stopped_at.is_some() { json!({"type": "archived"}) } else { session["status"].clone() },
                    "tokenUsage": token_usage,
                    "updatedAt": updated_at.max(stopped_at.unwrap_or_default()),
                })
            })
            .collect();
        children.sort_by(|left, right| {
            (left["createdAt"].as_u64(), left["id"].as_str())
                .cmp(&(right["createdAt"].as_u64(), right["id"].as_str()))
        });
        children
    }
}

#[cfg(test)]
mod directory_tests;

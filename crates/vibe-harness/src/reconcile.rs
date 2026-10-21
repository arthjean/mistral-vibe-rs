//! Notifications derived from two successive wire states.
//!
//! Reference `reconcile_snapshot` (`vibe/app_server/events.py`) and the
//! adapter's `_translate_snapshot_update`, `_interleave_stats`,
//! `_advance_stats` and `_record_unseen_transition`
//! (`vibe/app_server/_unified_harness_backend_adapter.py`): every snapshot the
//! harness publishes is diffed against the last one a client saw, the
//! differences go out as entry, session and turn notifications, and the
//! session's accounting follows behind them.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibe_core::clock::now_millis;

use crate::patch::{make_json_patch, patch_json};

/// Paths whose string growth is published as an append.
const STREAMING_TEXT_PATHS: &[&str] = &["/content/0/text", "/text", "/state/outputText"];

/// One notification, ready for the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: &'static str,
    pub params: Value,
}

/// Reference `AgentStatsSnapshot` (`vibe/app_server/models.py`): a session's
/// accounting as `session/statsUpdated` and `runtime/read` publish it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentStatsSnapshot {
    pub steps: u64,
    pub session_prompt_tokens: u64,
    pub session_completion_tokens: u64,
    pub session_cached_tokens: u64,
    pub input_price_per_million: f64,
    pub output_price_per_million: f64,
    pub cached_input_price_per_million: Option<f64>,
    pub tool_calls_agreed: u64,
    pub tool_calls_rejected: u64,
    pub tool_calls_failed: u64,
    pub tool_calls_succeeded: u64,
    pub context_tokens: u64,
    pub last_turn_prompt_tokens: u64,
    pub last_turn_completion_tokens: u64,
    pub last_turn_cached_tokens: u64,
    /// Seconds.
    pub last_turn_duration: f64,
    pub tokens_per_second: f64,
}

impl AgentStatsSnapshot {
    /// A session's opening accounting, priced by its model.
    #[must_use]
    pub fn priced(input: f64, output: f64, cached: Option<f64>) -> Self {
        Self {
            input_price_per_million: input,
            output_price_per_million: output,
            cached_input_price_per_million: cached,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn to_json(&self) -> Value {
        json!(self)
    }

    /// Reference `_seed_stats`: a resumed session adopts its spend and the
    /// context its last call left.
    pub fn seed(&mut self, state: &Value) {
        let session = &state["session"];
        if let Some(usage) = session["tokenUsage"].as_object() {
            self.session_prompt_tokens = usage["inputTokens"].as_u64().unwrap_or(0);
            self.session_completion_tokens = usage["outputTokens"].as_u64().unwrap_or(0);
        }
        if let Some(context) = context_tokens(&session["contextUsage"]) {
            self.context_tokens = context;
        }
    }
}

fn context_tokens(usage: &Value) -> Option<u64> {
    let usage = usage.as_object()?;
    Some(usage["inputTokens"].as_u64().unwrap_or(0) + usage["outputTokens"].as_u64().unwrap_or(0))
}

/// Translates successive wire states into notifications, keeping the event
/// counter and the accounting a client reads.
#[derive(Debug, Clone)]
pub struct Translator {
    pub previous: Value,
    pub event_id: u64,
    pub stats: AgentStatsSnapshot,
    pub context_window: u64,
}

impl Translator {
    #[must_use]
    pub fn new(
        state: Value,
        event_id: u64,
        stats: AgentStatsSnapshot,
        context_window: u64,
    ) -> Self {
        Self {
            previous: state,
            event_id,
            stats,
            context_window,
        }
    }

    fn next_id(&mut self) -> u64 {
        self.event_id += 1;
        self.event_id
    }

    fn session_id(&self) -> Value {
        self.previous["session"]["id"].clone()
    }

    /// Reference `_record_unseen_transition` then `_translate_snapshot_update`.
    /// The answer says whether the step turned the session unseen, which the
    /// caller persists with the session's marks.
    pub fn translate(&mut self, mut current: Value) -> (Vec<Notification>, bool) {
        current["eventId"] = json!(self.event_id);
        let marked_unseen = should_mark_unseen(&self.previous, &current);
        if marked_unseen {
            current["session"]["isUnseen"] = json!(true);
        }
        let snapshot_changed = self.previous["retrying"] != current["retrying"]
            || self.previous["backgroundProcesses"] != current["backgroundProcesses"];
        let events = reconcile(&self.previous, &current);
        let steps = events
            .iter()
            .filter(|event| {
                event.method == "history/entryAdded"
                    && event.params["entry"]["type"] == "message"
                    && matches!(
                        event.params["entry"]["role"].as_str(),
                        Some("user" | "assistant")
                    )
            })
            .count() as u64;
        let stats = self.advanced_stats(&current, steps);
        let mut updated = stats != self.stats;
        self.stats = stats;
        let mut out = Vec::new();
        for mut event in events {
            if event.method == "turn/completed" && updated {
                out.push(self.stats_event());
                updated = false;
            }
            let id = self.next_id();
            stamp(&mut event.params, id);
            out.push(event);
        }
        if updated {
            out.push(self.stats_event());
        }
        current["eventId"] = json!(self.event_id);
        if snapshot_changed {
            let id = self.next_id();
            current["eventId"] = json!(id);
            out.push(Notification {
                method: "session/snapshot",
                params: json!({
                    "eventId": id,
                    "sessionId": current["session"]["id"],
                    "emittedAt": now_millis(),
                    "state": current,
                }),
            });
            if current["retrying"].is_object() {
                out.push(retrying_event(
                    &current["session"]["id"],
                    &current["retrying"],
                ));
            }
        }
        self.previous = current;
        (out, marked_unseen)
    }

    /// A session patch the host publishes on its own (a bump, a pin, a seen
    /// mark): reference `_publish_session_metadata`.
    pub fn publish_session_patch(&mut self, operations: &[(&str, Value)]) -> Option<Notification> {
        let before = self.previous["session"].clone();
        let mut patch = Vec::new();
        for (path, value) in operations {
            let key = path.trim_start_matches('/');
            self.previous["session"][key] = value.clone();
            patch.push(json!({"op": "replace", "path": path, "value": value}));
        }
        if self.previous["session"] == before {
            return None;
        }
        let id = self.next_id();
        self.previous["eventId"] = json!(id);
        Some(Notification {
            method: "session/updated",
            params: json!({
                "eventId": id,
                "sessionId": self.session_id(),
                "emittedAt": now_millis(),
                "patch": patch,
            }),
        })
    }

    /// Reference `_child_summary_events`: one event for every child whose
    /// summary differs from the one last published, in `children` order.
    pub fn child_sessions_updated(&mut self, children: Vec<Value>) -> Vec<Notification> {
        let prior = self.previous["childSessions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut events = Vec::new();
        for child in &children {
            if prior.iter().any(|known| known == child) {
                continue;
            }
            let id = self.next_id();
            events.push(Notification {
                method: "session/childSessionUpdated",
                params: json!({
                    "eventId": id,
                    "sessionId": self.session_id(),
                    "emittedAt": now_millis(),
                    "childSession": child,
                }),
            });
        }
        if self.previous.is_object() {
            self.previous["childSessions"] = Value::Array(children);
            if !events.is_empty() {
                self.previous["eventId"] = json!(self.event_id);
            }
        }
        events
    }

    /// Reference `_callback_events` for `callback_requested`: the callback
    /// joins the history the client sees.
    pub fn callback_requested(&mut self, entry: Value) -> Notification {
        let id = self.next_id();
        if let Some(history) = self.previous["history"].as_array_mut() {
            history.retain(|known| known["id"] != entry["id"]);
            history.push(entry.clone());
        }
        Notification {
            method: "history/entryAdded",
            params: json!({
                "eventId": id,
                "emittedAt": now_millis(),
                "sessionId": entry["sessionId"],
                "turnId": entry["turnId"],
                "entry": entry,
            }),
        }
    }

    /// Reference `_callback_events` for `callback_resolved`: the callback
    /// completes, its whole state replaced.
    pub fn callback_resolved(&mut self, entry: Value) -> Notification {
        let id = self.next_id();
        if let Some(known) = self.previous["history"]
            .as_array_mut()
            .and_then(|history| history.iter_mut().find(|known| known["id"] == entry["id"]))
        {
            *known = entry.clone();
        }
        Notification {
            method: "history/entryUpdated",
            params: json!({
                "eventId": id,
                "emittedAt": now_millis(),
                "sessionId": entry["sessionId"],
                "turnId": entry["turnId"],
                "entryId": entry["id"],
                "patch": [
                    {"op": "replace", "path": "/generationStatus", "value": "completed"},
                    {"op": "replace", "path": "/state", "value": entry["state"]},
                ],
            }),
        }
    }

    fn stats_event(&mut self) -> Notification {
        let id = self.next_id();
        Notification {
            method: "session/statsUpdated",
            params: json!({
                "eventId": id,
                "sessionId": self.session_id(),
                "emittedAt": now_millis(),
                "stats": self.stats.to_json(),
                "contextWindow": self.context_window,
            }),
        }
    }

    /// Reference `_advance_stats`.
    fn advanced_stats(&self, current: &Value, steps: u64) -> AgentStatsSnapshot {
        let mut stats = self.stats.clone();
        stats.steps += steps;
        let session = &current["session"];
        if let Some(context) = context_tokens(&session["contextUsage"]) {
            stats.context_tokens = context;
        }
        let Some(usage) = session["tokenUsage"].as_object() else {
            return stats;
        };
        let before = self.previous["session"]["tokenUsage"].as_object();
        let read = |object: Option<&serde_json::Map<String, Value>>, key: &str| {
            object.and_then(|usage| usage[key].as_u64()).unwrap_or(0)
        };
        let prompt = read(Some(usage), "inputTokens");
        let completion = read(Some(usage), "outputTokens");
        let added_prompt = prompt.saturating_sub(read(before, "inputTokens"));
        let added_completion = completion.saturating_sub(read(before, "outputTokens"));
        stats.session_prompt_tokens = prompt;
        stats.session_completion_tokens = completion;
        if added_prompt > 0 || added_completion > 0 {
            stats.last_turn_prompt_tokens = added_prompt;
            stats.last_turn_completion_tokens = added_completion;
        }
        stats
    }
}

fn stamp(params: &mut Value, event_id: u64) {
    params["eventId"] = json!(event_id);
    params["emittedAt"] = json!(now_millis());
}

fn retrying_event(session_id: &Value, retrying: &Value) -> Notification {
    // The notification names what is retrying; when the next attempt is due
    // lives in the state alone.
    let params = json!({
        "sessionId": session_id,
        "category": retrying["category"],
        "detail": retrying["detail"],
    });
    Notification {
        method: "turn/retrying",
        params,
    }
}

/// Reference `reconcile_snapshot`, without the snapshot and child events the
/// adapter drops.
#[must_use]
pub fn reconcile(previous: &Value, current: &Value) -> Vec<Notification> {
    let mut events = Vec::new();
    let session_id = &current["session"]["id"];
    if previous["session"] != current["session"] {
        let patch = make_json_patch(&previous["session"], &current["session"], &[]);
        events.push(Notification {
            method: "session/updated",
            params: json!({"sessionId": session_id, "patch": patch_json(&patch)}),
        });
    }
    let empty = Vec::new();
    let prior_entries = previous["history"].as_array().unwrap_or(&empty);
    for entry in current["history"].as_array().unwrap_or(&empty) {
        let prior = prior_entries
            .iter()
            .find(|prior| prior["id"] == entry["id"]);
        match prior {
            None => events.push(Notification {
                method: "history/entryAdded",
                params: json!({
                    "sessionId": entry["sessionId"],
                    "turnId": entry["turnId"],
                    "entry": entry,
                }),
            }),
            Some(prior) if prior == entry => {}
            Some(prior) => {
                let patch = make_json_patch(prior, entry, STREAMING_TEXT_PATHS);
                events.push(Notification {
                    method: "history/entryUpdated",
                    params: json!({
                        "sessionId": entry["sessionId"],
                        "turnId": entry["turnId"],
                        "entryId": entry["id"],
                        "patch": patch_json(&patch),
                    }),
                });
            }
        }
    }
    let prior_turns = previous["turns"].as_array().unwrap_or(&empty);
    for turn in current["turns"].as_array().unwrap_or(&empty) {
        let prior = prior_turns.iter().find(|prior| prior["id"] == turn["id"]);
        if prior == Some(turn) {
            continue;
        }
        let method = if turn["status"] == "in_progress" {
            "turn/started"
        } else {
            "turn/completed"
        };
        events.push(Notification {
            method,
            params: json!({"sessionId": turn["sessionId"], "turn": turn}),
        });
    }
    if previous["turnQueue"] != current["turnQueue"] {
        events.push(Notification {
            method: "turn/queueUpdated",
            params: json!({"sessionId": session_id, "queue": current["turnQueue"]}),
        });
    }
    events
}

/// Reference `should_mark_session_unseen`, over wire states.
#[must_use]
pub fn should_mark_unseen(previous: &Value, current: &Value) -> bool {
    let status = |state: &Value| state["session"]["status"].clone();
    let (before, after) = (status(previous), status(current));
    if after["type"] == "blocked"
        && (before["type"] != "blocked" || before["callbackId"] != after["callbackId"])
    {
        return true;
    }
    if after["type"] == "failed" && before["type"] != "failed" {
        return true;
    }
    let latest = |state: &Value| {
        state["turns"]
            .as_array()
            .and_then(|turns| turns.last())
            .cloned()
    };
    let (previous_turn, current_turn) = (latest(previous), latest(current));
    if let (Some(previous_turn), Some(current_turn)) = (&previous_turn, &current_turn)
        && previous_turn["status"] == "in_progress"
        && current_turn["id"] != previous_turn["id"]
    {
        return true;
    }
    let Some(current_turn) = current_turn else {
        return false;
    };
    if !matches!(
        current_turn["status"].as_str(),
        Some("completed" | "failed")
    ) {
        return false;
    }
    previous_turn.is_none_or(|previous_turn| {
        previous_turn["id"] != current_turn["id"]
            || previous_turn["status"] != current_turn["status"]
    })
}

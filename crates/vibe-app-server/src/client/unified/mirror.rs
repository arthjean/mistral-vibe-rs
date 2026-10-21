//! One unified session as its client mirrors it, reference
//! `ClientProjection` (`vibe/app_server/events.py`): the state it opened
//! with, every event it published applied in order.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::client::{AgentStatsSnapshot, ProgrammaticUpdate};

/// One session as its client mirrors it: the state it opened with, each
/// published event applied in order.
pub(super) struct Mirror {
    pub(super) state: Value,
    pub(super) last_event: u64,
    /// The accounting the session last published, with its context window:
    /// reference `ClientState.stats`, which `session/statsUpdated` replaces.
    pub(super) accounting: Option<(AgentStatsSnapshot, u64)>,
    /// The events no running turn took, handed to the next one that runs
    /// as watermarks so its reader sees an unbroken sequence.
    pub(super) skipped: Vec<u64>,
}

/// What one notification changed, for whoever reads the session live.
pub(super) enum Published {
    Entry(Value),
    TurnCompleted(Value),
    /// The accounting a `session/statsUpdated` published, whole.
    Stats {
        stats: AgentStatsSnapshot,
        context_window: u64,
    },
    Handoff {
        from: String,
        to: String,
    },
    Nothing,
}

impl Mirror {
    pub(super) fn new(state: Value) -> Self {
        let last_event = state["eventId"].as_u64().unwrap_or(0);
        Self {
            state,
            last_event,
            accounting: None,
            skipped: Vec::new(),
        }
    }

    pub(super) fn history(&self) -> &[Value] {
        self.state["history"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The turn the session reports running.
    pub(super) fn active_turn(&self) -> Option<String> {
        self.state["session"]["status"]["activeTurnId"]
            .as_str()
            .map(ToOwned::to_owned)
    }

    /// The title and preview the session now carries, as a live reader
    /// hears of a rename.
    pub(super) fn renamed(&self) -> ProgrammaticUpdate {
        let session = &self.state["session"];
        let text = |key: &str| session[key].as_str().map(ToOwned::to_owned);
        ProgrammaticUpdate::Renamed {
            title: text("title"),
            preview: text("preview"),
        }
    }

    /// Reference `ClientProjection.consume`. A gap in the event sequence is
    /// taken as it comes: the next snapshot or read restores whatever it
    /// missed.
    pub(super) fn consume(&mut self, method: &str, params: &BTreeMap<String, Value>) -> Published {
        if matches!(method, "session/compacted" | "session/contextCleared") {
            let from = self.state["session"]["id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let state = params.get("state").cloned().unwrap_or(Value::Null);
            let to = state["session"]["id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            *self = Self::new(state);
            return Published::Handoff { from, to };
        }
        if let Some(event_id) = params.get("eventId").and_then(Value::as_u64) {
            if event_id <= self.last_event {
                return Published::Nothing;
            }
            self.last_event = event_id;
        }
        match method {
            "session/snapshot" => {
                if let Some(state) = params.get("state") {
                    self.state = state.clone();
                }
                Published::Nothing
            }
            "session/updated" => {
                if let Some(patch) = params.get("patch").and_then(Value::as_array) {
                    let _ =
                        vibe_harness::patch::apply_json_patch(&mut self.state["session"], patch);
                }
                Published::Nothing
            }
            "history/entryAdded" => {
                let Some(entry) = params.get("entry").cloned() else {
                    return Published::Nothing;
                };
                if !self.state["history"].is_array() {
                    self.state["history"] = json!([]);
                }
                if let Some(history) = self.state["history"].as_array_mut() {
                    history.retain(|existing| existing["id"] != entry["id"]);
                    history.push(entry.clone());
                }
                Published::Entry(entry)
            }
            "history/entryUpdated" => {
                let entry_id = params
                    .get("entryId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(patch) = params.get("patch").and_then(Value::as_array) else {
                    return Published::Nothing;
                };
                let Some(entry) = self.state["history"]
                    .as_array_mut()
                    .and_then(|history| history.iter_mut().find(|entry| entry["id"] == entry_id))
                else {
                    return Published::Nothing;
                };
                if vibe_harness::patch::apply_json_patch(entry, patch).is_err() {
                    return Published::Nothing;
                }
                Published::Entry(entry.clone())
            }
            "turn/started" | "turn/completed" => {
                let Some(turn) = params.get("turn").cloned() else {
                    return Published::Nothing;
                };
                if !self.state["turns"].is_array() {
                    self.state["turns"] = json!([]);
                }
                if let Some(turns) = self.state["turns"].as_array_mut() {
                    match turns
                        .iter_mut()
                        .find(|existing| existing["id"] == turn["id"])
                    {
                        Some(existing) => *existing = turn.clone(),
                        None => turns.push(turn.clone()),
                    }
                }
                if method == "turn/completed" {
                    Published::TurnCompleted(turn)
                } else {
                    Published::Nothing
                }
            }
            "turn/queueUpdated" => {
                if let Some(queue) = params.get("queue") {
                    self.state["turnQueue"] = queue.clone();
                }
                Published::Nothing
            }
            "session/statsUpdated" => {
                let stats: AgentStatsSnapshot = params
                    .get("stats")
                    .and_then(|stats| serde_json::from_value(stats.clone()).ok())
                    .unwrap_or_default();
                let context_window = params
                    .get("contextWindow")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.accounting = Some((stats.clone(), context_window));
                Published::Stats {
                    stats,
                    context_window,
                }
            }
            "session/childSessionUpdated" => {
                if let Some(child) = params.get("childSession") {
                    if !self.state["childSessions"].is_array() {
                        self.state["childSessions"] = json!([]);
                    }
                    if let Some(children) = self.state["childSessions"].as_array_mut() {
                        match children
                            .iter_mut()
                            .find(|existing| existing["id"] == child["id"])
                        {
                            Some(existing) => *existing = child.clone(),
                            None => children.push(child.clone()),
                        }
                        // Reference `ClientState.apply`: by creation, then id.
                        children.sort_by(|left, right| {
                            let key = |child: &Value| {
                                (
                                    child["createdAt"].as_i64().unwrap_or(0),
                                    child["id"].as_str().unwrap_or_default().to_owned(),
                                )
                            };
                            key(left).cmp(&key(right))
                        });
                    }
                }
                Published::Nothing
            }
            _ => Published::Nothing,
        }
    }
}

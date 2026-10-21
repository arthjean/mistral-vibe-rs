//! The projector: each snapshot derived from the one before and the
//! observations of the step between them.

use serde_json::{Value, json};

use super::entries::{
    MessageEntry, belongs_to_action, completed_effect_entry, count_checkpoints, discovery_entry,
    is_open_effect, is_provisional, merge_provisional, message_entry, reasoning_entry,
    running_effect_entry, settle_open_effects, settle_provisional, terminal_compaction_entry,
    upsert,
};
use super::{
    CompletionDelta, HarnessState, HarnessTurn, Observation, SessionStatus, TokenUsage, ToolAction,
    TransitionTurn, TurnStatus, content_preview, first_user_message_preview,
};

/// Derives each snapshot from the one before.
///
/// The live snapshot carries provisional completion content the journal never
/// holds; `durable` is the journaled view, and the watermark counts journaled
/// advances only.
#[derive(Debug, Clone)]
pub struct Projector {
    pub state: HarnessState,
    pub watermark: u64,
}

/// Whether an advance is journaled or only published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    Journaled,
    Provisional,
}

impl Projector {
    #[must_use]
    pub fn new(mut state: HarnessState, watermark: u64) -> Self {
        if state.session.preview.is_empty() {
            state.session.preview = first_user_message_preview(&state.entries);
        }
        Self { state, watermark }
    }

    /// The journaled view: everything but streamed content.
    #[must_use]
    pub fn durable(&self) -> HarnessState {
        let mut durable = self.state.clone();
        durable.entries.retain(|entry| !is_provisional(entry));
        durable
    }

    fn advance(&mut self, kind: Advance) -> Advance {
        if kind == Advance::Journaled {
            self.watermark += 1;
        }
        kind
    }

    /// Reference `apply_action_started`: a tool call's running entry.
    pub fn apply_action_started(&mut self, action: &ToolAction, observed_at: u64) -> Advance {
        let input_entry_id = self.state.active_input_entry_id(&action.turn_id);
        let entry =
            running_effect_entry(&self.state.session.id, action, observed_at, input_entry_id);
        upsert(&mut self.state.entries, entry);
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }

    /// Reference `SessionProjector.apply`: the step's observations in order,
    /// then the session in the state `turn` leaves it.
    pub fn apply(
        &mut self,
        input_id: u64,
        observations: &[Observation],
        turn: Option<&TransitionTurn>,
        observed_at: u64,
    ) -> Advance {
        let state = &mut self.state;
        if state.session.preview.is_empty() {
            state.session.preview = first_user_message_preview(&state.entries);
        }
        for observation in observations {
            state.observe(input_id, observation, observed_at);
        }
        state.session.status = match turn {
            Some(TransitionTurn::Running(turn_id)) => SessionStatus::Running {
                active_turn_id: turn_id.clone(),
            },
            Some(TransitionTurn::Failed(message)) => SessionStatus::Failed {
                message: message.clone(),
            },
            Some(TransitionTurn::Settled) | None => SessionStatus::Idle,
        };
        if matches!(
            state.session.status,
            SessionStatus::Idle | SessionStatus::Failed { .. }
        ) {
            // A provider retry only lives while a completion is in flight.
            state.retrying = None;
        }
        state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }

    /// Reference `append_provisional_completion_content`.
    pub fn apply_completion_delta(&mut self, delta: &CompletionDelta, observed_at: u64) -> Advance {
        let session_id = self.state.session.id.clone();
        let input_entry_id = self.state.active_input_entry_id(&delta.turn_id);
        let entries = &mut self.state.entries;
        if delta.restart {
            for entry in entries.iter_mut() {
                if is_provisional(entry) && belongs_to_action(entry, &delta.action_id) {
                    if entry["type"] == "reasoning" {
                        entry["text"] = json!("");
                    } else {
                        entry["content"] = json!([{"type": "text", "text": ""}]);
                    }
                    entry["updatedAt"] = json!(observed_at);
                }
            }
        }
        if !delta.reasoning.is_empty() {
            let id = format!("reasoning-{}-0", delta.action_id);
            merge_provisional(entries, &id, observed_at, |entry| {
                let text = entry["text"].as_str().unwrap_or_default().to_owned();
                entry["text"] = json!(format!("{text}{}", delta.reasoning));
            })
            .unwrap_or_else(|| {
                entries.push(json!({
                    "type": "reasoning",
                    "id": id,
                    "sessionId": session_id,
                    "turnId": delta.turn_id,
                    "createdAt": observed_at,
                    "updatedAt": observed_at,
                    "generationStatus": "in_progress",
                    "outcome": null,
                    "relatedEntryId": null,
                    "inputEntryId": input_entry_id,
                    "text": delta.reasoning,
                    "summary": [],
                }));
            });
        }
        if !delta.text.is_empty() {
            let id = format!("assistant-{}", delta.action_id);
            merge_provisional(entries, &id, observed_at, |entry| {
                let mut blocks = entry["content"].as_array().cloned().unwrap_or_default();
                if blocks.is_empty() || !blocks[0].is_object() {
                    blocks.insert(0, json!({"type": "text", "text": ""}));
                }
                let text = blocks[0]["text"].as_str().unwrap_or_default().to_owned();
                blocks[0]["text"] = json!(format!("{text}{}", delta.text));
                entry["content"] = Value::Array(blocks);
            })
            .unwrap_or_else(|| {
                entries.push(json!({
                    "type": "message",
                    "id": id,
                    "sessionId": session_id,
                    "turnId": delta.turn_id,
                    "createdAt": observed_at,
                    "updatedAt": observed_at,
                    "generationStatus": "in_progress",
                    "outcome": null,
                    "relatedEntryId": null,
                    "inputEntryId": input_entry_id,
                    "role": "assistant",
                    "content": [{"type": "text", "text": delta.text}],
                    "source": "harness",
                }));
            });
        }
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Provisional)
    }

    /// Reference `apply_model_change`.
    pub fn apply_model_change(&mut self, model: &str, observed_at: u64) -> Advance {
        let sequence = count_checkpoints(&self.state.entries, "model_change") + 1;
        self.state.entries.push(json!({
            "type": "checkpoint",
            "id": format!("checkpoint-model-change-{sequence}"),
            "sessionId": self.state.session.id,
            "turnId": null,
            "createdAt": observed_at,
            "updatedAt": observed_at,
            "generationStatus": "completed",
            "relatedEntryId": null,
            "inputEntryId": null,
            "kind": "model_change",
            "message": format!("Model changed to {model}"),
            "details": {"model": model},
        }));
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }

    /// Reference `apply_agent_change`.
    pub fn apply_agent_change(
        &mut self,
        agent: &str,
        turn_id: Option<&str>,
        observed_at: u64,
    ) -> Advance {
        let sequence = count_checkpoints(&self.state.entries, "agent_change") + 1;
        self.state.entries.push(json!({
            "type": "checkpoint",
            "id": format!("checkpoint-agent-change-{sequence}"),
            "sessionId": self.state.session.id,
            "turnId": turn_id,
            "createdAt": observed_at,
            "updatedAt": observed_at,
            "generationStatus": "completed",
            "relatedEntryId": null,
            "kind": "agent_change",
            "message": format!("Agent changed to {agent}"),
            "details": {"agent": agent},
        }));
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }

    /// Reference `append_public_history_entries`.
    pub fn append_entries(&mut self, entries: Vec<Value>, observed_at: u64) -> Advance {
        self.state.entries.extend(entries);
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }

    /// Adds `entry`, or replaces the entry it shares an identifier with,
    /// without journaling it: a step still in flight.
    pub fn upsert_provisional(&mut self, entry: Value, observed_at: u64) -> Advance {
        upsert(&mut self.state.entries, entry);
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Provisional)
    }

    /// Reference `rename_session`.
    pub fn rename(&mut self, title: &str, source: &str, observed_at: u64) -> Advance {
        self.state.session.title = Some(title.to_owned());
        source.clone_into(&mut self.state.session.title_source);
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }

    /// A snapshot published without a step behind it, which only stamps the
    /// session: reference `_advance` over an unchanged state with a new
    /// observation time.
    pub fn touch(&mut self, observed_at: u64) -> Advance {
        self.state.session.updated_at = observed_at;
        self.advance(Advance::Journaled)
    }
}

/// Reference `settle_stalled_projection`: a running turn nothing owns any more
/// reads as interrupted.
pub fn settle_stalled(state: &mut HarnessState, observed_at: u64) {
    let Some(turn_id) = state.session.status.active_turn_id().map(str::to_owned) else {
        return;
    };
    let reason = "Interrupted by process restart";
    settle_open_effects(&mut state.entries, &turn_id, observed_at, true, reason);
    state
        .settle_turn(&turn_id, TurnStatus::Interrupted, observed_at)
        .reason = Some(reason.to_owned());
    state.session.status = SessionStatus::Idle;
}

impl HarnessState {
    /// One observation of the step `input_id` folded into the snapshot.
    fn observe(&mut self, input_id: u64, observation: &Observation, observed_at: u64) {
        match observation {
            Observation::TurnStarted {
                turn_id,
                content,
                injected,
                queue_item_id,
                client_message_id,
                user_display_content,
            } => {
                // An injected message reaches the model only: no entry
                // shows it, and the turn answers none.
                let input_entry_id = if *injected {
                    None
                } else {
                    if self.session.preview.is_empty() {
                        self.session.preview = content_preview(content);
                    }
                    let entry = MessageEntry {
                        session_id: &self.session.id,
                        turn_id: Some(turn_id),
                        entry_id: &format!("user-{turn_id}-{input_id}"),
                        role: "user",
                        content,
                        observed_at,
                        source: "turn_start",
                        input_entry_id: None,
                        client_message_id: client_message_id.as_deref(),
                        user_display_content: user_display_content.as_ref(),
                    };
                    Some(turn_start_entry(&mut self.entries, turn_id, &entry))
                };
                self.latest_turn = Some(HarnessTurn {
                    id: turn_id.clone(),
                    session_id: self.session.id.clone(),
                    status: TurnStatus::InProgress,
                    queue_item_id: queue_item_id.clone(),
                    input_entry_id,
                    started_at: observed_at,
                    completed_at: None,
                    error: None,
                    reason: None,
                });
            }
            Observation::TurnSteeringReceived {
                turn_id,
                content,
                client_message_id,
                user_display_content,
            } => {
                let entry = message_entry(&MessageEntry {
                    session_id: &self.session.id,
                    turn_id: Some(turn_id),
                    entry_id: &format!("user-{turn_id}-{input_id}"),
                    role: "user",
                    content,
                    observed_at,
                    source: "turn_steer",
                    input_entry_id: None,
                    client_message_id: client_message_id.as_deref(),
                    user_display_content: user_display_content.as_ref(),
                });
                self.entries.push(entry);
            }
            Observation::TurnSteered {
                turn_id,
                client_message_id,
            } => {
                let active = self.active_input_entry_id(turn_id);
                if let Some(input_entry_id) = steered_input_entry_id(
                    &self.entries,
                    turn_id,
                    active.as_deref(),
                    client_message_id.as_deref(),
                ) && let Some(turn) = self.latest_turn.as_mut()
                {
                    turn.input_entry_id = Some(input_entry_id);
                }
            }
            Observation::AssistantMessageCommitted {
                turn_id,
                action_id,
                reasoning,
                content,
                usage,
            } => {
                let input_entry_id = self.active_input_entry_id(turn_id);
                for (index, part) in reasoning.iter().enumerate() {
                    if let Some(entry) = reasoning_entry(
                        &self.session.id,
                        turn_id,
                        &format!("reasoning-{action_id}-{index}"),
                        part,
                        observed_at,
                        input_entry_id.as_deref(),
                    ) {
                        upsert(&mut self.entries, entry);
                    }
                }
                if !content.is_empty() {
                    let entry = message_entry(&MessageEntry {
                        session_id: &self.session.id,
                        turn_id: Some(turn_id),
                        entry_id: &format!("assistant-{action_id}"),
                        role: "assistant",
                        content,
                        observed_at,
                        source: "harness",
                        input_entry_id: input_entry_id.as_deref(),
                        client_message_id: None,
                        user_display_content: None,
                    });
                    upsert(&mut self.entries, entry);
                }
                for entry in &mut self.entries {
                    if is_provisional(entry) && belongs_to_action(entry, action_id) {
                        settle_provisional(entry, observed_at, "failure");
                    }
                }
                if let Some(usage) = usage {
                    self.add_token_usage(*usage);
                    self.session.context_usage = Some(*usage);
                }
            }
            Observation::ToolResultCommitted {
                turn_id,
                action_id,
                result,
            } => {
                let input_entry_id = self.active_input_entry_id(turn_id);
                let entry = completed_effect_entry(
                    &self.session.id,
                    &self.entries,
                    turn_id,
                    action_id,
                    result,
                    observed_at,
                    input_entry_id.as_deref(),
                );
                upsert(&mut self.entries, entry);
            }
            Observation::ToolDiscoveryFinished {
                turn_id,
                call_id,
                mode,
                tool_count,
                connector_notice_count,
            } => {
                let input_entry_id = self.active_input_entry_id(turn_id);
                let entry = discovery_entry(
                    &self.session.id,
                    turn_id,
                    call_id,
                    mode,
                    json!({
                        "mode": mode,
                        "toolCount": tool_count,
                        "connectorNoticeCount": connector_notice_count,
                    }),
                    observed_at,
                    input_entry_id.as_deref(),
                );
                upsert(&mut self.entries, entry);
            }
            Observation::ContextCompacted {
                turn_id,
                compaction_id,
                trigger,
                attempt,
                summary_len,
                usage,
            } => {
                self.settle_compaction(
                    compaction_id,
                    turn_id.as_deref(),
                    observed_at,
                    "Context compacted",
                    json!({"trigger": trigger, "attempt": attempt, "summaryLength": summary_len}),
                );
                if let Some(usage) = usage {
                    self.add_token_usage(*usage);
                }
                self.session.context_usage = Some(TokenUsage::default());
            }
            Observation::ContextCompactionFailed {
                turn_id,
                compaction_id,
                trigger,
                attempt,
                code,
                reason,
            } => {
                self.settle_compaction(
                    compaction_id,
                    turn_id.as_deref(),
                    observed_at,
                    "Context compaction failed",
                    json!({
                        "trigger": trigger,
                        "attempt": attempt,
                        "error": {"code": code},
                        "reason": reason,
                    }),
                );
            }
            Observation::TurnCompleted { turn_id } => {
                self.entries.retain(|entry| !is_open_effect(entry, turn_id));
                for entry in &mut self.entries {
                    if is_provisional(entry) && entry["turnId"] == json!(turn_id) {
                        settle_provisional(entry, observed_at, "failure");
                    }
                }
                self.settle_turn(turn_id, TurnStatus::Completed, observed_at);
            }
            Observation::TurnInterrupted { turn_id, reason } => {
                let cause = reason.as_deref().unwrap_or("Turn interrupted");
                settle_open_effects(&mut self.entries, turn_id, observed_at, true, cause);
                self.settle_turn(turn_id, TurnStatus::Interrupted, observed_at)
                    .reason
                    .clone_from(reason);
            }
            Observation::TurnFailed { turn_id, error } => {
                settle_open_effects(
                    &mut self.entries,
                    turn_id,
                    observed_at,
                    false,
                    &error.message,
                );
                self.settle_turn(turn_id, TurnStatus::Failed, observed_at)
                    .error = Some(error.clone());
            }
        }
    }

    /// The input entry of the turn `turn_id`, while it is the one running.
    fn active_input_entry_id(&self, turn_id: &str) -> Option<String> {
        self.latest_turn
            .as_ref()
            .filter(|turn| turn.in_progress_for(turn_id))
            .and_then(|turn| turn.input_entry_id.clone())
    }

    fn add_token_usage(&mut self, usage: TokenUsage) {
        self.session.token_usage = Some(self.session.token_usage.unwrap_or_default().added(usage));
    }

    /// The compaction `compaction_id` settled with `message`.
    fn settle_compaction(
        &mut self,
        compaction_id: &str,
        turn_id: Option<&str>,
        observed_at: u64,
        message: &str,
        details: Value,
    ) {
        let input_entry_id = turn_id.and_then(|turn| self.active_input_entry_id(turn));
        let entry = terminal_compaction_entry(
            &self.session.id,
            &self.entries,
            compaction_id,
            turn_id,
            observed_at,
            input_entry_id.as_deref(),
            message,
            details,
        );
        upsert(&mut self.entries, entry);
    }

    /// The turn `turn_id` settled as `status`, keeping what the running
    /// turn knew of itself; the caller records why it stopped.
    fn settle_turn(
        &mut self,
        turn_id: &str,
        status: TurnStatus,
        observed_at: u64,
    ) -> &mut HarnessTurn {
        let active = self
            .latest_turn
            .as_ref()
            .filter(|turn| turn.in_progress_for(turn_id));
        let turn = HarnessTurn {
            id: turn_id.to_owned(),
            session_id: self.session.id.clone(),
            status,
            queue_item_id: active.and_then(|turn| turn.queue_item_id.clone()),
            input_entry_id: active.and_then(|turn| turn.input_entry_id.clone()),
            started_at: active.map_or(observed_at, |turn| turn.started_at),
            completed_at: Some(observed_at),
            error: None,
            reason: None,
        };
        self.latest_turn.insert(turn)
    }
}

/// The ID of the user entry that starts the turn `turn_id`: the one
/// `entries` already holds, else `entry`, added to them.
fn turn_start_entry(entries: &mut Vec<Value>, turn_id: &str, entry: &MessageEntry<'_>) -> String {
    let existing = entries.iter().find(|candidate| {
        candidate["type"] == "message"
            && candidate["role"] == "user"
            && candidate["turnId"] == json!(turn_id)
            && candidate["source"] == "turn_start"
    });
    if let Some(existing) = existing {
        return existing["id"].as_str().unwrap_or_default().to_owned();
    }
    let entry = message_entry(entry);
    let id = entry["id"].as_str().unwrap_or_default().to_owned();
    entries.push(entry);
    id
}

/// The steering entry a steered turn now answers: the one the client named,
/// else the first after the turn's current input.
fn steered_input_entry_id(
    entries: &[Value],
    turn_id: &str,
    active: Option<&str>,
    client_message_id: Option<&str>,
) -> Option<String> {
    let is_steering = |entry: &Value| {
        entry["type"] == "message"
            && entry["role"] == "user"
            && entry["source"] == "turn_steer"
            && entry["turnId"] == json!(turn_id)
    };
    if let Some(id) = client_message_id
        && entries
            .iter()
            .any(|entry| entry["id"] == json!(id) && is_steering(entry))
    {
        return Some(id.to_owned());
    }
    let mut after = active.is_none();
    for entry in entries {
        if active.is_some() && entry["id"].as_str() == active {
            after = true;
            continue;
        }
        if after && is_steering(entry) {
            return entry["id"].as_str().map(str::to_owned);
        }
    }
    None
}

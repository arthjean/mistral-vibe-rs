//! What a session keeps between turns, and the steps that open a turn,
//! steer it and rewind the conversation.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};
use vibe_core::clock::now_millis;
use vibe_core::session_id::random_hex;

use super::calls::compact;
use super::conversation::{ConversationMessage, client_message_position, tagged};
use super::{CallbackEvent, Publication, TurnRequest};
use crate::gate::CallbackBook;
use crate::state::{Advance, HarnessState, Observation, Projector, TransitionTurn};
use crate::tools::vibe::VibeTools;

/// Everything a session keeps between turns.
#[derive(Debug)]
pub struct SessionCore {
    pub projector: Projector,
    pub conversation: Vec<ConversationMessage>,
    pub vibe: VibeTools,
    /// The last input the core took; every turn start, model answer and tool
    /// result takes one.
    pub inputs: u64,
    /// What the session runs on, as its turns pinned it: `model` and
    /// `reasoningEffort`.
    pub pins: BTreeMap<String, String>,
    /// The callbacks the session waits on, which are published beside its
    /// state and never journaled.
    pub callbacks: CallbackBook,
    /// The background processes the session's programs started.
    pub processes: Arc<crate::processes::ProcessManager>,
    /// The subagents the session's programs started.
    pub subagents: Arc<crate::subagents::SubagentManager>,
    /// The prompts the session scheduled for itself.
    pub loops: Arc<crate::loops::ScheduledLoops>,
    /// Steering accepted for the running turn that no model call carried
    /// yet: each message's content and client message ID.
    pub pending_steer: Vec<(Vec<Value>, Option<String>)>,
    /// Reference `PluginLockV1` left empty: a session imported from a legacy
    /// transcript binds no plugins, now or on any later resume.
    pub without_plugins: bool,
}

impl SessionCore {
    #[must_use]
    pub fn new(state: HarnessState, scratchpad: std::path::PathBuf) -> Self {
        Self {
            projector: Projector::new(state, 0),
            conversation: Vec::new(),
            vibe: VibeTools::new(scratchpad),
            inputs: 0,
            pins: BTreeMap::new(),
            callbacks: CallbackBook::default(),
            processes: crate::processes::ProcessManager::new(),
            subagents: crate::subagents::SubagentManager::new(),
            loops: Arc::new(crate::loops::ScheduledLoops::new()),
            pending_steer: Vec::new(),
            without_plugins: false,
        }
    }

    /// Reference `UserMessageMode::Steer`: the message joins the history
    /// at once and the model reads it on its next call.
    pub fn steer(&mut self, turn_id: &str, request: &TurnRequest) -> Advance {
        let input = self.next_input();
        let advance = self.projector.apply(
            input,
            &[Observation::TurnSteeringReceived {
                turn_id: turn_id.to_owned(),
                content: request.content.clone(),
                client_message_id: request.client_message_id.clone(),
                user_display_content: request.user_display_content.clone(),
            }],
            Some(&TransitionTurn::Running(turn_id.to_owned())),
            now_millis(),
        );
        self.pending_steer
            .push((request.content.clone(), request.client_message_id.clone()));
        advance
    }

    /// Hands the pending steering to the model, the turn now answering the
    /// last message; nothing when none is pending.
    pub(super) fn deliver_steering(&mut self, turn_id: &str) -> Option<Advance> {
        if self.pending_steer.is_empty() {
            return None;
        }
        let steering = std::mem::take(&mut self.pending_steer);
        let observations: Vec<Observation> = steering
            .iter()
            .map(|(_, client_message_id)| Observation::TurnSteered {
                turn_id: turn_id.to_owned(),
                client_message_id: client_message_id.clone(),
            })
            .collect();
        let input = self.inputs;
        let advance = self.projector.apply(
            input,
            &observations,
            Some(&TransitionTurn::Running(turn_id.to_owned())),
            now_millis(),
        );
        for (content, client_message_id) in steering {
            self.conversation.push(ConversationMessage::User {
                content: tagged(&content, client_message_id.as_deref()),
                injected: false,
            });
        }
        Some(advance)
    }

    /// Reference `finish_turn`: steering the model never read stays in
    /// its context, as the history already shows it.
    pub(super) fn keep_pending_steering(&mut self) {
        for (content, client_message_id) in std::mem::take(&mut self.pending_steer) {
            self.conversation.push(ConversationMessage::User {
                content: tagged(&content, client_message_id.as_deref()),
                injected: false,
            });
        }
    }

    /// Reference `_drain_runtime_notifications`: what ended in the
    /// background since the model last read reaches it as one user message.
    pub(super) fn deliver_notifications(&mut self) {
        let mut notifications: Vec<String> = self
            .processes
            .take_notifications()
            .iter()
            .map(|notification| format!("Runtime notification:\n{}", compact(notification)))
            .collect();
        notifications.extend(self.subagents.take_notifications());
        if notifications.is_empty() {
            return;
        }
        let text = notifications.join("\n\n");
        self.conversation.push(ConversationMessage::User {
            content: vec![json!({"type": "text", "text": text})],
            injected: true,
        });
    }

    /// Takes the process list into the public state.
    pub fn refresh_processes(&mut self) {
        self.projector.state.background_processes = self.processes.public();
    }

    /// The session's state as published now, with `event` before it.
    #[must_use]
    pub fn publication(&self, advance: Advance, event: Option<CallbackEvent>) -> Publication {
        Publication {
            state: self.projector.state.clone(),
            advance,
            callbacks: self.callbacks.open.clone(),
            event,
        }
    }

    /// The input entry of the latest turn: the message its model calls
    /// answer.
    pub(super) fn latest_input_entry_id(&self) -> Option<String> {
        self.projector
            .state
            .latest_turn
            .as_ref()
            .and_then(|turn| turn.input_entry_id.clone())
    }

    /// The input entry of the turn `turn_id`, when it is the latest one.
    pub(super) fn input_entry_of(&self, turn_id: &str) -> Option<String> {
        self.projector
            .state
            .latest_turn
            .as_ref()
            .filter(|turn| turn.id == turn_id)
            .and_then(|turn| turn.input_entry_id.clone())
    }

    pub(super) fn next_input(&mut self) -> u64 {
        self.inputs += 1;
        self.inputs
    }

    /// Commits a new turn: its user entry, and the session running it.
    pub fn begin_turn(&mut self, request: &TurnRequest) -> (String, HarnessState) {
        self.begin_turn_as(request, fresh_turn_id())
    }

    /// Reference `_start_queued_turn`: a queued turn's context entries reach
    /// the model ahead of its user entry, and the turn names the item. Its
    /// message is never injected.
    pub fn begin_queued_turn(
        &mut self,
        request: &TurnRequest,
        queue_item_id: &str,
        context: &[Vec<Value>],
    ) -> (String, HarnessState) {
        for content in context {
            self.conversation.push(ConversationMessage::User {
                content: content.clone(),
                injected: true,
            });
        }
        self.start_turn(request, fresh_turn_id(), false, Some(queue_item_id))
    }

    /// [`Self::begin_turn`] under the turn ID `turn_id`.
    pub fn begin_turn_as(
        &mut self,
        request: &TurnRequest,
        turn_id: String,
    ) -> (String, HarnessState) {
        self.start_turn(request, turn_id, request.injected, None)
    }

    fn start_turn(
        &mut self,
        request: &TurnRequest,
        turn_id: String,
        injected: bool,
        queue_item_id: Option<&str>,
    ) -> (String, HarnessState) {
        let input = self.next_input();
        self.projector.apply(
            input,
            &[Observation::TurnStarted {
                turn_id: turn_id.clone(),
                content: request.content.clone(),
                injected,
                queue_item_id: queue_item_id.map(ToOwned::to_owned),
                client_message_id: request.client_message_id.clone(),
                user_display_content: request.user_display_content.clone(),
            }],
            Some(&TransitionTurn::Running(turn_id.clone())),
            now_millis(),
        );
        self.conversation.push(ConversationMessage::User {
            content: tagged(&request.content, request.client_message_id.as_deref()),
            injected: false,
        });
        (turn_id, self.projector.state.clone())
    }

    /// Reference `rewind` on the harness host: the conversation and the
    /// history before the user message `entry_id`, closed by a `rewind`
    /// checkpoint. Only a message a client named can be found.
    ///
    /// # Errors
    ///
    /// No user message carries that identifier.
    pub fn rewind(&mut self, entry_id: &str, now: u64) -> Result<Advance, String> {
        let position = client_message_position(&self.conversation, entry_id)
            .ok_or_else(|| format!("Cannot find user entry: {entry_id}"))?;
        self.conversation.truncate(position);
        let session_id = self.projector.state.session.id.clone();
        let entries = &mut self.projector.state.entries;
        if let Some(index) = entries.iter().position(|entry| entry["id"] == entry_id) {
            entries.truncate(index);
        }
        entries.push(json!({
            "type": "checkpoint",
            "id": format!("checkpoint-rewind-{entry_id}"),
            "sessionId": session_id,
            "turnId": null,
            "createdAt": now,
            "updatedAt": now,
            "generationStatus": "completed",
            "relatedEntryId": null,
            "inputEntryId": null,
            "kind": "rewind",
            "message": "Conversation rewound",
            "details": {"entryId": entry_id, "inplace": true, "restoreFiles": false},
        }));
        self.projector.state.latest_turn = None;
        self.vibe.todos.clear();
        self.next_input();
        Ok(self.projector.touch(now))
    }
}

fn fresh_turn_id() -> String {
    format!("turn-{}", random_hex::<16>())
}

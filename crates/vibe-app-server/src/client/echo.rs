//! A driver that answers every turn with one fixed reply, for tests and for
//! launches that exercise the session surface without a backend.

use std::path::PathBuf;

use vibe_core::events::{LifecycleState, ModelMessage};
use vibe_core::storage::SessionStore;

use super::{
    DriverError, DriverFuture, PublicTurnStopReason, TurnDriver, TurnOutcome, TurnReservation,
};

#[derive(Debug, Clone)]
pub struct EchoTurnDriver {
    response: String,
    session_root: Option<PathBuf>,
}

impl EchoTurnDriver {
    #[must_use]
    pub fn new(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
            session_root: None,
        }
    }

    #[must_use]
    pub fn with_session_root(mut self, session_root: impl Into<PathBuf>) -> Self {
        self.session_root = Some(session_root.into());
        self
    }
}

impl TurnDriver for EchoTurnDriver {
    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
        Box::pin(async move {
            let mut reducer = vibe_core::events::ProjectionReducer::for_turn(
                &reservation.session_id,
                &reservation.turn_id,
            );
            let mut events = Vec::new();
            for (event_id, event) in [
                vibe_core::events::EngineEvent::UserMessage {
                    attachments: Vec::new(),
                    message_id: None,
                    content: reservation.prompt.clone(),
                    user_display_content: None,
                },
                vibe_core::events::EngineEvent::ModelText {
                    message_id: None,
                    text: self.response.clone(),
                },
                vibe_core::events::EngineEvent::Lifecycle {
                    state: LifecycleState::Completed,
                    message: Some("Turn completed".to_owned()),
                },
            ]
            .into_iter()
            .enumerate()
            {
                let envelope = vibe_core::events::EventEnvelope {
                    session_id: reservation.session_id.clone(),
                    turn_id: Some(reservation.turn_id.clone()),
                    emitted_at: crate::host::now_millis(),
                    working_directory: None,
                    event_id: u64::try_from(event_id).unwrap_or(0).saturating_add(1),
                    event,
                };
                reducer
                    .apply(&envelope)
                    .map_err(vibe_core::engine::EngineError::Projection)
                    .map_err(DriverError::Engine)?;
                events.push(envelope);
            }
            let outcome = TurnOutcome {
                session_id: reservation.session_id.clone(),
                events,
                snapshot: reducer.state().clone(),
                messages: vec![
                    ModelMessage::user(reservation.prompt.clone()),
                    ModelMessage::Assistant {
                        message_id: None,
                        reasoning_message_id: None,
                        content: self.response.clone(),
                        reasoning: None,
                        reasoning_payloads: Vec::new(),
                        tool_calls: Vec::new(),
                        keeps_empty_content: false,
                    },
                ],
                usage: vibe_core::provider::Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_tokens: 0,
                },
                context_tokens: 2,
                price_micros: 0,
                steps: 1,
                checkpoints: 1,
                stop_reason: PublicTurnStopReason::Complete,
            };
            if let Some(session_root) = &self.session_root {
                let store = SessionStore::new(session_root);
                let timestamp = crate::host::now_millis();
                let mut metadata = match store.open(&reservation.session_id) {
                    Ok(hydrated) => hydrated.metadata,
                    Err(vibe_core::storage::StorageError::SessionNotFound(_)) => store
                        .create(
                            &reservation.session_id,
                            &reservation.working_directory,
                            None,
                            timestamp,
                        )
                        .map_err(DriverError::Storage)?,
                    Err(error) => return Err(DriverError::Storage(error)),
                };
                for message in &outcome.messages {
                    store
                        .append_message(&mut metadata, message, timestamp)
                        .map_err(DriverError::Storage)?;
                }
            }
            Ok(outcome)
        })
    }

    fn steer(
        &self,
        _session_id: &str,
        _turn_id: &str,
        _content: &str,
        _inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        Ok(())
    }

    fn inject_context(
        &self,
        _session_id: &str,
        _content: &str,
        _as_message: bool,
        _inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        Ok(())
    }

    fn resolve_callback(
        &self,
        _session_id: &str,
        _turn_id: &str,
        _callback_id: &str,
        _accepted: bool,
        _value: Option<&str>,
    ) -> Result<(), DriverError> {
        Ok(())
    }
}

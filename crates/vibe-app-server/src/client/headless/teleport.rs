//! Reference `_teleport` (`vibe/cli/programmatic.py`), as a program launches
//! one.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

use super::HeadlessService;
use crate::client::{
    ClientError, LiveNotificationListener, ProgrammaticTeleportEvent, PublicNotification,
    TurnDriver,
};

impl<D> HeadlessService<D>
where
    D: TurnDriver,
{
    /// Reference `_teleport` (`vibe/cli/programmatic.py`): the picker opens
    /// for Teleport, which the server gates, a repository with no linked
    /// project is refused, and the run's events are read as they arrive, every
    /// push question answered with `approve_push`, until the run completes or
    /// fails.
    pub async fn teleport(
        &mut self,
        session_id: &str,
        prompt: &str,
        approve_push: bool,
    ) -> Result<Vec<ProgrammaticTeleportEvent>, ClientError> {
        let opened = self
            .public_call_async(
                "vibeCode/projects/open",
                json!({
                    "sessionId": session_id,
                    "purpose": "teleport",
                    "prompt": (!prompt.is_empty()).then_some(prompt),
                }),
            )
            .await?;
        let picker_id = opened
            .result
            .get("pickerId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ClientError::InvalidResponse("project picker omitted pickerId".to_owned())
            })?;
        let project_id = opened
            .result
            .get("resolvedProjectId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ClientError::InvalidResponse(
                    "no Vibe Code project is linked to this working directory".to_owned(),
                )
            })?;
        let operation_id = vibe_core::session_id::uuid_v4();
        let mut receiver = self
            .start_teleport(
                session_id,
                picker_id,
                project_id,
                &operation_id,
                (!prompt.is_empty()).then_some(prompt),
            )
            .await?;
        let mut events = Vec::new();
        while let Some(event) = receiver.recv().await {
            let event = event.map_err(ClientError::Json)?;
            let terminal = matches!(
                event,
                ProgrammaticTeleportEvent::Complete { .. }
                    | ProgrammaticTeleportEvent::Failed { .. }
            );
            let push = matches!(event, ProgrammaticTeleportEvent::PushRequired { .. });
            events.push(event);
            if push {
                self.public_call_async(
                    "vibeCode/teleport/push/respond",
                    json!({
                        "sessionId": session_id,
                        "operationId": operation_id,
                        "approved": approve_push,
                    }),
                )
                .await?;
            }
            if terminal {
                break;
            }
        }
        Ok(events)
    }

    /// Reference `VibeCodeResource.teleport`: starts the run `operation_id`
    /// names and answers the channel its events arrive on, the server
    /// answering the start before it publishes the first one. An event of
    /// this run that does not read as one arrives as its decoding error, so
    /// the caller ends the run with the reason rather than waiting on a
    /// terminal event it already missed.
    pub async fn start_teleport(
        &mut self,
        session_id: &str,
        picker_id: &str,
        project_id: &str,
        operation_id: &str,
        prompt: Option<&str>,
    ) -> Result<TeleportEvents, ClientError> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let wanted = operation_id.to_owned();
        let listener: LiveNotificationListener =
            Arc::new(move |notification: PublicNotification| {
                if notification.method != "vibeCode/teleport/event" {
                    return;
                }
                if let Some(event) = notification
                    .params
                    .get("event")
                    .filter(|event| event["operationId"] == wanted.as_str())
                {
                    let _ = sender.send(ProgrammaticTeleportEvent::deserialize(event));
                }
            });
        self.begin_public_call(
            "vibeCode/teleport/start",
            json!({
                "sessionId": session_id,
                "pickerId": picker_id,
                "projectId": project_id,
                "operationId": operation_id,
                "prompt": prompt,
            }),
        )?
        .with_listener(listener)
        .complete()
        .await?;
        Ok(receiver)
    }
}

/// The events of one Teleport run, in the order the server publishes them.
pub type TeleportEvents = UnboundedReceiver<Result<ProgrammaticTeleportEvent, serde_json::Error>>;
